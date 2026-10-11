//! The one door adapters use to reach the network (§4.9).
//!
//! WebDAV and R2 build plain [`HttpRequest`]s and hand them to an
//! [`HttpTransport`]. Production uses [`ReqwestTransport`]; tests use
//! [`MockTransport`], a closure-backed in-memory server, so no test ever opens
//! a socket.

use std::fmt;
use std::future::Future;
use std::time::Duration;

/// Total time one request may take, connect through body.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest response body [`ReqwestTransport`] reads, well above any segment,
/// snapshot or listing page. A larger body is refused, never buffered. Uploads
/// stay below it too (`provider::MAX_OBJECT_BYTES`), so every stored object
/// can be read back.
pub(crate) const DEFAULT_MAX_BODY: usize = 64 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

/// Credential-bearing headers are never printed, so a request can be logged or
/// shown in a failed assertion without leaking a password or signature.
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                let shown = if is_secret_header(name) { "<redacted>" } else { value.as_str() };
                (name.as_str(), shown)
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_len", &self.body.len())
            .finish()
    }
}

fn is_secret_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("proxy-authorization")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

/// First value of `name` (ASCII case-insensitive), as HTTP header names are.
pub(crate) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// The request never produced a response. Strings carry no URL or credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransportFailure {
    Timeout,
    Connect(String),
    Tls(String),
    Other(String),
    /// The response body grew past the transport's cap. Retrying cannot help.
    BodyTooLarge,
}

pub(crate) trait HttpTransport: Send + Sync {
    fn send(&self, req: HttpRequest) -> impl Future<Output = Result<HttpResponse, TransportFailure>> + Send;
}

/// reqwest over rustls with the webpki roots: certificates are always
/// verified, redirects are never followed (a redirect could carry the
/// `Authorization` header to another host), every request times out, and
/// every response body is capped at `max_body` bytes.
pub(crate) struct ReqwestTransport {
    client: reqwest::Client,
    max_body: usize,
}

impl ReqwestTransport {
    pub(crate) fn new() -> Result<Self, TransportFailure> {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| TransportFailure::Other(err.without_url().to_string()))?;
        Ok(Self { client, max_body: DEFAULT_MAX_BODY })
    }
}

impl HttpTransport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| TransportFailure::Other("invalid HTTP method".to_owned()))?;
        let mut builder = self.client.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let mut response = builder.body(req.body).send().await.map_err(classify)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(classify)? {
            append_capped(&mut body, &chunk, self.max_body)?;
        }
        Ok(HttpResponse { status, headers, body })
    }
}

/// Appends `chunk` unless that would take `body` past `max_body` bytes.
fn append_capped(body: &mut Vec<u8>, chunk: &[u8], max_body: usize) -> Result<(), TransportFailure> {
    if chunk.len() > max_body.saturating_sub(body.len()) {
        return Err(TransportFailure::BodyTooLarge);
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn classify(err: reqwest::Error) -> TransportFailure {
    let tls = is_tls(&err);
    let timeout = err.is_timeout();
    let connect = err.is_connect();
    let text = err.without_url().to_string();
    if tls {
        TransportFailure::Tls(text)
    } else if timeout {
        TransportFailure::Timeout
    } else if connect {
        TransportFailure::Connect(text)
    } else {
        TransportFailure::Other(text)
    }
}

/// A rustls error anywhere in the source chain. `io::Error::source` skips the
/// error it wraps, so wrapped errors are inspected through `get_ref` too.
fn is_tls(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(err);
    while let Some(current) = next {
        if current.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io.get_ref() {
                if is_tls(inner) {
                    return true;
                }
            }
        }
        next = current.source();
    }
    false
}

#[cfg(test)]
pub(crate) use mock::MockTransport;

#[cfg(test)]
mod mock {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    type Handler = dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync;

    struct Inner {
        handler: Box<Handler>,
        requests: Mutex<Vec<HttpRequest>>,
        scripted: Mutex<VecDeque<Result<HttpResponse, TransportFailure>>>,
    }

    /// An in-memory server: every request is recorded, then answered by the
    /// next scripted result if any, else by the handler. Clones share state,
    /// so a test keeps one handle while the adapter owns another.
    #[derive(Clone)]
    pub(crate) struct MockTransport {
        inner: Arc<Inner>,
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    impl MockTransport {
        pub(crate) fn new(handler: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> Self {
            Self {
                inner: Arc::new(Inner {
                    handler: Box::new(handler),
                    requests: Mutex::new(Vec::new()),
                    scripted: Mutex::new(VecDeque::new()),
                }),
            }
        }

        pub(crate) fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.inner.requests).clone()
        }

        pub(crate) fn request_count(&self) -> usize {
            lock(&self.inner.requests).len()
        }

        /// The next request gets `result` instead of reaching the handler.
        pub(crate) fn script_next(&self, result: Result<HttpResponse, TransportFailure>) {
            lock(&self.inner.scripted).push_back(result);
        }

        /// The next request gets an empty-bodied `status` response.
        pub(crate) fn fail_next(&self, status: u16, headers: &[(&str, &str)]) {
            let headers = headers.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
            self.script_next(Ok(HttpResponse { status, headers, body: Vec::new() }));
        }
    }

    impl HttpTransport for MockTransport {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailure> {
            lock(&self.inner.requests).push(req.clone());
            let scripted = lock(&self.inner.scripted).pop_front();
            match scripted {
                Some(result) => result,
                None => Ok((self.inner.handler)(&req)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> HttpRequest {
        HttpRequest {
            method: "GET".to_owned(),
            url: "https://example.invalid/x".to_owned(),
            headers: headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn debug_output_redacts_credentials() {
        let shown = format!("{:?}", request(&[("Authorization", "Basic c2VjcmV0"), ("ETag", "\"e1\"")]));
        assert!(!shown.contains("c2VjcmV0"), "{shown}");
        assert!(shown.contains("<redacted>"));
        assert!(shown.contains("ETag"));
    }

    #[test]
    fn header_lookup_ignores_case() {
        let req = request(&[("ETag", "\"a\"")]);
        assert_eq!(req.header("etag"), Some("\"a\""));
        assert_eq!(req.header("missing"), None);
    }

    #[tokio::test]
    async fn mock_records_every_request_and_scripts_override_the_handler() {
        let mock = MockTransport::new(|_| HttpResponse { status: 200, headers: Vec::new(), body: b"ok".to_vec() });
        let handle = mock.clone();
        mock.fail_next(503, &[("Retry-After", "7")]);
        mock.script_next(Err(TransportFailure::Timeout));
        let first = mock.send(request(&[])).await.unwrap();
        assert_eq!(first.status, 503);
        assert_eq!(first.header("retry-after"), Some("7"));
        assert_eq!(mock.send(request(&[])).await, Err(TransportFailure::Timeout));
        assert_eq!(mock.send(request(&[])).await.unwrap().body, b"ok");
        assert_eq!(handle.request_count(), 3);
        assert!(handle.requests().iter().all(|req| req.method == "GET"));
    }

    #[test]
    fn reqwest_transport_builds_without_network() {
        assert!(ReqwestTransport::new().is_ok());
    }

    #[test]
    fn body_cap_accepts_up_to_the_limit_and_refuses_one_byte_more() {
        let mut body = Vec::new();
        assert_eq!(append_capped(&mut body, b"abcd", 6), Ok(()));
        assert_eq!(append_capped(&mut body, b"", 6), Ok(()));
        assert_eq!(append_capped(&mut body, b"ef", 6), Ok(()));
        assert_eq!(body, b"abcdef");
        assert_eq!(append_capped(&mut body, b"g", 6), Err(TransportFailure::BodyTooLarge));
        assert_eq!(body, b"abcdef", "an over-cap chunk is not kept");
        let mut fresh = Vec::new();
        assert_eq!(append_capped(&mut fresh, b"0123456", 6), Err(TransportFailure::BodyTooLarge));
        assert!(fresh.is_empty());
    }

    #[test]
    fn over_cap_body_is_permanent() {
        let err: crate::sync::provider::ProviderError = TransportFailure::BodyTooLarge.into();
        assert!(matches!(err, crate::sync::provider::ProviderError::Permanent(_)), "{err:?}");
    }

    #[test]
    fn reqwest_transport_caps_bodies_by_default() {
        assert_eq!(ReqwestTransport::new().unwrap().max_body, DEFAULT_MAX_BODY);
    }

    #[test]
    fn tls_errors_are_found_inside_io_errors() {
        let wrapped = std::io::Error::other(rustls::Error::General("bad cert".to_owned()));
        assert!(is_tls(&wrapped));
        assert!(!is_tls(&std::io::Error::other("refused")));
    }
}
