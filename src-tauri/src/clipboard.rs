// Background clipboard history capture.
//
// macOS doesn't expose system-wide clipboard history to third-party apps, so
// we poll the system pasteboard every POLL_INTERVAL_MS and build our own
// ring buffer. Entries are deduplicated by content hash, capped at
// `max_entries`, and persisted as JSON under the app data dir so history
// survives restarts.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

const DEFAULT_MAX_ENTRIES: usize = 100;
const POLL_INTERVAL_MS: u64 = 800;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ClipEntry {
    pub id: String,
    pub text: String,
    pub preview: String,
    pub ts: i64,
    pub pinned: bool,
    /// Optional user-authored note attached to this entry. Persisted across
    /// restarts. `#[serde(default)]` so old JSON files (pre-memo feature)
    /// deserialize cleanly without the field.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedState {
    entries: Vec<ClipEntry>,
    #[serde(default)]
    max_entries: Option<usize>,
}

pub struct ClipboardState {
    pub entries: Vec<ClipEntry>,
    pub max_entries: usize,
    /// Last text we observed on the system pasteboard. Lets the poller skip
    /// the no-change case without doing any other work.
    pub last_text: Option<String>,
    /// When we programmatically write to the system clipboard (the user
    /// clicked "reuse"), we stash the text here so the very next poll skips
    /// it instead of re-adding it.
    pub ignore_text: Option<String>,
}

pub struct ClipboardHandle {
    pub state: Mutex<ClipboardState>,
    pub capture_enabled: AtomicBool,
}

impl ClipboardHandle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ClipboardState {
                entries: Vec::new(),
                max_entries: DEFAULT_MAX_ENTRIES,
                last_text: None,
                ignore_text: None,
            }),
            capture_enabled: AtomicBool::new(true),
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn hash_text(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn make_preview(s: &str) -> String {
    let trimmed = s.trim();
    let count = trimmed.chars().count();
    if count <= 80 {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(80).collect();
    format!("{}…", truncated)
}

/// Drop the obviously-sensitive things (OTPs, key material, blanks). Kept
/// intentionally conservative — false positives mean the user loses an
/// entry they'd have wanted. macOS NSPasteboard `concealed` / password-manager
/// transient flag detection is intentionally not here; that lands later via
/// `objc2` so PR1 has no new native deps.
fn looks_sensitive(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.chars().count() < 2 {
        return true;
    }
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if trimmed.contains("BEGIN PRIVATE KEY")
        || trimmed.contains("BEGIN OPENSSH PRIVATE KEY")
        || trimmed.contains("BEGIN RSA PRIVATE KEY")
    {
        return true;
    }
    false
}

fn store_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("clipboard.json"))
}

fn load_persisted(app: &AppHandle) -> (Vec<ClipEntry>, Option<usize>) {
    let Some(path) = store_path(app) else {
        return (Vec::new(), None);
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return (Vec::new(), None);
    };
    let parsed: PersistedState = serde_json::from_str(&raw).unwrap_or_default();
    (parsed.entries, parsed.max_entries)
}

fn save_persisted(app: &AppHandle, entries: &[ClipEntry], max_entries: usize) {
    let Some(path) = store_path(app) else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let state = PersistedState {
        entries: entries.to_vec(),
        max_entries: Some(max_entries),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = fs::write(path, json);
    }
}

fn snapshot(handle: &ClipboardHandle) -> (Vec<ClipEntry>, usize) {
    let s = handle.state.lock().unwrap();
    (s.entries.clone(), s.max_entries)
}

/// Enforce the max-entries cap while always keeping pinned items. Pinned
/// items don't count against the cap (so a user who pinned 200 things doesn't
/// lose them when the cap is 100 — they just won't get new unpinned slots
/// until they unpin some).
fn cap_entries(s: &mut ClipboardState) {
    if s.entries.len() <= s.max_entries {
        return;
    }
    let pinned_count = s.entries.iter().filter(|e| e.pinned).count();
    let mut unpinned_room = s.max_entries.saturating_sub(pinned_count);
    // entries is newest-first, so iterating in order naturally keeps recent
    // unpinned items and drops older ones.
    let mut kept: Vec<ClipEntry> = Vec::with_capacity(s.entries.len().min(s.max_entries));
    for e in s.entries.drain(..) {
        if e.pinned {
            kept.push(e);
        } else if unpinned_room > 0 {
            kept.push(e);
            unpinned_room -= 1;
        }
    }
    kept.sort_by(|a, b| b.ts.cmp(&a.ts));
    s.entries = kept;
}

pub fn spawn_poller(app: AppHandle, handle: Arc<ClipboardHandle>) {
    // Seed from disk so history survives restarts.
    let (loaded, max_entries) = load_persisted(&app);
    {
        let mut s = handle.state.lock().unwrap();
        s.entries = loaded;
        if let Some(m) = max_entries {
            s.max_entries = m.clamp(10, 2000);
        }
        // Seed last_text with whatever the system clipboard currently holds
        // so the first poll doesn't re-add what we already had.
        s.last_text = app.clipboard().read_text().ok();
    }

    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

        if !handle.capture_enabled.load(Ordering::Relaxed) {
            continue;
        }

        let Ok(text) = app.clipboard().read_text() else {
            continue;
        };

        // Hot path: clipboard unchanged. Avoid taking the lock for nothing.
        {
            let s = handle.state.lock().unwrap();
            if s.last_text.as_deref() == Some(text.as_str()) {
                continue;
            }
        }

        let mut s = handle.state.lock().unwrap();
        s.last_text = Some(text.clone());

        // Skip our own programmatic writes (user clicked "reuse").
        if s.ignore_text.as_deref() == Some(text.as_str()) {
            s.ignore_text = None;
            continue;
        }

        if looks_sensitive(&text) {
            continue;
        }

        let id = hash_text(&text);
        let new_entry = ClipEntry {
            id: id.clone(),
            preview: make_preview(&text),
            text,
            ts: now_ms(),
            pinned: false,
            note: None,
        };

        // Deduplicate: if the same content is already in history, move it to
        // top and keep its pin state. Otherwise insert as new.
        let emitted = if let Some(pos) = s.entries.iter().position(|e| e.id == id) {
            let existing = s.entries.remove(pos);
            let merged = ClipEntry {
                ts: new_entry.ts,
                pinned: existing.pinned,
                note: existing.note,
                ..new_entry
            };
            s.entries.insert(0, merged.clone());
            merged
        } else {
            s.entries.insert(0, new_entry.clone());
            cap_entries(&mut s);
            new_entry
        };

        let (entries, max) = (s.entries.clone(), s.max_entries);
        drop(s);

        let _ = app.emit("clipboard:new", &emitted);
        save_persisted(&app, &entries, max);
    });
}

#[tauri::command]
pub fn get_clipboard_history(handle: tauri::State<'_, Arc<ClipboardHandle>>) -> Vec<ClipEntry> {
    handle.state.lock().unwrap().entries.clone()
}

#[tauri::command]
pub fn set_clipboard_text(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    text: String,
) -> Result<(), String> {
    // Mark this write as ours BEFORE touching the system clipboard so the
    // poller's race window (poll fires between set and write) still skips it.
    let (snapshot_entries, max) = {
        let mut s = handle.state.lock().unwrap();
        s.ignore_text = Some(text.clone());
        // Reuse → float to top.
        if let Some(pos) = s.entries.iter().position(|e| e.text == text) {
            let mut entry = s.entries.remove(pos);
            entry.ts = now_ms();
            s.entries.insert(0, entry);
        }
        (s.entries.clone(), s.max_entries)
    };
    app.clipboard()
        .write_text(text)
        .map_err(|e| e.to_string())?;
    save_persisted(&app, &snapshot_entries, max);
    Ok(())
}

#[tauri::command]
pub fn delete_clipboard_entry(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    id: String,
) -> Result<(), String> {
    {
        let mut s = handle.state.lock().unwrap();
        s.entries.retain(|e| e.id != id);
    }
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    Ok(())
}

/// Attach (or clear) a user-authored note to a specific entry.
/// `None` / empty string clears the note. We trim leading/trailing whitespace
/// and treat purely whitespace input as "clear".
#[tauri::command]
pub fn set_clipboard_entry_note(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    id: String,
    note: Option<String>,
) -> Result<(), String> {
    let normalized = note.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });
    {
        let mut s = handle.state.lock().unwrap();
        if let Some(e) = s.entries.iter_mut().find(|e| e.id == id) {
            e.note = normalized;
        }
    }
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    Ok(())
}

#[tauri::command]
pub fn toggle_clipboard_pin(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    id: String,
) -> Result<(), String> {
    {
        let mut s = handle.state.lock().unwrap();
        if let Some(e) = s.entries.iter_mut().find(|e| e.id == id) {
            e.pinned = !e.pinned;
        }
    }
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    Ok(())
}

#[tauri::command]
pub fn clear_clipboard_history(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
) -> Result<(), String> {
    {
        let mut s = handle.state.lock().unwrap();
        s.entries.retain(|e| e.pinned);
    }
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    let _ = app.emit("clipboard:cleared", ());
    Ok(())
}

/// Wipe every entry, pinned or not. Used by the explicit "delete all" action.
#[tauri::command]
pub fn wipe_clipboard_history(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
) -> Result<(), String> {
    {
        let mut s = handle.state.lock().unwrap();
        s.entries.clear();
    }
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    let _ = app.emit("clipboard:cleared", ());
    Ok(())
}

#[tauri::command]
pub fn set_clipboard_capture(
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    enabled: bool,
) {
    handle.capture_enabled.store(enabled, Ordering::Relaxed);
}

#[tauri::command]
pub fn get_clipboard_capture(handle: tauri::State<'_, Arc<ClipboardHandle>>) -> bool {
    handle.capture_enabled.load(Ordering::Relaxed)
}

#[tauri::command]
pub fn set_clipboard_max_entries(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    max: usize,
) -> Result<(), String> {
    {
        let mut s = handle.state.lock().unwrap();
        s.max_entries = max.clamp(10, 2000);
        cap_entries(&mut s);
    }
    let (entries, final_max) = snapshot(&handle);
    save_persisted(&app, &entries, final_max);
    Ok(())
}
