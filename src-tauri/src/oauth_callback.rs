//! Loopback listener for OAuth redirects.
//!
//! Replaces `Bun.serve()` from sayknow-cli's `utils/oauth/callback-server.ts`.
//! That file runs on Bun; this app is Tauri, so the socket lives in Rust and
//! the result is handed to the webview as a `oauth:callback` event.
//!
//! Deliberately hand-rolled on `std::net` rather than pulling in a web
//! framework: this serves exactly one request, on loopback, for a few seconds.

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

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
    /// The AI/webview listener. Only `oauth_callback_start`/`_stop` touch it.
    current: Mutex<Option<Listener>>,
    /// The native (sync) listener. Only Rust reaches it: the webview commands
    /// above can neither stop nor replace it (N-6).
    native: Arc<Mutex<Option<NativeListener>>>,
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
    stop_listener_slot(&state.current);
}

/// Stops whatever listener `slot` holds. Touches nothing else, so the native
/// slot survives every webview-initiated stop or restart.
fn stop_listener_slot(slot: &Mutex<Option<Listener>>) {
    if let Ok(mut slot) = slot.lock() {
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

// ---------------------------------------------------------------------------
// Native listener (sync sign-in, §4.8 / N-6)
//
// The result goes to Rust through a oneshot channel, never to the webview, so
// the authorization code never leaves the backend. Only Rust can stop it: the
// webview commands above touch `current` only.
// ---------------------------------------------------------------------------

/// Why the native listener produced no callback. Carries no code or state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OAuthError {
    /// An empty expected `state` would accept any redirect; nothing was bound.
    EmptyState,
    /// A native listener is already running.
    Busy,
    /// The loopback socket could not be bound.
    Bind(String),
    /// The listener stopped without a result (cancelled, dropped or panicked).
    Cancelled,
    /// No callback with the expected `state` arrived in time.
    Timeout,
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyState => f.write_str("OAuth state must not be empty"),
            Self::Busy => f.write_str("an OAuth sign-in is already waiting"),
            Self::Bind(reason) => write!(f, "failed to bind OAuth callback listener: {reason}"),
            Self::Cancelled => f.write_str("OAuth sign-in was cancelled"),
            Self::Timeout => f.write_str("OAuth sign-in timed out"),
        }
    }
}

/// The redirect that carried the expected `state`. Exactly one field is set:
/// the provider's error wins over a code.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct NativeOutcome {
    pub code: Option<String>,
    pub error: Option<String>,
}

/// The authorization code is a credential until exchanged; never print it.
impl fmt::Debug for NativeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeOutcome")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("error", &self.error)
            .finish()
    }
}

/// The occupant of the native slot. Dropping `sender` (taking it out of the
/// shared cell) resolves the waiter at once.
struct NativeListener {
    id: u64,
    stop: Arc<AtomicBool>,
    sender: Arc<Mutex<Option<oneshot::Sender<NativeOutcome>>>>,
}

/// The receiving half handed to the caller of [`start_native_listener`].
pub(crate) struct NativeWait {
    rx: oneshot::Receiver<NativeOutcome>,
    timed_out: Arc<AtomicBool>,
}

impl NativeWait {
    /// Resolves when the listener delivers, times out, or is stopped. A
    /// dropped sender without the timeout flag is `Cancelled`, immediately.
    pub(crate) async fn wait(self) -> Result<NativeOutcome, OAuthError> {
        match self.rx.await {
            Ok(outcome) => Ok(outcome),
            Err(_) if self.timed_out.load(Ordering::SeqCst) => Err(OAuthError::Timeout),
            Err(_) => Err(OAuthError::Cancelled),
        }
    }
}

/// A poisoned slot still holds a valid `Option`; recover it rather than
/// leaving the slot stuck.
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Frees the native slot (only if it still holds listener `id`) and drops the
/// sender, so the waiter resolves. Runs on every listener exit, panics too.
struct NativeExit {
    id: u64,
    slot: Arc<Mutex<Option<NativeListener>>>,
    sender: Arc<Mutex<Option<oneshot::Sender<NativeOutcome>>>>,
}

impl NativeExit {
    fn free_slot(&self) {
        let mut slot = lock_recover(&self.slot);
        if slot.as_ref().is_some_and(|listener| listener.id == self.id) {
            *slot = None;
        }
    }
}

impl Drop for NativeExit {
    fn drop(&mut self) {
        self.free_slot();
        drop(lock_recover(&self.sender).take());
    }
}

const NATIVE_SUCCESS: &str = "Sign-in complete. You can close this tab and return to SayKnow Kit.";
const NATIVE_FAILURE: &str = "Sign-in did not complete. Return to SayKnow Kit to try again.";
/// The request line of a redirect is short; a longer one is refused.
const MAX_REQUEST_LINE: u64 = 16 * 1024;
const NATIVE_IO_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
thread_local! {
    /// Test hook: the next `start_native_listener` on this thread fails as if
    /// the OS refused a new thread (std drops the closure, then errors).
    static FAIL_NEXT_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Bind a loopback listener for a Rust-driven sign-in on `port` (0 picks an
/// ephemeral one) and return the bound port plus the waiter. Only a request to
/// `path` with `state == expected_state` completes it; anything else is
/// answered (404/400) and the listener keeps waiting until `timeout`.
pub(crate) fn start_native_listener(
    st: &OAuthCallbackState,
    port: u16,
    path: &str,
    expected_state: String,
    timeout: Duration,
) -> Result<(u16, NativeWait), OAuthError> {
    if expected_state.is_empty() {
        return Err(OAuthError::EmptyState);
    }
    // Held across bind and install so two starts cannot both pass the check.
    let mut slot = lock_recover(&st.native);
    if slot.is_some() {
        return Err(OAuthError::Busy);
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| OAuthError::Bind(e.to_string()))?;
    let bound = listener.local_addr().map_err(|e| OAuthError::Bind(e.to_string()))?.port();
    listener.set_nonblocking(true).map_err(|e| OAuthError::Bind(e.to_string()))?;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(tx)));
    let stop = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let exit = NativeExit { id, slot: st.native.clone(), sender: sender.clone() };
    let path = path.to_owned();
    let worker_stop = stop.clone();
    let worker_timed_out = timed_out.clone();
    let deadline = Instant::now() + timeout;

    // Install, then release the lock before spawning: if the spawn fails, the
    // dropped closure's `NativeExit` frees this slot and must not find the
    // mutex still held by this thread.
    *slot = Some(NativeListener { id, stop, sender });
    drop(slot);

    let worker =
        move || serve_native(listener, &path, &expected_state, deadline, &worker_stop, &worker_timed_out, exit);
    let spawn_failed = |e: std::io::Error| OAuthError::Bind(format!("failed to start listener thread: {e}"));
    #[cfg(test)]
    if FAIL_NEXT_SPAWN.with(|fail| fail.replace(false)) {
        drop(worker);
        return Err(spawn_failed(std::io::Error::other("injected spawn failure")));
    }
    std::thread::Builder::new().name("oauth-native-callback".to_owned()).spawn(worker).map_err(spawn_failed)?;
    Ok((bound, NativeWait { rx, timed_out }))
}

/// Stop the native listener, if any. The waiter gets `Cancelled` at once.
/// Reached from Rust and from `sync_connect_cancel` only.
pub(crate) fn cancel_native(st: &OAuthCallbackState) {
    let taken = lock_recover(&st.native).take();
    if let Some(listener) = taken {
        listener.stop.store(true, Ordering::SeqCst);
        drop(lock_recover(&listener.sender).take());
    }
}

fn serve_native(
    listener: TcpListener,
    path: &str,
    expected_state: &str,
    deadline: Instant,
    stop: &AtomicBool,
    timed_out: &AtomicBool,
    exit: NativeExit,
) {
    loop {
        if stop.load(Ordering::SeqCst) || lock_recover(&exit.sender).as_ref().is_none_or(|tx| tx.is_closed()) {
            return;
        }
        if Instant::now() >= deadline {
            // Set before `exit` drops the sender, so the waiter reads it.
            timed_out.store(true, Ordering::SeqCst);
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(outcome) = handle_native_request(stream, path, expected_state, deadline) {
                    // Free the slot before delivering, so the woken caller can
                    // start another sign-in without hitting `Busy`.
                    exit.free_slot();
                    if let Some(tx) = lock_recover(&exit.sender).take() {
                        let _ = tx.send(outcome);
                    }
                    return;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            // A transient accept failure; the deadline bounds the retries.
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// Answers one request. Returns the outcome only for the expected path and
/// state; every other request is refused and the listener keeps waiting. A
/// request still unread at `deadline` is dropped without an answer.
fn handle_native_request(
    stream: TcpStream,
    path: &str,
    expected_state: &str,
    deadline: Instant,
) -> Option<NativeOutcome> {
    // An accepted socket may inherit non-blocking mode. Reads are bounded by
    // the listener's own deadline (see `DeadlineRead`), so no client, not even
    // one trickling a byte at a time, holds the listener past it.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(NATIVE_IO_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let Some(target) = read_native_target(&stream, deadline) else {
        if Instant::now() < deadline {
            respond(stream, "405 Method Not Allowed", "");
        }
        return None;
    };
    if target.split('?').next().unwrap_or("") != path {
        respond(stream, "404 Not Found", "");
        return None;
    }
    if query_param(&target, "state").as_deref() != Some(expected_state) {
        respond(stream, "400 Bad Request", "");
        return None;
    }
    let outcome = match (query_param(&target, "error"), query_param(&target, "code")) {
        (Some(error), _) => NativeOutcome { code: None, error: Some(error) },
        (None, Some(code)) if !code.is_empty() => NativeOutcome { code: Some(code), error: None },
        (None, _) => NativeOutcome { code: None, error: Some("missing_code".to_owned()) },
    };
    // The page never echoes provider text, so nothing it sent is rendered.
    if outcome.code.is_some() {
        respond(stream, "200 OK", &completion_page(true, "Signed in", NATIVE_SUCCESS));
    } else {
        respond(stream, "400 Bad Request", &completion_page(false, "Sign-in failed", NATIVE_FAILURE));
    }
    Some(outcome)
}

/// Reads from a socket with every read bounded by
/// `min(NATIVE_IO_TIMEOUT, time left before deadline)`; once the deadline has
/// passed, the next read fails `TimedOut` without touching the socket.
struct DeadlineRead<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "listener deadline passed"));
        }
        self.stream.set_read_timeout(Some(remaining.min(NATIVE_IO_TIMEOUT)))?;
        self.stream.read(buf)
    }
}

/// Like [`read_request_target`], but reads at most [`MAX_REQUEST_LINE`] bytes
/// and never past `deadline`.
fn read_native_target(stream: &TcpStream, deadline: Instant) -> Option<String> {
    let mut reader = BufReader::new(DeadlineRead { stream, deadline }.take(MAX_REQUEST_LINE));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !line.ends_with('\n') {
        return None;
    }
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next().map(str::to_owned)
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

    // --- native listener (AC 14), against real loopback sockets ---

    const STATE: &str = "s-1234";

    /// Sends one GET and returns the response status code.
    fn get_status(port: u16, target: &str) -> u16 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or(0)
    }

    fn native_occupied(st: &OAuthCallbackState) -> bool {
        lock_recover(&st.native).is_some()
    }

    /// Puts an idle AI-style listener into `current`, as `oauth_callback_start`
    /// would, without an `AppHandle`.
    fn install_ai_listener(st: &OAuthCallbackState) -> TcpListener {
        let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        *st.current.lock().unwrap() = Some(Listener { stop: Arc::new(AtomicBool::new(false)), port });
        socket
    }

    #[test]
    fn native_empty_state_is_refused_before_binding() {
        let st = OAuthCallbackState::default();
        let result = start_native_listener(&st, 0, "/callback", String::new(), Duration::from_secs(5));
        assert!(matches!(result, Err(OAuthError::EmptyState)));
        assert!(!native_occupied(&st), "nothing was installed");
    }

    #[tokio::test]
    async fn native_second_start_is_busy_until_the_first_ends() {
        let st = OAuthCallbackState::default();
        let (_, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(5)).unwrap();
        let second = start_native_listener(&st, 0, "/callback", "other".into(), Duration::from_secs(5));
        assert!(matches!(second, Err(OAuthError::Busy)));
        // An AI listener in `current` never makes the native slot busy.
        let _ai = install_ai_listener(&st);
        cancel_native(&st);
        assert_eq!(wait.wait().await, Err(OAuthError::Cancelled));
        let (_, again) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(5)).unwrap();
        cancel_native(&st);
        assert_eq!(again.wait().await, Err(OAuthError::Cancelled));
    }

    #[tokio::test]
    async fn native_survives_ai_stop_and_a_second_ai_start() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(10)).unwrap();
        // First AI flow, then the webview's stop, then a second AI start (which
        // stops the current slot before installing), then another stop.
        let _first = install_ai_listener(&st);
        stop_listener_slot(&st.current);
        stop_listener_slot(&st.current);
        let _second = install_ai_listener(&st);
        stop_listener_slot(&st.current);
        assert!(st.current.lock().unwrap().is_none());
        assert!(native_occupied(&st), "webview-side stops never reach the native slot");

        assert_eq!(get_status(port, &format!("/callback?code=abc%2Fd&state={STATE}")), 200);
        let outcome = wait.wait().await.unwrap();
        assert_eq!(outcome, NativeOutcome { code: Some("abc/d".into()), error: None });
        assert!(!native_occupied(&st), "a delivered result frees the slot");
    }

    #[tokio::test]
    async fn native_bad_or_missing_state_is_refused_and_the_listener_keeps_waiting() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(10)).unwrap();
        assert_eq!(get_status(port, "/callback?code=evil&state=wrong"), 400);
        assert_eq!(get_status(port, "/callback?code=evil"), 400);
        assert_eq!(get_status(port, "/callback?error=access_denied"), 400);
        assert_eq!(get_status(port, &format!("/elsewhere?code=evil&state={STATE}")), 404);
        assert!(native_occupied(&st), "still waiting");
        assert_eq!(get_status(port, &format!("/callback?code=good&state={STATE}")), 200);
        assert_eq!(wait.wait().await.unwrap(), NativeOutcome { code: Some("good".into()), error: None });
    }

    #[tokio::test]
    async fn native_provider_error_wins_over_a_code() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start_native_listener(&st, 0, "/cb", STATE.into(), Duration::from_secs(10)).unwrap();
        assert_eq!(get_status(port, &format!("/cb?code=x&error=access_denied&state={STATE}")), 400);
        let outcome = wait.wait().await.unwrap();
        assert_eq!(outcome, NativeOutcome { code: None, error: Some("access_denied".into()) });
    }

    #[tokio::test]
    async fn native_only_mismatches_end_in_timeout_not_cancelled() {
        let st = OAuthCallbackState::default();
        let started = Instant::now();
        let (port, wait) =
            start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_millis(300)).unwrap();
        assert_eq!(get_status(port, "/callback?code=evil&state=wrong"), 400);
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(!native_occupied(&st), "a timed-out listener frees the slot");
    }

    #[tokio::test]
    async fn native_cancel_resolves_cancelled_well_before_the_timeout() {
        let st = Arc::new(OAuthCallbackState::default());
        let (_, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30)).unwrap();
        let started = Instant::now();
        let canceller = {
            let st = st.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                cancel_native(&st);
            })
        };
        assert_eq!(wait.wait().await, Err(OAuthError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        canceller.await.unwrap();
        assert!(!native_occupied(&st));
    }

    #[test]
    fn native_dropped_waiter_frees_the_slot() {
        let st = OAuthCallbackState::default();
        let (_, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30)).unwrap();
        drop(wait);
        let deadline = Instant::now() + Duration::from_secs(2);
        while native_occupied(&st) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!native_occupied(&st));
    }

    #[test]
    fn native_outcome_debug_hides_the_code() {
        let shown = format!("{:?}", NativeOutcome { code: Some("secret-code".into()), error: None });
        assert!(!shown.contains("secret-code"), "{shown}");
    }

    /// A failed thread spawn drops the worker closure while the slot is
    /// already installed: its exit frees the slot (no self-deadlock on the
    /// slot mutex), the socket closes, and a later start succeeds.
    #[test]
    fn native_spawn_failure_frees_the_slot_and_a_later_start_succeeds() {
        let st = OAuthCallbackState::default();
        FAIL_NEXT_SPAWN.with(|fail| fail.set(true));
        let err = start_native_listener(&st, 0, "/callback", "s".into(), Duration::from_secs(30)).map(drop).unwrap_err();
        assert!(matches!(&err, OAuthError::Bind(reason) if reason.contains("failed to start listener thread")), "{err:?}");
        assert!(!native_occupied(&st), "the failed start leaves the slot free");
        let (port, _wait) = start_native_listener(&st, 0, "/callback", "s".into(), Duration::from_secs(30)).unwrap();
        assert!(native_occupied(&st));
        assert_eq!(get_status(port, "/callback?state=wrong&code=x"), 400, "the later listener serves");
        cancel_native(&st);
    }
}

/// G6 red team (ultragoal G006, AC 14 / N-6): adversarial tests for the
/// native loopback listener over real `127.0.0.1` sockets. They sit here
/// because the slot fields are private. A real defect is reproduced as
/// `#[ignore = "G6 blocker: ..."]`.
#[cfg(test)]
mod redteam_g6_oauth {
    use super::*;
    use std::sync::Barrier;

    const STATE: &str = "rt-state-1";

    /// Sends `raw` and returns (status, whole response). 0 when the server
    /// closed without a status line.
    fn exchange(port: u16, raw: &[u8]) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let _ = stream.write_all(raw);
        if raw.is_empty() {
            // Connect-and-close: an idle client is RT-NAT-2/3, not a flood.
            let _ = stream.shutdown(Shutdown::Write);
        }
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let response = String::from_utf8_lossy(&response).into_owned();
        let status = response.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, response)
    }

    fn get(port: u16, target: &str) -> (u16, String) {
        exchange(port, format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
    }

    fn occupied(st: &OAuthCallbackState) -> bool {
        lock_recover(&st.native).is_some()
    }

    fn start(st: &OAuthCallbackState, timeout: Duration) -> (u16, NativeWait) {
        start_native_listener(st, 0, "/callback", STATE.into(), timeout).unwrap()
    }

    /// RT-NAT-1: a flood of wrong-path, wrong-state, wrong-method and garbage
    /// requests from many clients delivers nothing; the real redirect still
    /// completes afterwards.
    #[tokio::test]
    async fn flood_of_bad_requests_delivers_nothing_then_the_real_redirect_lands() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let started = Instant::now();
        let workers: Vec<_> = (0..64)
            .map(|i| {
                std::thread::spawn(move || match i % 6 {
                    0 => get(port, "/callback?code=evil&state=wrong").0,
                    1 => get(port, &format!("/elsewhere?code=evil&state={STATE}")).0,
                    2 => exchange(port, format!("POST /callback?code=evil&state={STATE} HTTP/1.1\r\n\r\n").as_bytes()).0,
                    3 => exchange(port, b"\x00\x01\x02garbage\r\n").0,
                    4 => exchange(port, b"").0,
                    _ => get(port, "/callback?code=evil").0,
                })
            })
            .collect();
        let statuses: Vec<u16> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert!(statuses.iter().all(|s| *s != 200), "{statuses:?}");
        assert!(occupied(&st), "still waiting after the flood");
        assert_eq!(get(port, &format!("/callback?code=good&state={STATE}")).0, 200);
        assert_eq!(wait.wait().await.unwrap(), NativeOutcome { code: Some("good".into()), error: None });
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    /// RT-NAT-2: an idle (slowloris) client that never sends a line delays the
    /// real redirect by at most the per-connection I/O timeout; it never
    /// blocks it.
    #[tokio::test]
    async fn idle_client_delays_but_never_blocks_the_real_redirect() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        let real = std::thread::spawn(move || get(port, &format!("/callback?code=late&state={STATE}")).0);
        assert_eq!(wait.wait().await.unwrap(), NativeOutcome { code: Some("late".into()), error: None });
        assert_eq!(real.join().unwrap(), 200);
        assert!(started.elapsed() < NATIVE_IO_TIMEOUT + Duration::from_secs(3), "{:?}", started.elapsed());
        drop(idle);
    }

    /// RT-NAT-3: with an idle client connected the listener still times out,
    /// late by at most one per-connection I/O timeout (loose pass-1 bound;
    /// RT-NAT-16 checks the tighter absolute-deadline bound).
    #[tokio::test]
    async fn idle_client_overshoots_the_deadline_by_at_most_the_io_timeout() {
        let st = OAuthCallbackState::default();
        let started = Instant::now();
        let (port, wait) = start(&st, Duration::from_millis(300));
        let idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        assert!(started.elapsed() < Duration::from_millis(300) + NATIVE_IO_TIMEOUT + Duration::from_millis(1500));
        assert!(!occupied(&st));
        assert!(start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(1)).is_ok());
        cancel_native(&st);
        drop(idle);
    }

    /// RT-NAT-4 (regression for resolved blocker B-G6-2): a client that
    /// trickles one byte at a time never trips the per-read timeout; before
    /// the absolute deadline it kept the single-threaded listener in one
    /// `read_line` far past its own deadline. Now `wait()` returns `Timeout`
    /// when the timeout elapses (g6-design §A "The deadline is absolute").
    #[tokio::test]
    async fn trickling_client_cannot_hold_the_listener_past_its_deadline() {
        let st = OAuthCallbackState::default();
        let started = Instant::now();
        let (port, wait) = start(&st, Duration::from_millis(500));
        let trickler = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for byte in b"GET /callback?pad=xxxxxxxx" {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        let result = wait.wait().await;
        let elapsed = started.elapsed();
        trickler.join().unwrap();
        assert_eq!(result, Err(OAuthError::Timeout));
        assert!(elapsed < Duration::from_secs(2), "timed out only after {elapsed:?}");
    }

    /// RT-NAT-5: an oversized request line (no newline within 16 KiB) is
    /// refused even if it carries the right state; one just under the limit
    /// is accepted.
    #[tokio::test]
    async fn oversized_request_line_is_refused_and_the_listener_keeps_waiting() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let pad = "x".repeat(MAX_REQUEST_LINE as usize);
        let (status, _) = get(port, &format!("/callback?state={STATE}&code=big&pad={pad}"));
        assert_ne!(status, 200);
        let (status, _) = exchange(port, format!("GET /callback?state={STATE}&code=big&pad={pad}").as_bytes());
        assert_ne!(status, 200);
        assert!(occupied(&st), "an oversized line delivers nothing");
        let under = "y".repeat(MAX_REQUEST_LINE as usize - 200);
        assert_eq!(get(port, &format!("/callback?state={STATE}&code=ok&pad={under}")).0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("ok"));
    }

    /// RT-NAT-6: duplicate `state`/`code` parameters: the first occurrence
    /// decides, so a wrong first state is refused; a parameter without `=`
    /// makes the whole query unreadable (fail closed).
    #[tokio::test]
    async fn duplicate_and_malformed_state_params() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        assert_eq!(get(port, &format!("/callback?state=wrong&state={STATE}&code=a")).0, 400);
        assert_eq!(get(port, &format!("/callback?state=&state={STATE}&code=a")).0, 400);
        assert_eq!(get(port, &format!("/callback?flag&state={STATE}&code=a")).0, 400);
        assert_eq!(get(port, &format!("/callback?STATE={STATE}&code=a")).0, 400);
        assert!(occupied(&st));
        assert_eq!(get(port, &format!("/callback?code=first&code=second&state={STATE}&state=wrong")).0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("first"));
    }

    /// RT-NAT-7: percent-encoded state is compared after decoding; near
    /// misses (case, NUL, `+`, truncated escape) are refused.
    #[tokio::test]
    async fn encoded_state_is_decoded_and_near_misses_are_refused() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        for bad in ["RT-STATE-1", "rt-state-1%00", "rt-state-1+", "rt-state-1%2", "rt-state-%31%", "rt%2Dstate%2D1%20"] {
            assert_eq!(get(port, &format!("/callback?state={bad}&code=x")).0, 400, "{bad}");
        }
        assert!(occupied(&st));
        assert_eq!(get(port, "/callback?state=%72t%2Dstate%2d1&code=a%2Bb%26c").0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("a+b&c"));
    }

    /// RT-NAT-8: `cancel_native` while the listener is inside a request
    /// resolves `Cancelled` at once and frees the slot; the old worker never
    /// clears the newer listener's slot. (Observation: the old socket stays
    /// bound until its read returns, so re-binding the same fixed port fails
    /// with `Bind` meanwhile.)
    #[tokio::test]
    async fn cancel_during_an_active_request() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        cancel_native(&st);
        assert_eq!(wait.wait().await, Err(OAuthError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        assert!(!occupied(&st), "the slot is free at once");
        let same_port = start_native_listener(&st, port, "/callback", STATE.into(), Duration::from_secs(30));
        assert!(matches!(same_port, Err(OAuthError::Bind(_))), "observation: old socket still bound");
        assert!(!occupied(&st), "a failed bind installs nothing");
        let (new_port, new_wait) = start(&st, Duration::from_secs(30));
        drop(idle);
        std::thread::sleep(Duration::from_millis(400));
        assert!(occupied(&st), "the old worker's exit never frees the newer listener");
        assert_eq!(get(new_port, &format!("/callback?code=fresh&state={STATE}")).0, 200);
        assert_eq!(new_wait.wait().await.unwrap().code.as_deref(), Some("fresh"));
        // The cancelled listener is gone: nothing answers there any more.
        std::thread::sleep(Duration::from_millis(200));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    /// RT-NAT-9: never Busy after a timeout, a cancel, or a delivery.
    #[tokio::test]
    async fn slot_frees_after_timeout_cancel_and_delivery() {
        let st = OAuthCallbackState::default();
        let (_, wait) = start(&st, Duration::from_millis(150));
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        let (port, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30))
            .expect("not Busy right after Timeout");
        assert_eq!(get(port, &format!("/callback?code=c&state={STATE}")).0, 200);
        assert!(wait.wait().await.is_ok());
        let (_, wait) = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30))
            .expect("not Busy right after delivery");
        cancel_native(&st);
        assert!(start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30)).is_ok(), "not Busy after cancel");
        assert_eq!(wait.wait().await, Err(OAuthError::Cancelled));
        cancel_native(&st);
    }

    /// RT-NAT-10: many simultaneous starts: exactly one wins, the rest are
    /// Busy (never a second bound listener, never a Bind error).
    #[test]
    fn simultaneous_starts_install_exactly_one_listener() {
        let st = Arc::new(OAuthCallbackState::default());
        let barrier = Arc::new(Barrier::new(8));
        let results: Vec<Result<(u16, NativeWait), OAuthError>> = (0..8)
            .map(|_| {
                let (st, barrier) = (st.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(results.iter().filter(|r| r.is_err()).all(|r| matches!(r, Err(OAuthError::Busy))));
        cancel_native(&st);
    }

    /// RT-NAT-11: stopping an AI listener whose slot names the native port
    /// (the webview's stop pokes that port) neither stops nor resolves the
    /// native listener; the AI flow cannot take the native port.
    #[tokio::test]
    async fn ai_stop_on_the_native_port_does_not_touch_the_native_listener() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        *st.current.lock().unwrap() = Some(Listener { stop: Arc::new(AtomicBool::new(false)), port });
        stop_listener_slot(&st.current);
        stop_listener_slot(&st.current);
        assert!(st.current.lock().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(100));
        assert!(occupied(&st));
        assert!(TcpListener::bind(("127.0.0.1", port)).is_err(), "the AI flow cannot bind the native port");
        assert_eq!(get(port, &format!("/callback?code=kept&state={STATE}")).0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("kept"));

        // The reverse: a native start on a port an AI listener holds fails to
        // bind and leaves the AI slot alone.
        let ai = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let ai_port = ai.local_addr().unwrap().port();
        *st.current.lock().unwrap() = Some(Listener { stop: Arc::new(AtomicBool::new(false)), port: ai_port });
        let clash = start_native_listener(&st, ai_port, "/callback", STATE.into(), Duration::from_secs(5));
        assert!(matches!(clash, Err(OAuthError::Bind(_))));
        assert!(st.current.lock().unwrap().is_some());
        assert!(!occupied(&st));
    }

    /// RT-NAT-12: the completion pages never echo the code or provider text.
    #[tokio::test]
    async fn completion_pages_echo_nothing() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let (status, page) = get(port, &format!("/callback?code=CODE-SECRET-9&state={STATE}"));
        assert_eq!(status, 200);
        assert!(!page.contains("CODE-SECRET-9"));
        assert!(wait.wait().await.is_ok());
        let (port, wait) = start(&st, Duration::from_secs(30));
        let (status, page) = get(port, &format!("/callback?error=%3Cscript%3Ex%3C%2Fscript%3E&state={STATE}"));
        assert_eq!(status, 400);
        assert!(!page.contains("<script>"), "{page}");
        assert_eq!(wait.wait().await.unwrap().error.as_deref(), Some("<script>x</script>"));
    }

    /// Writes `bytes` one at a time with `gap` between them, then reads the
    /// answer. Status 0 when the server closed without answering.
    fn trickle(port: u16, bytes: &[u8], gap: Duration) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        for byte in bytes {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(gap);
        }
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let response = String::from_utf8_lossy(&response).into_owned();
        let status = response.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, response)
    }

    /// Slack for scheduler noise on top of the 20 ms accept poll.
    const SLACK: Duration = Duration::from_millis(500);

    /// RT-NAT-13: a valid redirect (right path, right state) trickled so its
    /// newline lands after the deadline is never delivered and never
    /// answered: `Timeout` at the deadline, the client gets no status line,
    /// and the slot frees.
    #[tokio::test]
    async fn valid_redirect_completing_after_the_deadline_is_dropped_unanswered() {
        let st = OAuthCallbackState::default();
        let started = Instant::now();
        let timeout = Duration::from_millis(600);
        let (port, wait) = start(&st, timeout);
        let line = format!("GET /callback?code=late&state={STATE} HTTP/1.1\r\n\r\n");
        let client = std::thread::spawn(move || trickle(port, line.as_bytes(), Duration::from_millis(30)));
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        let elapsed = started.elapsed();
        assert!(elapsed < timeout + SLACK, "Timeout only after {elapsed:?}");
        assert!(!occupied(&st));
        let (status, response) = client.join().unwrap();
        assert_eq!(status, 0, "dropped without an answer: {response:?}");
    }

    /// RT-NAT-14: the same slow trickle that completes well before the
    /// deadline is still served: the per-read bound does not refuse a slow
    /// but legitimate client.
    #[tokio::test]
    async fn slow_trickle_completing_before_the_deadline_is_delivered() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(10));
        let line = format!("GET /callback?code=slow&state={STATE} HTTP/1.1\r\n\r\n");
        let client = std::thread::spawn(move || trickle(port, line.as_bytes(), Duration::from_millis(20)));
        assert_eq!(wait.wait().await.unwrap(), NativeOutcome { code: Some("slow".into()), error: None });
        assert_eq!(client.join().unwrap().0, 200);
    }

    /// RT-NAT-15: many trickling clients queued behind each other cannot
    /// stack their stalls: the listener still times out at its deadline,
    /// nobody gets a 200, the slot frees, and a new start succeeds.
    #[tokio::test]
    async fn many_trickling_clients_cannot_extend_the_deadline() {
        let st = OAuthCallbackState::default();
        let started = Instant::now();
        let timeout = Duration::from_millis(700);
        let (port, wait) = start(&st, timeout);
        let clients: Vec<_> = (0..24)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut line = format!("GET /callback?code=c{i}&state={STATE}&pad=").into_bytes();
                    line.extend(std::iter::repeat_n(b'x', 60));
                    trickle(port, &line, Duration::from_millis(50)).0
                })
            })
            .collect();
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        let elapsed = started.elapsed();
        assert!(elapsed < timeout + SLACK, "Timeout only after {elapsed:?}");
        assert!(!occupied(&st));
        let (_, again) = start(&st, Duration::from_secs(5));
        let statuses: Vec<u16> = clients.into_iter().map(|c| c.join().unwrap()).collect();
        assert!(statuses.iter().all(|s| *s != 200), "{statuses:?}");
        cancel_native(&st);
        assert_eq!(again.wait().await, Err(OAuthError::Cancelled));
    }

    /// RT-NAT-16: an idle client connected at the deadline no longer
    /// overshoots it by the I/O timeout, and it is dropped without a 405.
    #[tokio::test]
    async fn idle_client_is_cut_at_the_deadline_without_an_answer() {
        let st = OAuthCallbackState::default();
        let started = Instant::now();
        let timeout = Duration::from_millis(400);
        let (port, wait) = start(&st, timeout);
        let mut idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        let elapsed = started.elapsed();
        assert!(elapsed < timeout + SLACK, "Timeout only after {elapsed:?} (I/O timeout is {NATIVE_IO_TIMEOUT:?})");
        idle.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut answer = Vec::new();
        let _ = idle.read_to_end(&mut answer);
        assert!(answer.is_empty(), "{:?}", String::from_utf8_lossy(&answer));
    }

    /// RT-NAT-17: keep-alive. A refused request on a kept-alive connection
    /// is answered with `Connection: close` and the connection is closed; a
    /// correct redirect pipelined behind it on the same socket is never read
    /// or delivered; the client holding its end open neither delays the real
    /// redirect nor extends the deadline.
    #[tokio::test]
    async fn keep_alive_connections_never_carry_a_second_request_or_hold_the_deadline() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let pipelined = format!(
            "GET /callback?code=evil&state=wrong HTTP/1.1\r\nConnection: keep-alive\r\n\r\n\
             GET /callback?code=smuggled&state={STATE} HTTP/1.1\r\nConnection: keep-alive\r\n\r\n"
        );
        let (status, response) = exchange(port, pipelined.as_bytes());
        assert_eq!(status, 400);
        assert_eq!(response.matches("HTTP/1.1").count(), 1, "one answer per connection: {response}");
        assert!(response.contains("Connection: close"));
        assert!(occupied(&st), "the pipelined redirect was not delivered");

        let mut held = TcpStream::connect(("127.0.0.1", port)).unwrap();
        held.write_all(b"GET /callback?code=evil&state=wrong HTTP/1.1\r\nConnection: keep-alive\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert_eq!(get(port, &format!("/callback?code=real&state={STATE}")).0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("real"));
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        drop(held);

        let started = Instant::now();
        let timeout = Duration::from_millis(400);
        let (port, wait) = start(&st, timeout);
        let mut held = TcpStream::connect(("127.0.0.1", port)).unwrap();
        held.write_all(b"GET /callback?state=wrong HTTP/1.1\r\nConnection: keep-alive\r\n\r\n").unwrap();
        assert_eq!(wait.wait().await, Err(OAuthError::Timeout));
        assert!(started.elapsed() < timeout + SLACK, "{:?}", started.elapsed());
        assert!(!occupied(&st));
    }

    /// RT-NAT-18 (observation F-9): connections are served one at a time, so
    /// a trickling client delays the real redirect for as long as it keeps
    /// trickling plus one I/O timeout once it goes quiet (bounded only by the
    /// deadline). Pass-2 run observed ~8.3 s for a 3.3 s trickle. After that
    /// the real redirect lands.
    #[tokio::test]
    async fn trickler_delays_the_real_redirect_only_while_it_trickles() {
        let st = OAuthCallbackState::default();
        let (port, wait) = start(&st, Duration::from_secs(30));
        let partial = b"GET /callback?pad=xxxxxxxxxxxxxxx";
        let gap = Duration::from_millis(100);
        let trickler = std::thread::spawn(move || trickle(port, partial, gap).0);
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        let real = std::thread::spawn(move || get(port, &format!("/callback?code=after&state={STATE}")).0);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("after"));
        assert_eq!(real.join().unwrap(), 200);
        assert_ne!(trickler.join().unwrap(), 200);
        let bound = gap * partial.len() as u32 + NATIVE_IO_TIMEOUT + Duration::from_secs(2);
        assert!(started.elapsed() < bound, "{:?}", started.elapsed());
    }

    /// RT-NAT-19: a failed spawn on a fixed port releases the port at once (a
    /// connect is refused, the same port binds again) and leaves no slot.
    #[tokio::test]
    async fn spawn_failure_on_a_fixed_port_releases_the_port() {
        let st = OAuthCallbackState::default();
        let port = TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        FAIL_NEXT_SPAWN.with(|fail| fail.set(true));
        let err = start_native_listener(&st, port, "/callback", STATE.into(), Duration::from_secs(30)).map(drop).unwrap_err();
        assert!(matches!(&err, OAuthError::Bind(reason) if reason.contains("failed to start listener thread")), "{err:?}");
        assert!(!FAIL_NEXT_SPAWN.with(|fail| fail.get()), "the hook fired once");
        assert!(!occupied(&st));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "nothing listens after a failed spawn");
        let (same, wait) = start_native_listener(&st, port, "/callback", STATE.into(), Duration::from_secs(30)).unwrap();
        assert_eq!(same, port);
        assert_eq!(get(port, &format!("/callback?code=ok&state={STATE}")).0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("ok"));
    }

    /// RT-NAT-20: a failed spawn between a cancelled listener (whose worker is
    /// still blocked on an idle client) and a new one: the failed start frees
    /// only its own slot, Busy is reported while a listener is live, and the
    /// stale worker's late exit never frees the newest listener.
    #[tokio::test]
    async fn spawn_failure_next_to_a_stale_worker_never_frees_another_slot() {
        let st = OAuthCallbackState::default();
        let (old_port, old_wait) = start(&st, Duration::from_secs(30));
        let idle = TcpStream::connect(("127.0.0.1", old_port)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        cancel_native(&st);
        assert_eq!(old_wait.wait().await, Err(OAuthError::Cancelled));

        FAIL_NEXT_SPAWN.with(|fail| fail.set(true));
        let failed = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30));
        assert!(matches!(failed, Err(OAuthError::Bind(_))));
        assert!(!occupied(&st));

        let (port, wait) = start(&st, Duration::from_secs(30));
        FAIL_NEXT_SPAWN.with(|fail| fail.set(true));
        let busy = start_native_listener(&st, 0, "/callback", STATE.into(), Duration::from_secs(30));
        assert!(matches!(busy, Err(OAuthError::Busy)));
        assert!(occupied(&st), "a Busy start never touches the live slot");
        FAIL_NEXT_SPAWN.with(|fail| fail.set(false));

        drop(idle);
        std::thread::sleep(Duration::from_millis(400));
        assert!(occupied(&st), "the stale worker's exit never frees the newer slot");
        assert_eq!(get(port, &format!("/callback?code=new&state={STATE}")).0, 200);
        assert_eq!(wait.wait().await.unwrap().code.as_deref(), Some("new"));
    }
}
