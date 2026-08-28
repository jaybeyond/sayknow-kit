mod agent_usage;
mod display;
mod clipboard;

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use keyring::Entry;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri::Emitter;
#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_positioner::{Position, WindowExt};

/// Wrapper that kills the child process when dropped. Ensures OCP doesn't
/// outlive SayKnow Kit if the app crashes / quits before the JS-side stop.
struct OcpChild(Option<Child>);

impl Drop for OcpChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Global state shared between Rust handlers and JS via Tauri commands/events.
struct AppState {
    pinned: AtomicBool,
    /// The window's logical width, in points, as last requested.
    ///
    /// `outer_size()` is not usable for this: right after a show, a hide, or a
    /// resize it can report a mix of logical and physical numbers, and every
    /// one of those moments is exactly when the popover is being placed.
    /// Tracking what we asked for removes the guesswork.
    logical_width: Mutex<f64>,
    ocp: Mutex<OcpChild>,
    /// True once the positioner plugin has cached the tray icon's geometry
    /// (set when the user first hovers / clicks the tray, or we feed it a
    /// synthetic event). `move_window(TrayCenter)` panics if this isn't set.
    tray_positioned: AtomicBool,
}

/// Move the main window under the tray, but only if the positioner plugin
/// already has the tray's geometry cached. Without this guard,
/// `move_window(TrayCenter)` unwraps on `None` and aborts the process.
/// Index of the monitor containing `point`, in physical pixels.
fn monitor_containing(point: (i32, i32), monitors: &[Rect]) -> Option<usize> {
    let (px, py) = point;
    monitors.iter().position(|&(mx, my, mw, mh)| {
        px >= mx && px < mx + mw as i32 && py >= my && py < my + mh as i32
    })
}

/// Where the popover belongs: centred under the tray icon, kept inside the
/// display the icon is on. All inputs are physical pixels.
fn tray_anchored_origin(
    tray: Rect,
    win_w: u32,
    monitor: Rect,
    margin: i32,
) -> (i32, i32) {
    let (tx, ty, tw, th) = tray;
    let (mx, _my, mw, _mh) = monitor;
    let x = tx + tw as i32 / 2 - win_w as i32 / 2;
    let min_x = mx + margin;
    let max_x = mx + mw as i32 - win_w as i32 - margin;
    // A window wider than the display has nowhere to be clamped to; pinning it
    // to the left edge at least keeps the title and controls reachable.
    let x = if max_x >= min_x {
        x.clamp(min_x, max_x)
    } else {
        min_x
    };
    (x, ty + th as i32)
}

/// Position the popover from the tray rect Tauri reports, rather than the one
/// tauri-plugin-positioner caches.
///
/// The plugin converts the cached rect with a hardcoded scale factor of 1.0 and
/// then subtracts the window's *physical* width. On a mixed-DPI setup those are
/// different coordinate spaces, so the same tray icon lands the window in a
/// different place depending on which display the window happened to be on
/// last — which is what "the position keeps being wrong" looks like. Reading
/// the rect and converting it with the display's real scale factor keeps both
/// sides in physical pixels.
/// `intended_logical_width` is for callers that just asked for a resize:
/// `set_size` doesn't land before the next run-loop turn, so `outer_size()`
/// still reports the old width and the window ends up centred for a size it no
/// longer has. Passing the width we asked for avoids reading a value that
/// hasn't been applied yet.
fn position_under_tray(app: &AppHandle, intended_logical_width: Option<f64>) -> bool {
    let Some(win) = app.get_webview_window("main") else {
        log::info!("place: no main window");
        return false;
    };
    let Some(tray) = app.tray_by_id("sayknow-tray") else {
        log::info!("place: no tray by id");
        return false;
    };
    let rect = match tray.rect() {
        Ok(Some(r)) => r,
        Ok(None) => {
            log::info!("place: tray.rect() = None");
            return false;
        }
        Err(e) => {
            log::info!("place: tray.rect() failed: {e}");
            return false;
        }
    };
    let Ok(monitors) = win.available_monitors() else {
        return false;
    };
    if monitors.is_empty() {
        return false;
    }

    let logical_rect = |m: &tauri::Monitor| -> Rect {
        let p = m.position().to_logical::<f64>(m.scale_factor());
        let sz = m.size().to_logical::<f64>(m.scale_factor());
        (
            p.x.round() as i32,
            p.y.round() as i32,
            sz.width.round() as u32,
            sz.height.round() as u32,
        )
    };

    // Which display the tray icon is on is not fixed: with "Displays have
    // separate Spaces" — the macOS default — the menu bar follows the active
    // display, so clicking the icon over there must open the popover over
    // there. Assuming the primary display pulled the window back to the
    // built-in screen every time.
    //
    // The rect may arrive logical or physical and we can't tell which, so try
    // each display's own scale factor: the correct pairing is the one whose
    // converted point lands inside that same display.
    let rects: Vec<Rect> = monitors.iter().map(logical_rect).collect();
    let found = monitors.iter().enumerate().find(|(i, m)| {
        let p = rect.position.to_logical::<f64>(m.scale_factor());
        monitor_containing((p.x.round() as i32, p.y.round() as i32), &rects) == Some(*i)
    });
    // A tray rect that sits on no display at all is not a layout we can reason
    // about — it shows up briefly around startup as (0, screen_height), and
    // forcing it onto a display anyway put the window below the bottom edge.
    // Hand those to the fallback, which ends in a visibility check.
    let Some((index, _)) = found else {
        log::info!(
            "place: tray rect {:?} is on no display; falling back",
            rect.position
        );
        return false;
    };
    let monitor = &monitors[index];

    let scale = monitor.scale_factor();
    let tp = rect.position.to_logical::<f64>(scale);
    let ts = rect.size.to_logical::<f64>(scale);
    let mrect = rects[index];

    let logical_w = intended_logical_width.unwrap_or_else(|| {
        app.state::<AppState>()
            .logical_width
            .lock()
            .map(|w| *w)
            .unwrap_or(480.0)
    });
    let (x, y) = tray_anchored_origin(
        (
            tp.x.round() as i32,
            tp.y.round() as i32,
            ts.width.round() as u32,
            ts.height.round() as u32,
        ),
        logical_w.round() as u32,
        mrect,
        8,
    );
    // Multi-monitor placement can only be diagnosed from the numbers the app
    // actually saw; a screenshot of the result doesn't say which display or
    // scale factor it worked from.
    log::info!(
        "place(pt): tray_raw={:?} scale={} tray=({:.0},{:.0},{:.0},{:.0}) \
monitor#{}=({},{},{},{}) win_w={} -> ({},{})",
        rect.position,
        scale,
        tp.x,
        tp.y,
        ts.width,
        ts.height,
        index,
        mrect.0,
        mrect.1,
        mrect.2,
        mrect.3,
        logical_w,
        x,
        y
    );
    if win
        .set_position(tauri::LogicalPosition::new(x as f64, y as f64))
        .is_err()
    {
        return false;
    }
    // Cheap last look: the arithmetic above is clamped to the target display,
    // but a display that disappears between reading the monitor list and
    // moving the window would still strand it.
    ensure_on_screen(&win);
    true
}

fn safe_move_to_tray(app: &AppHandle) {
    safe_move_to_tray_sized(app, None)
}

fn safe_move_to_tray_sized(app: &AppHandle, intended_logical_width: Option<f64>) {
    // Our own placement is already clamped to the menu bar's display, so it
    // needs no rescue pass.
    if position_under_tray(app, intended_logical_width) {
        return;
    }
    let state = app.state::<AppState>();
    if !state.tray_positioned.load(Ordering::Relaxed) {
        return;
    }
    let Some(win) = app.get_webview_window("main") else { return };
    // Defense in depth: even if the cache exists, positioner does Option
    // arithmetic that could panic on multi-display edge cases. catch_unwind
    // keeps a crashy positioner from taking the whole app down.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = win.move_window(Position::TrayCenter);
    }));
    ensure_on_screen(&win);
}

/// A rectangle in logical points: (x, y, width, height).
///
/// Points, not pixels, deliberately. macOS's global coordinate space is in
/// points and `set_outer_position` divides whatever it is given by the scale
/// factor of the display the window is on *right now* — so a physical
/// coordinate computed for a different display arrives divided by the wrong
/// number. With displays of differing density that is the whole bug.
type Rect = (i32, i32, u32, u32);

/// Area of the window that falls inside `monitor`.
fn overlap_area(win: Rect, monitor: Rect) -> i64 {
    let (wx, wy, ww, wh) = win;
    let (mx, my, mw, mh) = monitor;
    let x = (wx + ww as i32).min(mx + mw as i32) - wx.max(mx);
    let y = (wy + wh as i32).min(my + mh as i32) - wy.max(my);
    if x <= 0 || y <= 0 {
        0
    } else {
        x as i64 * y as i64
    }
}

/// True when no single monitor shows enough of the window to be usable.
///
/// The positioner anchors to a tray rect captured at click time. Unplug a
/// display, change the arrangement, or move the menu bar to another screen and
/// that rect can point into space that no longer exists — the window then
/// "opens" somewhere nobody can see it, which reads as the app being broken.
fn is_offscreen(win: Rect, monitors: &[Rect]) -> bool {
    let area = win.2 as i64 * win.3 as i64;
    if area == 0 || monitors.is_empty() {
        return false;
    }
    let best = monitors
        .iter()
        .map(|m| overlap_area(win, *m))
        .max()
        .unwrap_or(0);
    // Two thirds keeps a deliberately half-hidden window alone while still
    // catching one that landed on a detached display.
    best * 3 < area * 2
}

/// Top-right of `monitor`, just below the menu bar — where a tray popover
/// belongs when we have to place it ourselves.
fn tray_fallback_origin(win: Rect, monitor: Rect, scale: f64) -> (i32, i32) {
    let margin = (8.0 * scale).round() as i32;
    let menubar = (26.0 * scale).round() as i32;
    let x = monitor.0 + monitor.2 as i32 - win.2 as i32 - margin;
    let y = monitor.1 + menubar;
    (x.max(monitor.0), y)
}

fn ensure_on_screen(win: &tauri::WebviewWindow) {
    // Logical points throughout, for the same reason as the placement above:
    // a pixel coordinate only means something on the display it came from.
    let scale = win.scale_factor().unwrap_or(1.0);
    let (Ok(pos), Ok(size)) = (win.outer_position(), win.outer_size()) else {
        return;
    };
    let pos = pos.to_logical::<f64>(scale);
    let size = size.to_logical::<f64>(scale);
    let Ok(monitors) = win.available_monitors() else {
        return;
    };
    let rects: Vec<Rect> = monitors
        .iter()
        .map(|m| {
            let p = m.position().to_logical::<f64>(m.scale_factor());
            let sz = m.size().to_logical::<f64>(m.scale_factor());
            (
                p.x.round() as i32,
                p.y.round() as i32,
                sz.width.round() as u32,
                sz.height.round() as u32,
            )
        })
        .collect();
    let win_rect: Rect = (
        pos.x.round() as i32,
        pos.y.round() as i32,
        size.width.round() as u32,
        size.height.round() as u32,
    );
    if !is_offscreen(win_rect, &rects) {
        return;
    }
    // Prefer the primary monitor: on macOS that is the one carrying the menu
    // bar, and therefore the tray icon the user just clicked.
    let Some(rect) = rects.first().copied() else {
        return;
    };
    // Prefer the display carrying the menu bar when we can identify it.
    let rect = win
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let p = m.position().to_logical::<f64>(m.scale_factor());
            let sz = m.size().to_logical::<f64>(m.scale_factor());
            (
                p.x.round() as i32,
                p.y.round() as i32,
                sz.width.round() as u32,
                sz.height.round() as u32,
            )
        })
        .unwrap_or(rect);
    let (x, y) = tray_fallback_origin(win_rect, rect, 1.0);
    let _ = win.set_position(tauri::LogicalPosition::new(x as f64, y as f64));
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const KEYRING_SERVICE: &str = "com.sayknow.app";
const KEYRING_USER: &str = "openrouter_api_key";

fn entry() -> Result<Entry, String> {
    Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())
}

/// Secondary credentials (currently the DeepL key) live under their own
/// Keychain account in the same service. Named accounts rather than one blob
/// so signing out of one provider can't take the other's key with it.
fn named_entry(account: &str) -> Result<Entry, String> {
    // Whitelist: the account name reaches the Keychain, so it is never taken
    // straight from the frontend.
    let allowed = matches!(account, "deepl_api_key");
    if !allowed {
        return Err(format!("unknown credential: {account}"));
    }
    Entry::new(KEYRING_SERVICE, account).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_secret(account: String) -> Result<Option<String>, String> {
    match named_entry(&account)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn set_secret(account: String, key: String) -> Result<(), String> {
    named_entry(&account)?
        .set_password(&key)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_secret(account: String) -> Result<(), String> {
    match named_entry(&account)?.delete_credential() {
        Ok(_) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn get_api_key() -> Result<Option<String>, String> {
    match entry()?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn set_api_key(key: String) -> Result<(), String> {
    entry()?.set_password(&key).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_api_key() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(_) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn hide_window(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        win.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn set_pinned(state: tauri::State<AppState>, pinned: bool) {
    state.pinned.store(pinned, Ordering::Relaxed);
}

#[tauri::command]
fn resize_main_window(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    if let Ok(mut w) = app.state::<AppState>().logical_width.lock() {
        *w = width;
    }
    if let Some(win) = app.get_webview_window("main") {
        win.set_size(tauri::LogicalSize::new(width, height))
            .map_err(|e| e.to_string())?;
    }
    // Re-anchor under the tray after resize so a smaller window doesn't end
    // up half-offscreen. The width we just asked for is passed through, since
    // the window itself won't report it until the next run-loop turn.
    safe_move_to_tray_sized(&app, Some(width));
    Ok(())
}

/// macOS GUI apps don't inherit the user's interactive shell PATH, so a
/// `claude` installed via npm / nvm / homebrew won't be on the default
/// PATH. Try common install locations first; if none hit, ask the user's
/// login shell to resolve the binary (`/bin/sh -lc 'command -v claude'`).
fn find_claude() -> Option<PathBuf> {
    // Delegate to which_via_shell so both detection helpers share the same
    // (more permissive) lookup path. Keeps a single source of truth for
    // which dirs and which shells we trust on macOS.
    which_via_shell("claude")
}

#[derive(serde::Serialize)]
pub struct ClaudeCliInfo {
    pub path: String,
    pub version: String,
}

#[tauri::command]
fn detect_claude_cli() -> Result<Option<ClaudeCliInfo>, String> {
    let Some(path) = find_claude() else { return Ok(None) };
    // Sanity-check by asking for version. Some installs print the version on
    // stdout, some on stderr — we don't care which, just that it runs.
    let out = Command::new(&path)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Ok(None)
    }
    let mut version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if version.is_empty() {
        version = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    Ok(Some(ClaudeCliInfo {
        path: path.to_string_lossy().into_owned(),
        version,
    }))
}

#[derive(serde::Deserialize)]
pub struct ClaudeMessage {
    pub role: String,
    pub content: String,
}

#[derive(serde::Serialize)]
pub struct ClaudeChatResult {
    pub content: String,
    pub model: String,
}

/// Calls `claude --print` with the user's prompt and returns its stdout.
/// `messages` follows OpenAI chat shape — we serialize the conversation into
/// a single prompt string and pull `system` out into `--append-system-prompt`.
#[tauri::command]
fn claude_chat(
    messages: Vec<ClaudeMessage>,
    model: Option<String>,
) -> Result<ClaudeChatResult, String> {
    let path = find_claude().ok_or_else(|| "Claude CLI not found".to_string())?;

    let mut system_parts: Vec<String> = Vec::new();
    let mut convo: Vec<String> = Vec::new();
    for m in &messages {
        match m.role.as_str() {
            "system" => system_parts.push(m.content.clone()),
            "user" => convo.push(format!("User:\n{}", m.content)),
            "assistant" => convo.push(format!("Assistant:\n{}", m.content)),
            _ => {}
        }
    }
    let prompt = convo.join("\n\n");
    if prompt.is_empty() {
        return Err("empty prompt".into())
    }

    let mut cmd = Command::new(&path);
    cmd.arg("--print");
    if let Some(m) = &model {
        if !m.trim().is_empty() {
            cmd.arg("--model").arg(m);
        }
    }
    if !system_parts.is_empty() {
        cmd.arg("--append-system-prompt").arg(system_parts.join("\n\n"));
    }
    cmd.arg(prompt);

    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        let code = out.status.code().unwrap_or(-1);
        return Err(format!("claude exited {}: {}", code, err.trim()))
    }
    let content = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(ClaudeChatResult {
        content,
        model: model.unwrap_or_default(),
    })
}

/// Find an executable installed anywhere the user's shell would find it.
/// macOS GUI apps don't inherit interactive PATH, so we fall back to the
/// user's actual login shell (zsh on modern macOS), which is what sources
/// `~/.zprofile` and the Homebrew `shellenv` block. `/bin/sh -lc` runs
/// bash, which silently ignores zsh profile files and ends up with a
/// stripped-down PATH that doesn't see Homebrew.
fn which_via_shell(name: &str) -> Option<PathBuf> {
    let common = [
        format!("/opt/homebrew/bin/{}", name),
        format!("/opt/homebrew/sbin/{}", name),
        format!("/usr/local/bin/{}", name),
        format!("/usr/local/sbin/{}", name),
    ];
    for p in &common {
        if std::path::Path::new(p).exists() {
            return Some(p.into())
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        for sub in [
            format!(".npm-global/bin/{}", name),
            format!(".local/bin/{}", name),
            format!(".bun/bin/{}", name),
            format!(".volta/bin/{}", name),
            format!(".fnm/aliases/default/bin/{}", name),
            format!(".nvm/versions/node/{}", name), // unlikely to land but harmless
        ] {
            let p = PathBuf::from(&home).join(sub);
            if p.exists() {
                return Some(p)
            }
        }
    }

    // Final fallback: ask the user's actual login shell (zsh by default on
    // macOS Catalina+). $SHELL is usually empty in a GUI-launched process,
    // so we don't trust it as the only option — we try the user's $SHELL
    // first if set, then zsh, then sh.
    let shells: Vec<String> = {
        let mut v: Vec<String> = Vec::with_capacity(3);
        if let Ok(s) = std::env::var("SHELL") {
            if !s.is_empty() {
                v.push(s);
            }
        }
        v.push("/bin/zsh".to_string());
        v.push("/bin/sh".to_string());
        v
    };
    for shell in shells {
        let out = Command::new(&shell)
            .args(["-lc", &format!("command -v {} 2>/dev/null", name)])
            .output()
            .ok();
        if let Some(out) = out {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(PathBuf::from(s));
                }
            }
        }
    }
    None
}

#[derive(serde::Serialize)]
pub struct OcpEnv {
    pub ocp_path: Option<String>,
    pub npm_path: Option<String>,
    pub claude_path: Option<String>,
    pub node_path: Option<String>,
    /// True when we have a `~/.sayknow-runtime/.../bin/node` that we downloaded
    /// ourselves. Lets the UI distinguish system vs. our private runtime.
    pub private_node_available: bool,
    pub running: bool,
    /// Port where OCP is actually responding (3456 by default, but the user
    /// may have started OCP on a different port, or we may have fallen back
    /// to one when :3456 was occupied). `None` if OCP isn't reachable.
    pub running_port: Option<u16>,
}

fn port_open(port: u16) -> bool {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Ports we'll consider when looking for an existing OCP instance. The
/// default OCP port is 3456 but the user (or our own fallback) may have
/// picked something else when that was occupied. Scanned in priority order.
const OCP_CANDIDATE_PORTS: &[u16] = &[3456, 3457, 3458, 3459, 3460];

/// Probe an HTTP service that's listening on `localhost:port` and return
/// true if its `/v1/models` response looks like OCP — i.e. lists at least
/// one Claude model. This lets us distinguish OCP from any other process
/// that happens to be bound to the same port.
fn looks_like_ocp(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/v1/models", port);
    let out = Command::new("curl")
        .args(["-s", "-m", "1", &url])
        .output()
        .ok();
    let Some(out) = out else { return false };
    if !out.status.success() {
        return false;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    body.contains("claude-") || body.contains("\"data\":")
}

/// Scan candidate ports + any user-supplied URL's port to find an OCP
/// instance that's already running. Returns the first port where the
/// `/v1/models` response looks like OCP.
fn find_running_ocp_port(user_base_url: Option<&str>) -> Option<u16> {
    let mut candidates: Vec<u16> = OCP_CANDIDATE_PORTS.to_vec();
    if let Some(url) = user_base_url {
        if let Some(p) = port_from_url(url) {
            if !candidates.contains(&p) {
                candidates.insert(0, p);
            }
        }
    }
    for p in candidates {
        if port_open(p) && looks_like_ocp(p) {
            return Some(p);
        }
    }
    None
}

/// Pick a port we can bind ourselves: prefer 3456, then the next free in
/// the candidate list. Returns None if every candidate is occupied.
fn pick_free_ocp_port() -> Option<u16> {
    for p in OCP_CANDIDATE_PORTS {
        if !port_open(*p) {
            return Some(*p);
        }
    }
    None
}

/// Extract host port from a baseURL like "http://localhost:3457/v1".
fn port_from_url(url: &str) -> Option<u16> {
    let after_scheme = url.split("://").nth(1)?;
    let host_port = after_scheme.split('/').next()?;
    let port = host_port.split(':').nth(1)?;
    port.parse().ok()
}

/// The user's actual login-shell `$PATH`, as zsh / bash / fish would see
/// it after sourcing `~/.zprofile`, `~/.zshrc`, etc. macOS GUI apps inherit
/// a stripped-down PATH that's missing Homebrew, nvm, volta, asdf, etc., so
/// we ask the user's own shell what its PATH actually is and prepend that
/// to OCP's install pipeline.
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if std::path::Path::new("/bin/zsh").exists() {
                Some("/bin/zsh".to_string())
            } else {
                None
            }
        })?;
    let out = Command::new(&shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Build the PATH we'll prepend to OCP's install pipeline. Order:
///   1. The Node bin dir we picked (so node/npm point at the right toolchain).
///   2. Parent dirs of any other binaries `setup.mjs` needs (`claude`, `git`,
///      `ocp`) as discovered by `which_via_shell` — fully dynamic, picks up
///      nvm, volta, asdf, Homebrew, or any other layout.
///   3. The user's login-shell `$PATH` so anything else they have continues
///      to resolve normally.
///   4. A small fixed safety net (`/opt/homebrew/bin`, `/usr/local/bin`,
///      `/usr/bin`, `/bin`) in case all of the above failed to surface
///      somewhere standard.
fn ocp_pipeline_path(node_bin_dir: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push_dedup = |p: String| {
        if !p.is_empty() && !parts.contains(&p) {
            parts.push(p);
        }
    };

    push_dedup(node_bin_dir.to_string_lossy().into_owned());

    for tool in ["claude", "git", "ocp", "node", "npm"] {
        if let Some(found) = which_via_shell(tool) {
            if let Some(parent) = found.parent() {
                push_dedup(parent.to_string_lossy().into_owned());
            }
        }
    }

    if let Some(p) = login_shell_path() {
        for seg in p.split(':') {
            push_dedup(seg.to_string());
        }
    }

    for fallback in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        push_dedup(fallback.to_string());
    }

    parts.join(":")
}

/// macOS uid — needed for `launchctl bootout/enable gui/<uid>/<label>`.
/// `id -u` is universally available; avoids pulling in libc explicitly.
fn current_uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "501".to_string())
}

fn ocp_log_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".ocp/logs/proxy.log"))
}

fn append_ocp_log(line: &str) {
    let Some(path) = ocp_log_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", line);
    }
}

fn tail_ocp_log(lines: usize) -> String {
    let Some(path) = ocp_log_path() else { return String::new() };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let mut tail: Vec<&str> = raw.lines().rev().take(lines).collect();
    tail.reverse();
    tail.join("\n")
}

fn emit_and_persist_ocp_log(app: &AppHandle, line: impl AsRef<str>) {
    let line = line.as_ref();
    append_ocp_log(line);
    let _ = app.emit("ocp:log", line.to_string());
}

#[cfg(target_os = "windows")]
fn exe_name(name: &str) -> String {
    format!("{}.exe", name)
}

#[cfg(not(target_os = "windows"))]
fn exe_name(name: &str) -> String {
    name.to_string()
}

fn bootout_ocp_service() {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        let _ = Command::new("launchctl")
            .arg("bootout")
            .arg(format!("gui/{}/dev.ocp.proxy", uid))
            .output();
    }
}

// ───────── Private Node.js runtime ─────────
// Downloaded under ~/.sayknow-runtime/ when the user doesn't have a system
// Node. No sudo, no global PATH changes — we just point `start_ocp`'s shell
// at our private bin directory.

/// Bundled Node.js runtime that SayKnow Kit downloads when the user's system
/// Node is too old (or missing). Node 24 LTS "Krypton" is what OCP's
/// `node:sqlite` import actually expects — node:sqlite was promoted out
/// of experimental in the 23.x → 24.x window, so this version runs
/// server.mjs without needing any feature flags.
const NODE_VERSION: &str = "v24.15.0";
/// Minimum system Node version we'll trust. OCP needs the `node:sqlite`
/// built-in, which arrived behind `--experimental-sqlite` in 22.5. Older
/// versions (18.x, 20.x — the typical macOS LTS minimum) get our bundled
/// runtime instead, automatically.
const MIN_NODE_MAJOR: u64 = 22;
const MIN_NODE_MINOR: u64 = 5;

fn runtime_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(PathBuf::from(home).join(".sayknow-runtime"))
}

fn node_subdir() -> &'static str {
    // Must match NODE_VERSION exactly — Node's tarballs unpack into a
    // directory named after the version+platform combo. Keep in sync.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    { "node-v24.15.0-darwin-arm64" }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    { "node-v24.15.0-darwin-x64" }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    { "node-v24.15.0-linux-x64" }
    #[cfg(target_os = "windows")]
    { "node-v24.15.0-win-x64" }
}

fn node_archive_url() -> String {
    let base = node_subdir();
    #[cfg(not(target_os = "windows"))]
    return format!("https://nodejs.org/dist/{}/{}.tar.gz", NODE_VERSION, base);
    #[cfg(target_os = "windows")]
    return format!("https://nodejs.org/dist/{}/{}.zip", NODE_VERSION, base);
}

fn private_node_bin_dir() -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join(node_subdir()).join("bin"))
}

fn parse_node_version(s: &str) -> Option<(u64, u64, u64)> {
    let trimmed = s.trim().trim_start_matches('v');
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn node_version_supported(bin_dir: &Path) -> bool {
    let node = bin_dir.join(exe_name("node"));
    if !node.exists() {
        return false;
    }
    let Ok(out) = Command::new(&node).arg("-v").output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let version = String::from_utf8_lossy(&out.stdout);
    let Some((major, minor, _patch)) = parse_node_version(&version) else {
        return false;
    };
    major > MIN_NODE_MAJOR || (major == MIN_NODE_MAJOR && minor >= MIN_NODE_MINOR)
}

/// Effective bin dir that contains `node` / `npm` for our OCP shell pipeline.
/// Prefers the user's system Node only when it is new enough for OCP
/// (>=22.5); falls back to `~/.sayknow-runtime/.../bin` otherwise. This is
/// important because many Macs have Node 18/20 installed; `setup.mjs` can
/// still write the plist with that old Node, but `server.mjs` then exits
/// immediately and :3456 never opens.
fn effective_node_bin_dir() -> Option<PathBuf> {
    if let Some(p) = which_via_shell("node") {
        if let Some(d) = p.parent() {
            let d = d.to_path_buf();
            if node_version_supported(&d) {
                return Some(d);
            }
        }
    }
    let priv_dir = private_node_bin_dir().ok()?;
    if node_version_supported(&priv_dir) {
        Some(priv_dir)
    } else {
        None
    }
}

/// Download + extract the official Node.js tarball into ~/.sayknow-runtime/.
/// Uses the OS-bundled `curl` and `tar` so we don't need new Rust deps.
fn install_node_runtime_inner(app: &AppHandle) -> Result<PathBuf, String> {
    let bin_dir = private_node_bin_dir()?;
    if bin_dir.join("node").exists() {
        return Ok(bin_dir);
    }
    let dir = runtime_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    // Clean up any older bundled Node we don't reference anymore. Keeps the
    // ~/.sayknow-runtime/ directory from accumulating dead versions across
    // SayKnow Kit upgrades.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let current = node_subdir();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if name_s.starts_with("node-v") && name_s.as_ref() != current {
                let _ = app.emit(
                    "ocp:log",
                    format!("    Removing old Node runtime: {}", name_s),
                );
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    let url = node_archive_url();
    let archive_path = dir.join("node-archive.tar.gz");

    let _ = app.emit(
        "ocp:log",
        format!("Downloading Node.js {} (~50 MB)…", NODE_VERSION),
    );
    let _ = app.emit("ocp:log", format!("  {}", url));
    let curl_status = Command::new("curl")
        .args(["-fL", "--retry", "2", "--show-error", "--silent", "-o"])
        .arg(&archive_path)
        .arg(&url)
        .status()
        .map_err(|e| format!("curl spawn failed: {}", e))?;
    if !curl_status.success() {
        return Err(format!(
            "curl exited {} downloading Node",
            curl_status.code().unwrap_or(-1)
        ));
    }

    let _ = app.emit("ocp:log", "Extracting Node runtime…");
    let tar_status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&dir)
        .status()
        .map_err(|e| format!("tar spawn failed: {}", e))?;
    if !tar_status.success() {
        return Err(format!(
            "tar exited {} extracting Node",
            tar_status.code().unwrap_or(-1)
        ));
    }
    let _ = std::fs::remove_file(&archive_path);

    if !bin_dir.join("node").exists() {
        return Err("Node extracted but binary not found at expected path".to_string());
    }
    let _ = app.emit("ocp:log", format!("Node runtime ready: {}", bin_dir.display()));
    Ok(bin_dir)
}

/// `npm i -g @anthropic-ai/claude-code` using whichever npm is in
/// `node_bin_dir`. With a private Node this installs to the private prefix
/// (no sudo); with the system Node this hits the user's global npm.
fn install_claude_cli_inner(app: &AppHandle, node_bin_dir: &Path) -> Result<(), String> {
    if which_via_shell("claude").is_some() {
        return Ok(());
    }
    let local_claude = node_bin_dir.join("claude");
    if local_claude.exists() {
        return Ok(());
    }
    let npm = node_bin_dir.join("npm");
    if !npm.exists() {
        return Err(format!("npm not found at {}", npm.display()));
    }
    let _ = app.emit("ocp:log", "Installing Claude CLI via npm…");

    let mut child = Command::new(&npm)
        .args(["install", "-g", "@anthropic-ai/claude-code", "--no-audit", "--no-fund"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("npm spawn failed: {}", e))?;
    if let Some(stdout) = child.stdout.take() {
        let a = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = a.emit("ocp:log", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let a = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = a.emit("ocp:log", line);
            }
        });
    }
    let status = child.wait().map_err(|e| format!("npm wait: {}", e))?;
    if !status.success() {
        return Err(format!(
            "npm install Claude CLI exited {}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

#[tauri::command]
fn install_node_runtime(app: AppHandle) -> Result<String, String> {
    let p = install_node_runtime_inner(&app)?;
    Ok(p.to_string_lossy().into_owned())
}

#[tauri::command]
fn detect_ocp_env(base_url: Option<String>) -> OcpEnv {
    let priv_node = private_node_bin_dir().ok().and_then(|d| {
        let p = d.join("node");
        if p.exists() {
            Some(p.to_string_lossy().into_owned())
        } else {
            None
        }
    });
    let sys_node = which_via_shell("node").map(|p| p.to_string_lossy().into_owned());
    // Scan known OCP ports plus whatever the user has configured. We don't
    // assume 3456 — any candidate that responds with a Claude-flavored
    // `/v1/models` answer counts. This way SayKnow Kit connects to OCP whether
    // the user (or another tool) started it on 3456, 3457, or wherever.
    let running_port = find_running_ocp_port(base_url.as_deref());
    OcpEnv {
        ocp_path: which_via_shell("ocp")
            .or_else(cloned_ocp_cli_path)
            .map(|p| p.to_string_lossy().into_owned()),
        npm_path: which_via_shell("npm").map(|p| p.to_string_lossy().into_owned()),
        claude_path: which_via_shell("claude").map(|p| p.to_string_lossy().into_owned()),
        node_path: sys_node.or_else(|| priv_node.clone()),
        private_node_available: priv_node.is_some(),
        running: running_port.is_some(),
        running_port,
    }
}

/// OCP isn't an npm package — it's a git repo with `node setup.mjs` as the
/// installer (which also registers a macOS launchd service so OCP keeps
/// running across reboots without our process). We clone into a known
/// directory under $HOME so re-runs can do a fast `git pull` instead.
fn ocp_dir() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(format!("{}/.sayknow-ocp", home))
}

fn cloned_ocp_cli_path() -> Option<PathBuf> {
    let dir = ocp_dir().ok()?;
    let p = PathBuf::from(dir).join(exe_name("ocp"));
    if p.exists() { Some(p) } else { None }
}

/// Last-resort OCP runner. On some machines `setup.mjs` successfully writes
/// and loads the launchd plist but the service never binds before the
/// installer's health check (or launchd is disabled/broken for the user
/// domain). In that case, keep OCP alive as a SayKnow Kit child process instead
/// of leaving the user stuck at "setup exited 1".
fn start_ocp_direct(
    app: &AppHandle,
    state: &AppState,
    node_bin_dir: &Path,
    reason: &str,
) -> Result<(), String> {
    let dir = PathBuf::from(ocp_dir()?);
    let server = dir.join("server.mjs");
    if !server.exists() {
        return Err(format!("OCP server not found at {}", server.display()));
    }

    let node = node_bin_dir.join(exe_name("node"));
    if !node.exists() {
        return Err(format!("node not found at {}", node.display()));
    }

    emit_and_persist_ocp_log(app, format!("Spawning OCP server ({reason})"));

    // Clean any old launchd service from previous SayKnow Kit versions so it
    // can't fight us for the port. Harmless when no service exists.
    bootout_ocp_service();

    // Pick a port that's actually free. Prefer 3456 (OCP's default), then
    // fall back through OCP_CANDIDATE_PORTS. If the user already has an
    // OCP responding on one of those ports we'd have returned earlier from
    // start_ocp; getting here means none of them was OCP.
    let port = match pick_free_ocp_port() {
        Some(p) => p,
        None => {
            return Err(
                "All candidate OCP ports (3456-3460) are occupied by non-OCP processes."
                    .to_string(),
            );
        }
    };
    emit_and_persist_ocp_log(app, format!("Using port {} for OCP", port));

    let mut cmd = Command::new(&node);
    // `node:sqlite` is a Node 22.5+ built-in but is only available behind
    // `--experimental-sqlite` until at least 22.x; OCP's server.mjs imports
    // it eagerly and would crash on startup without the flag. Newer Node
    // builds just print a deprecation note for the flag (harmless).
    cmd.arg("--experimental-sqlite")
        .arg(&server)
        .current_dir(&dir)
        .env("PATH", ocp_pipeline_path(node_bin_dir))
        .env("CLAUDE_PROXY_PORT", port.to_string())
        .env("CLAUDE_BIND", "127.0.0.1")
        .env("CLAUDE_AUTH_MODE", "none")
        .env("DISABLE_AUTOUPDATER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    if let Some(claude) = find_claude() {
        cmd.env("CLAUDE_BIN", claude);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start OCP directly: {}", e))?;

    if let Some(stdout) = child.stdout.take() {
        let a = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                emit_and_persist_ocp_log(&a, line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let a = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                emit_and_persist_ocp_log(&a, line);
            }
        });
    }

    {
        let mut guard = state.ocp.lock().map_err(|e| e.to_string())?;
        // Drop any previous direct child before replacing it.
        guard.0 = Some(child);
    }

    for _ in 0..100 {
        if port_open(port) && looks_like_ocp(port) {
            let _ = app.emit("ocp:port", port);
            let _ = app.emit("ocp:status", "ready");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }

    if let Ok(mut guard) = state.ocp.lock() {
        guard.0 = None;
    }

    let tail = tail_ocp_log(60);
    Err(if tail.is_empty() {
        format!("OCP direct start failed: :{} did not open after 20s", port)
    } else {
        format!(
            "OCP direct start failed: :{} did not open after 20s\n\nRecent ~/.ocp/logs/proxy.log:\n{}",
            port, tail
        )
    })
}

#[tauri::command]
fn install_ocp() -> Result<String, String> {
    // Kept for backward compatibility — delegates to the same install flow
    // ensure_ocp uses. Returns the directory we installed into.
    ocp_dir()
}

/// Install/upgrade the OCP repo and dependencies, then spawn `node server.mjs`
/// directly as a SayKnow Kit child process. setup.mjs and launchd are deliberately
/// bypassed — setup.mjs's 5-second health check kept failing, and launchd-
/// spawned servers were flaky across user environments. Direct spawn is much
/// more reliable and lets us stream logs straight to the UI.
///
/// The child is parked in AppState.ocp (Drop kills it), so OCP cleanly stops
/// when SayKnow Kit quits.
#[tauri::command]
fn start_ocp(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    eprintln!("[sayknow] start_ocp invoked");
    let _ = app.emit("ocp:log", "▶ Starting OCP setup…");

    // Already running — could be on 3456, or any other port if the user
    // started OCP themselves with a custom port. Scan candidates.
    if let Some(port) = find_running_ocp_port(None) {
        let _ = app.emit("ocp:log", format!("✓ OCP already responding on :{} — using it", port));
        let _ = app.emit("ocp:port", port);
        let _ = app.emit("ocp:status", "ready");
        return Ok(());
    }

    // ── Step 1: git ────────────────────────────────────────────────────────
    let _ = app.emit("ocp:log", "[1/5] Checking git…");
    if which_via_shell("git").is_none() {
        return Err("git is not installed (try: xcode-select --install)".to_string())
    }

    // ── Step 2: Node (system or downloaded under ~/.sayknow-runtime/) ──────
    let _ = app.emit("ocp:log", "[2/5] Checking Node.js…");
    let node_bin_dir = match effective_node_bin_dir() {
        Some(d) => {
            let _ = app.emit("ocp:log", format!("    ✓ found Node at {}", d.display()));
            d
        }
        None => {
            let _ = app.emit("ocp:status", "installing");
            let _ = app.emit("ocp:log", "    Node not found — downloading…");
            install_node_runtime_inner(&app)?
        }
    };

    // ── Step 3: Claude CLI ────────────────────────────────────────────────
    let _ = app.emit("ocp:log", "[3/5] Checking Claude CLI…");
    install_claude_cli_inner(&app, &node_bin_dir)?;
    let _ = app.emit("ocp:log", "    ✓ Claude CLI ready");

    // ── Step 4: clone or update the OCP repo ──────────────────────────────
    let _ = app.emit("ocp:status", "installing");
    let dir = ocp_dir()?;
    let dir_p = std::path::Path::new(&dir);
    let already_cloned = dir_p.join("server.mjs").exists();
    let prefix_path = ocp_pipeline_path(&node_bin_dir);

    let clone_cmd = if already_cloned {
        let _ = app.emit("ocp:log", "[4/5] Updating OCP repo…");
        format!(
            "export PATH=\"{path}\" && cd '{dir}' && git pull --rebase --autostash 2>&1 | tail -3 && ([ -d node_modules ] || npm install --no-audit --no-fund 2>&1 | tail -10)",
            path = prefix_path, dir = dir,
        )
    } else {
        let _ = app.emit("ocp:log", "[4/5] Cloning OCP repo + npm install…");
        format!(
            "export PATH=\"{path}\" && \
             mkdir -p '{dir}' && \
             git clone --depth 1 https://github.com/dtzp555-max/ocp.git '{dir}' 2>&1 | tail -5 && \
             cd '{dir}' && \
             ([ -d node_modules ] || npm install --no-audit --no-fund 2>&1 | tail -10)",
            path = prefix_path, dir = dir,
        )
    };

    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if std::path::Path::new("/bin/zsh").exists() {
                "/bin/zsh".to_string()
            } else {
                "/bin/sh".to_string()
            }
        });
    let mut child = Command::new(&shell)
        .args(["-lc", &clone_cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn shell: {}", e))?;

    if let Some(stdout) = child.stdout.take() {
        let app = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = app.emit("ocp:log", format!("    {line}"));
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let app = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = app.emit("ocp:log", format!("    {line}"));
            }
        });
    }
    let status = child.wait().map_err(|e| format!("waiting for clone/install: {}", e))?;
    if !status.success() {
        return Err(format!(
            "Clone / npm install failed (exit {})",
            status.code().unwrap_or(-1),
        ));
    }

    // ── Step 5: spawn server.mjs directly ─────────────────────────────────
    let _ = app.emit("ocp:log", "[5/5] Starting OCP server (node server.mjs)…");
    let _ = app.emit("ocp:status", "starting");
    start_ocp_direct(&app, &state, &node_bin_dir, "direct spawn (skip setup.mjs/launchd)").map_err(|err| {
        let tail = tail_ocp_log(60);
        if tail.is_empty() {
            err
        } else {
            format!("{err}\n\nRecent ~/.ocp/logs/proxy.log:\n{tail}")
        }
    })
}

#[tauri::command]
fn stop_ocp(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut guard = state.ocp.lock().map_err(|e| e.to_string())?;
    guard.0 = None; // Drop kills it
    bootout_ocp_service();
    let _ = app.emit("ocp:status", "disconnected");
    Ok(())
}

/// Stop OCP cleanly so the user disconnects from it without nuking the
/// install. We bootout the launchd service (server stops listening on
/// :3456) and that's it — we keep the plist file AND the cloned OCP repo
/// so a later sign-in just needs to re-bootstrap the same plist (fast,
/// no git/npm steps needed). For a full wipe the user can call
/// `uninstall_ocp` explicitly.
#[tauri::command]
fn disconnect_ocp(app: AppHandle) -> Result<(), String> {
    let _ = app.emit("ocp:log", "Stopping OCP daemon…");

    bootout_ocp_service();

    let _ = app.emit("ocp:status", "disconnected");
    let _ = app.emit("ocp:log", "OCP daemon stopped.");
    Ok(())
}

/// Full wipe: stop the daemon, remove its launchd plist (so it stops auto-
/// starting on login), and rm-rf the cloned repo. The user-facing
/// "uninstall OCP" button calls this; ordinary sign-out does not.
#[tauri::command]
fn uninstall_ocp(app: AppHandle) -> Result<(), String> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return Ok(()),
    };
    let plist = PathBuf::from(&home).join("Library/LaunchAgents/dev.ocp.proxy.plist");
    let dir = PathBuf::from(&home).join(".sayknow-ocp");

    let _ = app.emit("ocp:log", "Uninstalling OCP…");

    let uid = current_uid();
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(format!("gui/{}/dev.ocp.proxy", uid))
        .output();
    if plist.exists() {
        let _ = std::fs::remove_file(&plist);
    }
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }

    let _ = app.emit("ocp:status", "uninstalled");
    let _ = app.emit("ocp:log", "OCP uninstalled.");
    Ok(())
}

/// Update the tray menu's quit-item text. React calls this on mount with
/// the resolved i18n string so the menu honors the user's UI locale
/// override. TrayIcon doesn't expose a getter for the current menu, so we
/// rebuild a single-item menu and swap it in via `set_menu`.
#[tauri::command]
fn set_tray_quit_label(app: AppHandle, label: String) -> Result<(), String> {
    let tray = app
        .tray_by_id("sayknow-tray")
        .ok_or_else(|| "tray not found".to_string())?;
    let item = MenuItem::with_id(&app, "quit", &label, true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let menu = Menu::with_items(&app, &[&item]).map_err(|e| e.to_string())?;
    tray.set_menu(Some(menu)).map_err(|e| e.to_string())
}

/// Update the tray icon's hover tooltip. Same rationale as the quit label:
/// the builder can only bake in a locale-neutral default, so React pushes
/// the resolved i18n string once the WebView is up.
#[tauri::command]
fn set_tray_tooltip(app: AppHandle, tooltip: String) -> Result<(), String> {
    let tray = app
        .tray_by_id("sayknow-tray")
        .ok_or_else(|| "tray not found".to_string())?;
    tray.set_tooltip(Some(&tooltip)).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    // Only allow http(s) URLs — no file://, no scheme injection.
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("only http(s) URLs are allowed".into())
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window("settings") {
        let _ = existing.show();
        let _ = existing.set_focus();
        let _ = existing.unminimize();
        return Ok(())
    }
    WebviewWindowBuilder::new(
        &app,
        "settings",
        WebviewUrl::App("index.html?window=settings".into()),
    )
    .title("SayKnow Kit")
    .inner_size(820.0, 580.0)
    .min_inner_size(720.0, 500.0)
    .resizable(true)
    .build()
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn toggle_window(app: &AppHandle, shown_at: &Arc<AtomicI64>, source: &str) {
    if let Some(win) = app.get_webview_window("main") {
        let visible = win.is_visible().unwrap_or(false);
        if visible {
            let _ = win.hide();
        } else {
            safe_move_to_tray(app);
            shown_at.store(now_ms(), Ordering::Relaxed);
            let _ = win.show();
            let _ = win.set_focus();
            // JS listens for this — used to auto-fill clipboard on shortcut open.
            let _ = app.emit("sayknow:open", source);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    eprintln!("[sayknow] run() called");
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_http::init())
        .manage(AppState {
            pinned: AtomicBool::new(false),
            ocp: Mutex::new(OcpChild(None)),
            tray_positioned: AtomicBool::new(false),
            logical_width: Mutex::new(480.0),
        })
        .invoke_handler(tauri::generate_handler![
            get_api_key,
            set_api_key,
            delete_api_key,
            get_secret,
            set_secret,
            delete_secret,
            hide_window,
            set_pinned,
            resize_main_window,
            open_settings,
            open_external,
            set_tray_quit_label,
            set_tray_tooltip,
            detect_claude_cli,
            claude_chat,
            detect_ocp_env,
            install_ocp,
            install_node_runtime,
            start_ocp,
            stop_ocp,
            disconnect_ocp,
            uninstall_ocp,
            clipboard::get_clipboard_history,
            clipboard::set_clipboard_text,
            clipboard::delete_clipboard_entry,
            clipboard::toggle_clipboard_pin,
            clipboard::set_clipboard_entry_note,
            clipboard::clear_clipboard_history,
            clipboard::wipe_clipboard_history,
            clipboard::set_clipboard_capture,
            clipboard::get_clipboard_capture,
            clipboard::set_clipboard_max_entries,
            display::list_displays,
            display::set_display_brightness,
            display::set_display_power,
            display::sync_builtin_brightness,
            agent_usage::agent_usage
        ])
        .setup(|app| {
            eprintln!("[sayknow] setup hook entered");
            // Hide Dock icon — true menu bar utility behavior.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);
            eprintln!("[sayknow] activation policy set (Accessory)");

            // Logging in release too. A shipped build had no log of any kind,
            // so a misplaced window or a failed request could only be guessed
            // at from a screenshot — twice that cost a diagnosis. Info level
            // into the standard log directory is cheap and is the difference
            // between "it's in the wrong place" and knowing which display and
            // scale factor it worked from.
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    .targets([
                        tauri_plugin_log::Target::new(
                            tauri_plugin_log::TargetKind::LogDir { file_name: None },
                        ),
                        tauri_plugin_log::Target::new(
                            tauri_plugin_log::TargetKind::Stdout,
                        ),
                    ])
                    .build(),
            )?;

            // Clipboard history capture. Polls the system pasteboard every
            // 800ms and emits `clipboard:new` for fresh entries. Runs whether
            // the popover is visible or not — that's the whole point.
            let clipboard_handle = clipboard::ClipboardHandle::new();
            app.manage(clipboard_handle.clone());
            clipboard::spawn_poller(app.handle().clone(), clipboard_handle);

            // Tracks the last show() time so the focus-loss handler can ignore
            // the brief blur that fires while the window is being raised.
            let shown_at: Arc<AtomicI64> = Arc::new(AtomicI64::new(0));

            // Global shortcut: ⌘⇧T on macOS, Ctrl+Shift+T elsewhere.
            // SUPER on Windows = Win key, which collides with system shortcuts,
            // so we deliberately route to CONTROL on non-Apple platforms.
            #[cfg(target_os = "macos")]
            let primary_modifier = Modifiers::SUPER;
            #[cfg(not(target_os = "macos"))]
            let primary_modifier = Modifiers::CONTROL;
            let shortcut = Shortcut::new(
                Some(primary_modifier | Modifiers::SHIFT),
                Code::KeyT,
            );
            let shortcut_for_handler = shortcut;
            let shown_at_for_shortcut = shown_at.clone();
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |app, sc, ev| {
                        if sc == &shortcut_for_handler && ev.state() == ShortcutState::Pressed {
                            toggle_window(app, &shown_at_for_shortcut, "shortcut");
                        }
                    })
                    .build(),
            )?;
            app.global_shortcut().register(shortcut)?;

            // Tray icon — use small dedicated 44x44 PNG so it fits the macOS menu bar.
            eprintln!("[sayknow] building tray icon...");
            let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;

            // Default to English; React calls `set_tray_quit_label` with the
            // resolved i18n string once the WebView is up.
            let quit_item = MenuItem::with_id(app, "quit", "Quit SayKnow Kit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit_item])?;

            let shown_at_for_tray = shown_at.clone();
            let tray = TrayIconBuilder::with_id("sayknow-tray")
                .icon(icon)
                .icon_as_template(true)
                .tooltip("SayKnow Kit")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id == "quit" {
                        app.exit(0);
                    }
                })
                .on_tray_icon_event(move |tray, event| {
                    let app = tray.app_handle();
                    tauri_plugin_positioner::on_tray_event(app, &event);
                    // Mark tray geometry as cached so safe_move_to_tray()
                    // is allowed to position windows under the tray.
                    app.state::<AppState>()
                        .tray_positioned
                        .store(true, Ordering::Relaxed);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(app, &shown_at_for_tray, "tray");
                    }
                })
                .build(app)?;
            eprintln!("[sayknow] tray icon built: id={:?}", tray.id());

            // Hide window when it loses focus — popover behavior.
            // Skip blur events that fire within ~400ms of show() to avoid
            // the show-then-immediately-hide race during tray click.
            if let Some(win) = app.get_webview_window("main") {
                let win_clone = win.clone();
                let shown_at_for_blur = shown_at.clone();
                let app_handle_for_blur = app.handle().clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        // Honor user's pin: never hide while pinned.
                        if app_handle_for_blur
                            .state::<AppState>()
                            .pinned
                            .load(Ordering::Relaxed)
                        {
                            return
                        }
                        let since = now_ms() - shown_at_for_blur.load(Ordering::Relaxed);
                        if since > 400 {
                            let _ = win_clone.hide();
                        }
                    }
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri app")
        .run(|_app, event| {
            // Leaving the built-in panel gamma-dimmed after quit, with no
            // obvious way back, is a support ticket. Restore on the way out.
            if let tauri::RunEvent::ExitRequested { .. } = event {
                crate::display::restore_builtin_gamma();
            }
        });
}

#[cfg(test)]
mod window_placement_tests {
    use super::{
        is_offscreen, monitor_containing, overlap_area, tray_anchored_origin,
        tray_fallback_origin, Rect,
    };

    /// This machine, in logical points — the space macOS actually positions
    /// windows in. The built-in is Retina and carries the menu bar; the 1080p
    /// external sits to its left, so at negative x, and is offset downward.
    const BUILTIN: Rect = (0, 0, 1710, 1112);
    const EXTERNAL_LEFT: Rect = (-1920, 441, 1920, 1080);
    /// 480pt normal, 720pt compact. Points on every display, whatever its
    /// density — which is the entire reason placement works in points.
    const NORMAL_W: u32 = 480;
    const COMPACT_W: u32 = 720;

    fn win_at(x: i32, y: i32) -> Rect {
        (x, y, NORMAL_W, 580)
    }

    #[test]
    fn the_popover_is_centred_under_the_tray_icon() {
        // Icon as reported on the built-in: 34pt wide at x 1010.
        let tray: Rect = (1010, 0, 34, 39);
        let (x, y) = tray_anchored_origin(tray, NORMAL_W, BUILTIN, 8);
        assert_eq!(x, 787);
        assert_eq!(x + NORMAL_W as i32 / 2, 1010 + 17);
        assert_eq!(y, 39);
    }

    /// The report that started this: the same icon geometry on a display of a
    /// different density must land in the same place relative to the icon.
    /// Mixing pixels and points is what put the window hundreds of points off.
    #[test]
    fn density_does_not_change_where_the_window_lands() {
        let on_builtin: Rect = (1010, 0, 34, 39);
        let a = tray_anchored_origin(on_builtin, NORMAL_W, BUILTIN, 8);
        assert_eq!(a.0 + NORMAL_W as i32 / 2, 1027);

        // Icon at the same offset into the 1x external display.
        let on_external: Rect = (-1920 + 1010, 441, 34, 30);
        let b = tray_anchored_origin(on_external, NORMAL_W, EXTERNAL_LEFT, 8);
        assert_eq!(b.0 + NORMAL_W as i32 / 2, -1920 + 1027);
        // Same distance from the icon on both, despite 2x versus 1x.
        assert_eq!(a.0 - 1010, b.0 - (-1920 + 1010));
    }

    #[test]
    fn the_popover_opens_on_the_display_holding_the_tray() {
        let tray: Rect = (-700, 441, 34, 30);
        let (x, y) = tray_anchored_origin(tray, NORMAL_W, EXTERNAL_LEFT, 8);
        assert_eq!(x + NORMAL_W as i32 / 2, -700 + 17);
        assert_eq!(y, 471);
        assert!(!is_offscreen((x, y, NORMAL_W, 580), &[EXTERNAL_LEFT]));
        assert!(is_offscreen((x, y, NORMAL_W, 580), &[BUILTIN]));
    }

    #[test]
    fn a_tray_icon_near_the_corner_does_not_push_the_window_off() {
        let tray: Rect = (1690, 0, 34, 39);
        let (x, _) = tray_anchored_origin(tray, NORMAL_W, BUILTIN, 8);
        assert_eq!(x, 1710 - 480 - 8);
        assert!(!is_offscreen((x, 0, NORMAL_W, 580), &[BUILTIN]));
    }

    /// Toggling compact/normal re-anchors immediately, before the window has
    /// applied the new size; the width asked for is what must be used.
    #[test]
    fn a_resize_is_anchored_with_the_width_it_asked_for() {
        let tray: Rect = (1010, 0, 34, 39);
        let normal = tray_anchored_origin(tray, NORMAL_W, BUILTIN, 8);
        let compact = tray_anchored_origin(tray, COMPACT_W, BUILTIN, 8);
        assert_eq!(normal.0, 787);
        assert_eq!(compact.0, 667);
        // Both keep the icon centred; only the extra width differs.
        assert_eq!(
            normal.0 + NORMAL_W as i32 / 2,
            compact.0 + COMPACT_W as i32 / 2
        );
        assert_eq!(normal.0 - compact.0, 120);
    }

    #[test]
    fn the_tray_display_is_picked_by_which_one_contains_the_icon() {
        let monitors = [BUILTIN, EXTERNAL_LEFT];
        assert_eq!(monitor_containing((1010, 10), &monitors), Some(0));
        assert_eq!(monitor_containing((-700, 500), &monitors), Some(1));
        // A point on neither resolves to nothing rather than guessing.
        assert_eq!(monitor_containing((9000, 10), &monitors), None);
        // The external is offset downward, so its own top-left is inside it
        // while the same x above that offset is not.
        assert_eq!(monitor_containing((-1920, 441), &monitors), Some(1));
        assert_eq!(monitor_containing((-1920, 440), &monitors), None);
    }

    #[test]
    fn a_window_on_a_live_monitor_is_left_alone() {
        assert!(!is_offscreen(win_at(1000, 40), &[BUILTIN, EXTERNAL_LEFT]));
        assert!(!is_offscreen(win_at(-1000, 500), &[BUILTIN, EXTERNAL_LEFT]));
    }

    #[test]
    fn a_window_on_a_detached_display_is_flagged() {
        assert!(is_offscreen(win_at(-1000, 500), &[BUILTIN]));
    }

    #[test]
    fn a_mostly_visible_window_is_not_dragged_around() {
        let win = win_at(BUILTIN.2 as i32 - 360, 40);
        assert!(!is_offscreen(win, &[BUILTIN]));
    }

    #[test]
    fn overlap_is_zero_when_rects_do_not_touch() {
        assert_eq!(overlap_area(win_at(-1000, 500), BUILTIN), 0);
        assert!(overlap_area(win_at(100, 40), BUILTIN) > 0);
    }

    #[test]
    fn fallback_lands_top_right_under_the_menu_bar() {
        let (x, y) = tray_fallback_origin(win_at(0, 0), BUILTIN, 1.0);
        assert_eq!(x, 1710 - 480 - 8);
        assert_eq!(y, 26);
        assert!(!is_offscreen((x, y, NORMAL_W, 580), &[BUILTIN]));
    }

    #[test]
    fn fallback_never_pushes_past_the_left_edge() {
        let narrow: Rect = (0, 0, 300, 400);
        let (x, _) = tray_fallback_origin((0, 0, 480, 580), narrow, 1.0);
        assert_eq!(x, 0);
    }

    #[test]
    fn a_window_wider_than_the_display_clamps_to_the_left_edge() {
        let tiny: Rect = (0, 0, 300, 400);
        let (x, _) = tray_anchored_origin((150, 0, 20, 24), 480, tiny, 8);
        assert_eq!(x, 8);
    }

    #[test]
    fn no_monitors_means_no_opinion() {
        assert!(!is_offscreen(win_at(-1000, 500), &[]));
    }
}
