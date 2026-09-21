//! HTTP/2 transport for Cursor's Connect-RPC agent stream.
//!
//! Cursor rejects HTTP/1.1 outright (464), and the stream is bidirectional:
//! the client keeps writing heartbeats, blob answers, and exec results after
//! the response has started. That rules out a request/response HTTP client, so
//! this owns the h2 stream halves directly.
//!
//! Headers and endpoint copied from sayknow-cli
//! `packages/ai/src/providers/cursor.ts:388-402`, 2026-09-19.

use std::future::poll_fn;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use h2::client::SendRequest;
use h2::{RecvStream, SendStream};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio::time::{timeout, Instant};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use super::frame::{self, Deframer};
use super::pb;

pub const CURSOR_HOST: &str = "api2.cursor.sh";
pub const CURSOR_RUN_PATH: &str = "/agent.v1.AgentService/Run";
pub const CURSOR_GET_USABLE_MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";

/// The backend gates features and minimum versions on this value, so it must
/// match what sayknow-cli sends
/// (`packages/ai/src/providers/cursor/client-version.ts:10`).
pub const CURSOR_CLIENT_VERSION: &str = "cli-2026.02.13-41ac335";

/// No server message for this long means the turn is wedged. Failing loudly
/// beats a spinner that never resolves.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum TransportError {
    /// Credentials are the problem; the UI must prompt a re-login.
    Auth(String),
    Timeout,
    Cancelled,
    Other(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(m) => write!(f, "auth: {m}"),
            Self::Timeout => write!(f, "Cursor stream timed out after {}s", IDLE_TIMEOUT.as_secs()),
            Self::Cancelled => write!(f, "Request was cancelled"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl TransportError {
    pub fn other(e: impl std::fmt::Display) -> Self {
        Self::Other(e.to_string())
    }
}

fn tls_config() -> Arc<ClientConfig> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// An opened HTTP/2 connection to Cursor with ALPN negotiated to `h2`.
pub struct Connection {
    send_request: SendRequest<Bytes>,
}

impl Connection {
    pub async fn connect(host: &str) -> Result<Self, TransportError> {
        let tcp = TcpStream::connect((host, 443))
            .await
            .map_err(TransportError::other)?;
        tcp.set_nodelay(true).ok();

        let mut config = (*tls_config()).clone();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(TransportError::other)?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(TransportError::other)?;

        let (send_request, connection) = h2::client::handshake(tls)
            .await
            .map_err(TransportError::other)?;

        // The connection future drives the socket; dropping it closes the
        // stream, so it lives as long as the request task.
        tokio::spawn(async move {
            let _ = connection.await;
        });

        Ok(Self { send_request })
    }

    /// Open one stream.
    ///
    /// The response is deliberately NOT awaited here: Cursor answers only
    /// after it has seen the request body, so waiting for headers before
    /// writing the first frame deadlocks. The caller writes, then awaits
    /// [`receive`].
    pub async fn open(
        &mut self,
        path: &str,
        access_token: &str,
        content_type: &str,
        end_of_stream: bool,
    ) -> Result<(h2::client::ResponseFuture, SendStream<Bytes>), TransportError> {
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("https://{CURSOR_HOST}{path}"))
            .header("content-type", content_type)
            .header("connect-protocol-version", "1")
            .header("te", "trailers")
            .header("authorization", format!("Bearer {access_token}"))
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-version", CURSOR_CLIENT_VERSION)
            .header("x-cursor-client-type", "cli")
            .header("x-request-id", uuid::Uuid::new_v4().to_string())
            .body(())
            .map_err(TransportError::other)?;

        poll_fn(|cx| self.send_request.poll_ready(cx))
            .await
            .map_err(TransportError::other)?;

        self.send_request
            .send_request(request, end_of_stream)
            .map_err(TransportError::other)
    }
}

/// Await the response headers and check the status.
///
/// A bad token is 401 on the unary path, but 200-with-an-error-frame on the
/// streaming path, so a success here does not yet mean the call worked.
pub async fn receive(response: h2::client::ResponseFuture) -> Result<RecvStream, TransportError> {
    let response = response.await.map_err(|e| {
        if let Some(reason) = e.reason() {
            TransportError::Other(format!("HTTP/2 stream error: {reason}"))
        } else {
            TransportError::other(e)
        }
    })?;

    let status = response.status();
    if status == http::StatusCode::UNAUTHORIZED || status == http::StatusCode::FORBIDDEN {
        return Err(TransportError::Auth(format!("HTTP {status}")));
    }
    if !status.is_success() {
        // 464 is Cursor's "you spoke HTTP/1.1" answer and 415 means the wrong
        // content type for this path; surfacing the code keeps both
        // diagnosable instead of looking like an auth failure.
        return Err(TransportError::Other(format!(
            "Cursor rejected the request: HTTP {status}"
        )));
    }

    Ok(response.into_body())
}

/// Write one Connect frame, respecting HTTP/2 flow control.
pub async fn write_frame(
    send: &mut SendStream<Bytes>,
    payload: &[u8],
) -> Result<(), TransportError> {
    write_bytes(send, Bytes::from(frame::frame(payload, 0)), false).await
}

/// Write an unframed body, as the unary `application/proto` calls expect.
pub async fn write_body(
    send: &mut SendStream<Bytes>,
    payload: &[u8],
    end_of_stream: bool,
) -> Result<(), TransportError> {
    write_bytes(send, Bytes::copy_from_slice(payload), end_of_stream).await
}

async fn write_bytes(
    send: &mut SendStream<Bytes>,
    data: Bytes,
    end_of_stream: bool,
) -> Result<(), TransportError> {
    // An empty write needs no capacity. Reserving zero and then waiting for a
    // capacity callback never resolves, which shows up as a hung request — it
    // is how the unary model-list call first appeared to hang.
    if data.is_empty() {
        return send
            .send_data(data, end_of_stream)
            .map_err(TransportError::other);
    }

    send.reserve_capacity(data.len());

    loop {
        match poll_fn(|cx| send.poll_capacity(cx)).await {
            Some(Ok(n)) if n >= data.len() => break,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(TransportError::other(e)),
            None => return Err(TransportError::Other("stream closed while writing".into())),
        }
    }

    send.send_data(data, end_of_stream)
        .map_err(TransportError::other)
}

/// Encode and write a client message.
pub async fn send_message(
    send: &mut SendStream<Bytes>,
    message: &pb::AgentClientMessage,
) -> Result<(), TransportError> {
    write_frame(send, &message.encode_to_vec()).await
}

/// Outcome of reading the stream to its end.
pub enum StreamEnd {
    /// The server closed cleanly, or reported an error in the end-stream frame.
    Ended(Option<frame::ConnectError>),
}

/// Read the next server message, enforcing the idle timeout.
///
/// Returns `Ok(None)` once the body is exhausted.
pub async fn next_message(
    body: &mut RecvStream,
    deframer: &mut Deframer,
    deadline: Instant,
) -> Result<Option<Result<pb::AgentServerMessage, StreamEnd>>, TransportError> {
    loop {
        if let Some(frame) = deframer.next_frame() {
            if frame.is_compressed() {
                return Err(TransportError::Other(
                    "Cursor sent a compressed frame, which this client does not negotiate".into(),
                ));
            }
            if frame.is_end_stream() {
                let err = frame::parse_end_stream(&frame.payload).map_err(TransportError::Other)?;
                return Ok(Some(Err(StreamEnd::Ended(err))));
            }
            let message = pb::AgentServerMessage::decode(frame.payload.as_slice())
                .map_err(|e| TransportError::Other(format!("undecodable server message: {e}")))?;
            return Ok(Some(Ok(message)));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::Timeout);
        }

        let chunk = match timeout(remaining, body.data()).await {
            Err(_) => return Err(TransportError::Timeout),
            Ok(None) => return Ok(None),
            Ok(Some(Err(e))) => return Err(TransportError::other(e)),
            Ok(Some(Ok(chunk))) => chunk,
        };

        let _ = body.flow_control().release_capacity(chunk.len());
        deframer.push(&chunk);
    }
}

/// Read a unary response body to completion.
///
/// A unary Cursor call (`application/proto`) answers with the bare protobuf,
/// but the same endpoint will wrap it in Connect frames when asked in the
/// streaming content type. [`decode_unary`] sorts that out; this only collects
/// the bytes.
pub async fn read_body(body: &mut RecvStream) -> Result<Vec<u8>, TransportError> {
    let deadline = Instant::now() + IDLE_TIMEOUT;
    let mut collected = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(TransportError::Timeout);
        }
        match timeout(remaining, body.data()).await {
            Err(_) => return Err(TransportError::Timeout),
            Ok(None) => return Ok(collected),
            Ok(Some(Err(e))) => return Err(TransportError::other(e)),
            Ok(Some(Ok(chunk))) => {
                let _ = body.flow_control().release_capacity(chunk.len());
                collected.extend_from_slice(&chunk);
            }
        }
    }
}

/// Unwrap a unary response, whether or not it arrived Connect-framed.
///
/// The verified live behaviour (probed against api2.cursor.sh on 2026-09-19)
/// is that `application/proto` answers with the bare message, while an
/// end-stream frame carries a JSON error. Both shapes are accepted so a
/// server-side change of mind does not read as a decode failure.
pub fn decode_unary(payload: &[u8]) -> Result<Vec<u8>, TransportError> {
    if payload.len() < 5 {
        return Ok(payload.to_vec());
    }

    let mut deframer = Deframer::new();
    deframer.push(payload);
    while let Some(f) = deframer.next_frame() {
        if f.is_compressed() {
            // Not framed after all: a bare protobuf whose first byte happened
            // to look like a compression flag.
            return Ok(payload.to_vec());
        }
        if f.is_end_stream() {
            if let Some(err) = frame::parse_end_stream(&f.payload).map_err(TransportError::Other)? {
                return Err(if err.is_auth() {
                    TransportError::Auth(err.to_string())
                } else {
                    TransportError::Other(err.to_string())
                });
            }
            continue;
        }
        return Ok(f.payload);
    }

    // Nothing framed cleanly, so treat the whole body as the message.
    Ok(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_and_client_version_match_the_reference() {
        assert_eq!(CURSOR_HOST, "api2.cursor.sh");
        assert_eq!(CURSOR_RUN_PATH, "/agent.v1.AgentService/Run");
        assert_eq!(
            CURSOR_GET_USABLE_MODELS_PATH,
            "/agent.v1.AgentService/GetUsableModels"
        );
        assert_eq!(CURSOR_CLIENT_VERSION, "cli-2026.02.13-41ac335");
    }

    #[test]
    fn auth_errors_are_distinguishable_from_transport_errors() {
        assert!(matches!(
            TransportError::Auth("HTTP 401".into()),
            TransportError::Auth(_)
        ));
        assert_eq!(
            TransportError::Timeout.to_string(),
            "Cursor stream timed out after 60s"
        );
    }

    #[test]
    fn tls_roots_are_available() {
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    }

    #[test]
    fn a_bare_protobuf_body_is_returned_untouched() {
        // A GetUsableModels answer is not Connect-framed: the first bytes are
        // protobuf and must survive decode_unary unchanged.
        let bare = vec![0x0a, 0x05, b'h', b'e', b'l', b'l', b'o', 0x00];
        assert_eq!(decode_unary(&bare).unwrap(), bare);
    }

    #[test]
    fn a_framed_body_is_unwrapped() {
        let framed = frame::frame(b"payload", 0);
        assert_eq!(decode_unary(&framed).unwrap(), b"payload".to_vec());
    }

    #[test]
    fn an_end_stream_auth_error_surfaces_as_auth() {
        let framed = frame::frame(
            br#"{"error":{"code":"unauthenticated","message":"Error"}}"#,
            frame::FLAG_END_STREAM,
        );
        assert!(matches!(decode_unary(&framed), Err(TransportError::Auth(_))));
    }

    /// Hits the real api2.cursor.sh with a deliberately invalid token.
    ///
    /// No subscription is needed: the point is to prove the transport,
    /// headers, framing and error mapping against the live server instead of
    /// against our own assumptions. Ignored by default because it needs the
    /// network.
    ///
    /// Run with: `cargo test --lib -- --ignored live_cursor`
    #[tokio::test]
    #[ignore = "requires network access to api2.cursor.sh"]
    async fn live_cursor_run_rejects_a_bad_token_through_the_end_stream_frame() {
        let mut connection = Connection::connect(CURSOR_HOST)
            .await
            .expect("h2 + ALPN connect");

        let (response, mut send) = connection
            .open(
                CURSOR_RUN_PATH,
                "invalid-probe-token",
                "application/connect+proto",
                false,
            )
            .await
            .expect("open the Run stream");

        write_frame(&mut send, b"").await.expect("opening frame");
        let mut body = receive(response)
            .await
            .expect("the Run endpoint answers 200 for a framed stream");

        let mut deframer = Deframer::new();
        let deadline = Instant::now() + IDLE_TIMEOUT;
        let message = next_message(&mut body, &mut deframer, deadline)
            .await
            .expect("read the answer");

        match message {
            Some(Err(StreamEnd::Ended(Some(err)))) => assert!(
                err.is_auth(),
                "a bad token must map to a re-login, got {err}"
            ),
            Some(Err(StreamEnd::Ended(None))) => panic!("expected an error, got a clean close"),
            Some(Ok(_)) => panic!("expected an error, got a server message"),
            None => panic!("expected an error, got an empty body"),
        }
    }

    /// The unary model list is NOT the streaming content type: probed live,
    /// `application/connect+proto` answers 415 on this path.
    #[tokio::test]
    #[ignore = "requires network access to api2.cursor.sh"]
    async fn live_cursor_model_list_uses_the_unary_content_type() {
        let mut connection = Connection::connect(CURSOR_HOST)
            .await
            .expect("h2 + ALPN connect");
        let (response, mut send) = connection
            .open(
                CURSOR_GET_USABLE_MODELS_PATH,
                "invalid-probe-token",
                "application/connect+proto",
                false,
            )
            .await
            .expect("open the stream");
        write_body(&mut send, b"", true).await.expect("write body");
        match receive(response).await {
            Err(TransportError::Other(message)) => assert!(
                message.contains("415"),
                "the streaming content type is refused here, got {message}"
            ),
            other => panic!("expected a 415 refusal, got {}", other.is_ok()),
        }

        let mut connection = Connection::connect(CURSOR_HOST)
            .await
            .expect("h2 + ALPN connect");
        let (response, mut send) = connection
            .open(
                CURSOR_GET_USABLE_MODELS_PATH,
                "invalid-probe-token",
                "application/proto",
                false,
            )
            .await
            .expect("open the stream");
        write_body(&mut send, b"", true).await.expect("write body");
        assert!(
            matches!(receive(response).await, Err(TransportError::Auth(_))),
            "a bad token must read as auth, not as a protocol error"
        );
    }
}
