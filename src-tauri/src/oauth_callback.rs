//! Loopback listener for OAuth redirects.
//!
//! Replaces `Bun.serve()` from sayknow-cli's `utils/oauth/callback-server.ts`.
//! That file runs on Bun; this app is Tauri, so the socket lives in Rust and
//! the result is handed to the webview as a `oauth:callback` event.
//!
//! Deliberately hand-rolled on `std::net` rather than pulling in a web
//! framework: this serves exactly one request, on loopback, for a few seconds.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Result delivered to the webview. Exactly one of `code` / `error` is set.
#[derive(Clone, Serialize)]
pub struct CallbackPayload {
    /// Matches the `listener_id` returned by `oauth_callback_start`, so a
    /// stale listener's event cannot resolve a newer login attempt.
    pub listener_id: u64,
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

struct Listener {
    stop: Arc<AtomicBool>,
    port: u16,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub struct OAuthCallbackState {
    current: Mutex<Option<Listener>>,
}

/// Minimal completion page. Upstream ships `oauth.html`; this app only needs
/// to tell the user they can close the tab, and the wording is localized by
/// the caller before it reaches here.
fn completion_page(ok: bool, heading: &str, detail: &str) -> String {
    let accent = if ok { "#16a34a" } else { "#dc2626" };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>{heading}</title><style>\
body{{margin:0;height:100vh;display:flex;align-items:center;justify-content:center;\
font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#0b0b0c;color:#e5e5e5}}\
.card{{text-align:center;padding:40px 56px;border-radius:14px;background:#161618;\
border:1px solid #2a2a2e;max-width:420px}}\
h1{{margin:0 0 10px;font-size:17px;color:{accent}}}\
p{{margin:0;font-size:13px;line-height:1.6;color:#9a9aa0}}\
</style></head><body><div class=\"card\"><h1>{heading}</h1><p>{detail}</p></div></body></html>"
    )
}

fn respond(mut stream: TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

/// Pull the request target out of the request line. We only ever answer GET.
fn read_request_target(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    parts.next().map(|target| target.to_string())
}

fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Providers hand back URL-encoded values; `+` is a space in a query string.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bind the loopback listener and serve until the redirect lands.
///
/// `preferred_port` is what the provider has registered for this client;
/// when it is taken we fall back to an ephemeral port and report the real one
/// so the caller can advertise a matching `redirect_uri`.
#[tauri::command]
// The parameter list is the IPC signature the webview calls by name; grouping
// it into a struct would rename every field on the JavaScript side.
#[allow(clippy::too_many_arguments)]
pub fn oauth_callback_start(
    app: AppHandle,
    state: tauri::State<'_, OAuthCallbackState>,
    preferred_port: u16,
    callback_path: String,
    expected_state: String,
    allow_port_fallback: bool,
    success_heading: String,
    success_detail: String,
    failure_heading: String,
) -> Result<(u64, u16), String> {
    stop_current(&state);

    let listener = match TcpListener::bind(("127.0.0.1", preferred_port)) {
        Ok(listener) => listener,
        Err(err) => {
            if !allow_port_fallback {
                return Err(format!(
                    "OAuth callback port {preferred_port} unavailable and this provider \
                     requires an exact redirect URI: {err}"
                ));
            }
            TcpListener::bind(("127.0.0.1", 0))
                .map_err(|e| format!("failed to bind OAuth callback listener: {e}"))?
        }
    };
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to read OAuth callback port: {e}"))?
        .port();

    // Non-blocking so the accept loop can observe the stop flag instead of
    // parking forever when the user cancels.
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("failed to configure OAuth callback listener: {e}"))?;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let stop = Arc::new(AtomicBool::new(false));

    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let target = match read_request_target(&stream) {
                            Some(target) => target,
                            None => {
                                respond(stream, "405 Method Not Allowed", "");
                                continue;
                            }
                        };
                        let path = target.split('?').next().unwrap_or("");
                        if path != callback_path {
                            respond(stream, "404 Not Found", "");
                            continue;
                        }

                        let code = query_param(&target, "code");
                        let returned_state = query_param(&target, "state");
                        let error = query_param(&target, "error");
                        let error_description = query_param(&target, "error_description");

                        // Same precedence as the upstream handler: provider
                        // error, then missing code, then state mismatch.
                        let payload = if let Some(error) = error {
                            CallbackPayload {
                                listener_id: id,
                                code: None,
                                state: returned_state,
                                error: Some(format!(
                                    "Authorization failed: {}",
                                    error_description.unwrap_or(error)
                                )),
                            }
                        } else if code.is_none() {
                            CallbackPayload {
                                listener_id: id,
                                code: None,
                                state: returned_state,
                                error: Some("Missing authorization code".into()),
                            }
                        } else if !expected_state.is_empty()
                            && returned_state.as_deref() != Some(expected_state.as_str())
                        {
                            CallbackPayload {
                                listener_id: id,
                                code: None,
                                state: returned_state,
                                error: Some("State mismatch - possible CSRF attack".into()),
                            }
                        } else {
                            CallbackPayload {
                                listener_id: id,
                                code,
                                state: returned_state,
                                error: None,
                            }
                        };

                        let ok = payload.error.is_none();
                        let body = if ok {
                            completion_page(true, &success_heading, &success_detail)
                        } else {
                            completion_page(
                                false,
                                &failure_heading,
                                payload.error.as_deref().unwrap_or(""),
                            )
                        };
                        respond(
                            stream,
                            if ok { "200 OK" } else { "400 Bad Request" },
                            &body,
                        );

                        let _ = app.emit("oauth:callback", payload);
                        return;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => return,
                }
            }
        });
    }

    if let Ok(mut slot) = state.current.lock() {
        *slot = Some(Listener { stop, port });
    }
    Ok((id, port))
}

fn stop_current(state: &tauri::State<'_, OAuthCallbackState>) {
    if let Ok(mut slot) = state.current.lock() {
        if let Some(listener) = slot.take() {
            listener.stop.store(true, Ordering::Relaxed);
            // Unblock a listener parked between poll intervals.
            let _ = std::net::TcpStream::connect(("127.0.0.1", listener.port));
        }
    }
}

/// Tear the listener down. Safe to call when nothing is running.
#[tauri::command]
pub fn oauth_callback_stop(state: tauri::State<'_, OAuthCallbackState>) {
    stop_current(&state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
        // A trailing, truncated escape must not panic or eat the byte.
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn query_params_are_read_by_name() {
        let target = "/callback?code=abc&state=xyz";
        assert_eq!(query_param(target, "code").as_deref(), Some("abc"));
        assert_eq!(query_param(target, "state").as_deref(), Some("xyz"));
        assert_eq!(query_param(target, "error"), None);
        assert_eq!(query_param("/callback", "code"), None);
    }

    #[test]
    fn an_encoded_error_description_survives_the_round_trip() {
        let target = "/callback?error=access_denied&error_description=User%20said%20no";
        assert_eq!(
            query_param(target, "error_description").as_deref(),
            Some("User said no")
        );
    }
}
