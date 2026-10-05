// Background clipboard history capture, plus user-written memos.
//
// macOS doesn't expose system-wide clipboard history to third-party apps, so
// we poll the system pasteboard every POLL_INTERVAL_MS and build our own
// ring buffer. Entries are deduplicated by content hash, capped at
// `max_entries`, and persisted as JSON under the app data dir so history
// survives restarts.
//
// Memos share the same list and store but are authored in the app rather than
// captured. They get a random id (their text is editable, so a content hash
// would change under them) and, like pinned clips, are never auto-removed.

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

#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Captured from the system clipboard.
    #[default]
    Clip,
    /// Written by the user inside the app.
    Memo,
}

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
    /// `#[serde(default)]` so stores written before memos existed load every
    /// entry as a clip.
    #[serde(default)]
    pub kind: EntryKind,
}

impl ClipEntry {
    /// Pinned clips and memos survive the max-entries cap and "clear unpinned".
    fn survives_auto_removal(&self) -> bool {
        self.pinned || self.kind == EntryKind::Memo
    }
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
    format!("{truncated}…")
}

/// Drop the obviously-sensitive things (OTPs, key material, blanks). Kept
/// intentionally conservative — false positives mean the user loses an
/// entry they'd have wanted. Passwords are not guessed at here: password
/// managers mark them on the pasteboard, and `read_pasteboard` honours that.
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

/// Pasteboard types an app adds to say its contents must not be kept in a
/// clipboard history, from the nspasteboard.org convention: a password or
/// other secret (password managers such as 1Password and Enpass write
/// `ConcealedType`), text put there for a moment by a text expander, text the
/// user never copied, and the older per-app markers the convention replaced.
const UNRECORDED_PASTEBOARD_TYPES: &[&str] = &[
    "org.nspasteboard.ConcealedType",
    "org.nspasteboard.TransientType",
    "org.nspasteboard.AutoGeneratedType",
    "com.agilebits.onepassword",
    "de.petermaurer.TransientPasteboardType",
    "com.typeit4me.clipping",
    "Pasteboard generator type",
];

fn marked_unrecorded<S: AsRef<str>>(types: &[S]) -> bool {
    types
        .iter()
        .any(|t| UNRECORDED_PASTEBOARD_TYPES.contains(&t.as_ref()))
}

/// The pasteboard's change counter and whether its contents are marked as not
/// to be recorded, read in one go.
#[cfg(target_os = "macos")]
fn pasteboard_marks_of(pasteboard: &objc2_app_kit::NSPasteboard) -> (isize, bool) {
    let count = pasteboard.changeCount();
    let types: Vec<String> = pasteboard
        .types()
        .map(|types| types.to_vec().iter().map(|t| t.to_string()).collect())
        .unwrap_or_default();
    (count, marked_unrecorded(&types))
}

/// The poller runs on its own thread, which has no autorelease pool of its
/// own; without one every AppKit object read here would leak.
#[cfg(target_os = "macos")]
fn pasteboard_marks() -> Option<(isize, bool)> {
    objc2::rc::autoreleasepool(|_| {
        Some(pasteboard_marks_of(&objc2_app_kit::NSPasteboard::generalPasteboard()))
    })
}

#[cfg(target_os = "macos")]
fn pasteboard_change_count() -> Option<isize> {
    objc2::rc::autoreleasepool(|_| {
        Some(objc2_app_kit::NSPasteboard::generalPasteboard().changeCount())
    })
}

/// Windows has no nspasteboard markers to read, and nothing here is marked.
#[cfg(not(target_os = "macos"))]
fn pasteboard_marks() -> Option<(isize, bool)> {
    None
}

#[cfg(not(target_os = "macos"))]
fn pasteboard_change_count() -> Option<isize> {
    None
}

/// What the system pasteboard holds, as far as history is concerned.
enum Pasteboard {
    /// Its owner marked it as not to be recorded. The text was never read.
    Unrecorded,
    Text(String),
    /// Empty, not text, or replaced while it was being read.
    Unreadable,
}

/// Check the inexpensive native revision before allocating/copying clipboard
/// text. Failed or raced reads are retried; platforms without a counter keep
/// their existing text-based polling behavior.
fn poll_pasteboard(
    last_change_count: &mut Option<isize>,
    change_count: Option<isize>,
    read: impl FnOnce() -> Pasteboard,
) -> Option<Pasteboard> {
    if change_count.is_some() && change_count == *last_change_count {
        return None;
    }
    let contents = read();
    if !matches!(contents, Pasteboard::Unreadable) {
        *last_change_count = change_count;
    }
    Some(contents)
}

/// Read the pasteboard's text unless it is marked as not to be recorded.
///
/// The marks are checked first, so a copied password is never even read into
/// this process. The change counter is checked again after the read: if the
/// pasteboard changed in between, the text read may belong to contents whose
/// marks were never seen, so it is dropped and the next poll looks again.
fn read_pasteboard(app: &AppHandle) -> Pasteboard {
    let marks = pasteboard_marks();
    if marks.is_some_and(|(_, unrecorded)| unrecorded) {
        return Pasteboard::Unrecorded;
    }
    let Ok(text) = app.clipboard().read_text() else {
        return Pasteboard::Unreadable;
    };
    if let Some((count, _)) = marks {
        if pasteboard_change_count() != Some(count) {
            return Pasteboard::Unreadable;
        }
    }
    Pasteboard::Text(text)
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

/// Enforce the max-entries cap while always keeping pinned items and memos.
/// Those don't count against the cap (so a user who pinned 200 things doesn't
/// lose them when the cap is 100 — they just won't get new unpinned slots
/// until they unpin some).
fn cap_entries(s: &mut ClipboardState) {
    if s.entries.len() <= s.max_entries {
        return;
    }
    let kept_count = s.entries.iter().filter(|e| e.survives_auto_removal()).count();
    let mut unpinned_room = s.max_entries.saturating_sub(kept_count);
    // entries is newest-first, so iterating in order naturally keeps recent
    // unpinned items and drops older ones.
    let mut kept: Vec<ClipEntry> = Vec::with_capacity(s.entries.len().min(s.max_entries));
    for e in s.entries.drain(..) {
        if e.survives_auto_removal() {
            kept.push(e);
        } else if unpinned_room > 0 {
            kept.push(e);
            unpinned_room -= 1;
        }
    }
    kept.sort_by_key(|e| std::cmp::Reverse(e.ts));
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
        // so the first poll doesn't re-add what we already had. A marked
        // secret is not read even for this.
        s.last_text = match read_pasteboard(&app) {
            Pasteboard::Text(text) => Some(text),
            Pasteboard::Unrecorded | Pasteboard::Unreadable => None,
        };
    }
    let mut last_change_count = None;

    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

        if !handle.capture_enabled.load(Ordering::Relaxed) {
            continue;
        }

        let Some(contents) = poll_pasteboard(
            &mut last_change_count,
            pasteboard_change_count(),
            || read_pasteboard(&app),
        ) else {
            continue;
        };
        let text = match contents {
            Pasteboard::Text(text) => text,
            Pasteboard::Unrecorded => {
                // Forget the last text seen, so a clip copied over the secret
                // is recorded even when it repeats the one before it.
                handle.state.lock().unwrap().last_text = None;
                continue;
            }
            Pasteboard::Unreadable => continue,
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
            kind: EntryKind::Clip,
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

/// Build a fresh memo. Blank text (after trimming) is not a memo.
fn new_memo(text: &str, ts: i64) -> Option<ClipEntry> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(ClipEntry {
        id: format!("memo-{}", uuid::Uuid::new_v4()),
        preview: make_preview(text),
        text: text.to_string(),
        ts,
        pinned: false,
        note: None,
        kind: EntryKind::Memo,
    })
}

/// Replace a memo's text and float it to the top. Only memos are editable: a
/// clip's id is the hash of its text, so rewriting it would break dedup.
fn edit_memo(entries: &mut Vec<ClipEntry>, id: &str, text: &str, ts: i64) -> Result<ClipEntry, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("memo text is empty".into());
    }
    let pos = entries
        .iter()
        .position(|e| e.id == id && e.kind == EntryKind::Memo)
        .ok_or_else(|| format!("no memo with id {id}"))?;
    let mut memo = entries.remove(pos);
    memo.text = text.to_string();
    memo.preview = make_preview(text);
    memo.ts = ts;
    entries.insert(0, memo.clone());
    Ok(memo)
}

#[tauri::command]
pub fn create_clipboard_memo(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    text: String,
) -> Result<ClipEntry, String> {
    let memo = new_memo(&text, now_ms()).ok_or("memo text is empty")?;
    {
        let mut s = handle.state.lock().unwrap();
        s.entries.insert(0, memo.clone());
    }
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    let _ = app.emit("clipboard:new", &memo);
    Ok(memo)
}

#[tauri::command]
pub fn update_clipboard_memo(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    id: String,
    text: String,
) -> Result<ClipEntry, String> {
    let memo = {
        let mut s = handle.state.lock().unwrap();
        edit_memo(&mut s.entries, &id, &text, now_ms())?
    };
    let (entries, max) = snapshot(&handle);
    save_persisted(&app, &entries, max);
    let _ = app.emit("clipboard:new", &memo);
    Ok(memo)
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
        s.entries.retain(ClipEntry::survives_auto_removal);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_pasteboard_is_not_read_or_copied_again() {
        let mut revision = None;
        let large_text = "x".repeat(1024 * 1024);
        assert!(matches!(
            poll_pasteboard(&mut revision, Some(10), || Pasteboard::Text(large_text)),
            Some(Pasteboard::Text(_))
        ));
        for _ in 0..100 {
            assert!(poll_pasteboard(&mut revision, Some(10), || {
                panic!("unchanged clipboard must not be read")
            }).is_none());
        }
        assert!(matches!(
            poll_pasteboard(&mut revision, Some(11), || Pasteboard::Text("new copy".into())),
            Some(Pasteboard::Text(text)) if text == "new copy"
        ));
    }

    #[test]
    fn failed_or_raced_pasteboard_reads_retry_the_same_revision() {
        let mut revision = Some(10);
        assert!(matches!(
            poll_pasteboard(&mut revision, Some(11), || Pasteboard::Unreadable),
            Some(Pasteboard::Unreadable)
        ));
        assert_eq!(revision, Some(10));
        assert!(matches!(
            poll_pasteboard(&mut revision, Some(11), || Pasteboard::Text("retry".into())),
            Some(Pasteboard::Text(text)) if text == "retry"
        ));
    }

    #[test]
    fn unrecorded_revision_is_skipped_until_the_next_copy() {
        let mut revision = None;
        assert!(matches!(
            poll_pasteboard(&mut revision, Some(1), || Pasteboard::Unrecorded),
            Some(Pasteboard::Unrecorded)
        ));
        assert!(poll_pasteboard(&mut revision, Some(1), || {
            panic!("marked unchanged contents must not be read")
        }).is_none());
        assert!(matches!(
            poll_pasteboard(&mut revision, Some(2), || Pasteboard::Text("ordinary".into())),
            Some(Pasteboard::Text(text)) if text == "ordinary"
        ));
    }

    #[test]
    fn platforms_without_a_revision_keep_reading_each_poll() {
        let mut revision = None;
        let mut reads = 0;
        for _ in 0..3 {
            assert!(matches!(
                poll_pasteboard(&mut revision, None, || {
                    reads += 1;
                    Pasteboard::Text("copy".into())
                }),
                Some(Pasteboard::Text(_))
            ));
        }
        assert_eq!(reads, 3);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Needs a macOS pasteboard server; uses a private pasteboard only"]
    fn live_idle_polling_reads_large_private_pasteboard_once() {
        use objc2_app_kit::NSPasteboard;
        use objc2_foundation::NSString;

        objc2::rc::autoreleasepool(|_| {
            let pasteboard = NSPasteboard::pasteboardWithUniqueName();
            let text_type = NSString::from_str("public.utf8-plain-text");
            let payload = "x".repeat(1024 * 1024);
            pasteboard.clearContents();
            assert!(pasteboard.setString_forType(&NSString::from_str(&payload), &text_type));
            let read = || objc2::rc::autoreleasepool(|_| {
                pasteboard.stringForType(&text_type).unwrap().to_string()
            });

            let before = std::time::Instant::now();
            let mut baseline_bytes = 0;
            for _ in 0..100 {
                baseline_bytes += read().len();
            }
            let baseline_elapsed = before.elapsed();
            let before = std::time::Instant::now();
            let mut revision = None;
            let mut optimized_bytes = 0;
            let mut reads = 0;
            for _ in 0..100 {
                let result = poll_pasteboard(&mut revision, Some(pasteboard.changeCount()), || {
                    reads += 1;
                    Pasteboard::Text(read())
                });
                if let Some(Pasteboard::Text(text)) = result {
                    optimized_bytes += text.len();
                }
            }
            let optimized_elapsed = before.elapsed();
            assert_eq!(reads, 1);
            assert_eq!(baseline_bytes, 100 * payload.len());
            assert_eq!(optimized_bytes, payload.len());
            eprintln!("private pasteboard benchmark: baseline 100 reads/{baseline_bytes} bytes/{baseline_elapsed:?}; optimized {reads} read/{optimized_bytes} bytes/{optimized_elapsed:?}");
            let _: () = unsafe { objc2::msg_send![&*pasteboard, releaseGlobally] };
        });
    }

    fn clip(text: &str, ts: i64, pinned: bool) -> ClipEntry {
        ClipEntry {
            id: hash_text(text),
            text: text.into(),
            preview: make_preview(text),
            ts,
            pinned,
            note: None,
            kind: EntryKind::Clip,
        }
    }

    #[test]
    fn stores_written_before_memos_load_entries_as_clips() {
        let raw = r#"{"entries":[{"id":"x","text":"hi","preview":"hi","ts":1,"pinned":false}]}"#;
        let parsed: PersistedState = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.entries[0].kind, EntryKind::Clip);
        assert_eq!(parsed.entries[0].note, None);
    }

    #[test]
    fn marked_contents_are_not_recorded() {
        // What 1Password puts on the pasteboard next to a copied password.
        assert!(marked_unrecorded(&[
            "public.utf8-plain-text",
            "org.nspasteboard.ConcealedType",
            "com.agilebits.onepassword",
        ]));
        // A text expander's momentary paste.
        assert!(marked_unrecorded(&["public.utf8-plain-text", "org.nspasteboard.TransientType"]));
        assert!(marked_unrecorded(&["NSStringPboardType", "de.petermaurer.TransientPasteboardType"]));
    }

    #[test]
    fn ordinary_and_handoff_copies_are_recorded() {
        assert!(!marked_unrecorded(&["public.utf8-plain-text", "NSStringPboardType"]));
        // Handoff marks where a copy came from, not that it is private: the
        // user copied it on their other device.
        assert!(!marked_unrecorded(&["com.apple.is-remote-clipboard", "public.utf8-plain-text"]));
        assert!(!marked_unrecorded::<&str>(&[]));
    }

    /// The real AppKit path, on a private pasteboard so the user's clipboard
    /// is never touched: a password manager's marks are seen, and a plain
    /// copy is left alone.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "Needs a macOS pasteboard server; run with -- --ignored"]
    fn live_marks_are_read_off_a_real_pasteboard() {
        use objc2_app_kit::NSPasteboard;
        use objc2_foundation::NSString;

        objc2::rc::autoreleasepool(|_| {
            let pasteboard = NSPasteboard::pasteboardWithUniqueName();
            let text = NSString::from_str("public.utf8-plain-text");
            let concealed = NSString::from_str("org.nspasteboard.ConcealedType");

            pasteboard.clearContents();
            assert!(pasteboard.setString_forType(&NSString::from_str("hunter2"), &text));
            let (plain_count, plain) = pasteboard_marks_of(&pasteboard);
            assert!(!plain);

            pasteboard.clearContents();
            assert!(pasteboard.setString_forType(&NSString::from_str("hunter2"), &text));
            assert!(pasteboard.setString_forType(&NSString::from_str(""), &concealed));
            let (marked_count, marked) = pasteboard_marks_of(&pasteboard);
            assert!(marked);
            // The counter moves with every change, which is what catches a
            // copy that lands between the check and the read.
            assert_ne!(plain_count, marked_count);

            // objc2 does not bind releaseGlobally; without it the named
            // pasteboard would outlive the test in the pasteboard server.
            let _: () = unsafe { objc2::msg_send![&*pasteboard, releaseGlobally] };
        });
    }

    #[test]
    fn memo_kind_round_trips_as_lowercase() {
        let memo = new_memo("buy milk", 5).unwrap();
        let json = serde_json::to_string(&memo).unwrap();
        assert!(json.contains(r#""kind":"memo""#), "{json}");
        let back: ClipEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, EntryKind::Memo);
    }

    #[test]
    fn new_memo_trims_and_rejects_blank_text() {
        assert!(new_memo("   \n\t", 1).is_none());
        let memo = new_memo("  line one\nline two \n", 7).unwrap();
        assert_eq!(memo.text, "line one\nline two");
        assert_eq!(memo.ts, 7);
        assert!(memo.id.starts_with("memo-"));
        assert_ne!(memo.id, new_memo("line one\nline two", 7).unwrap().id);
    }

    #[test]
    fn edit_memo_rewrites_text_keeps_id_and_floats_to_top() {
        let memo = new_memo("draft", 1).unwrap();
        let id = memo.id.clone();
        let mut entries = vec![clip("newer clip", 3, false), memo];

        let edited = edit_memo(&mut entries, &id, " final ", 9).unwrap();
        assert_eq!(edited.id, id);
        assert_eq!(edited.text, "final");
        assert_eq!(edited.preview, "final");
        assert_eq!(edited.ts, 9);
        assert_eq!(entries[0].id, id);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn edit_memo_refuses_blank_text_and_clips() {
        let memo = new_memo("keep me", 1).unwrap();
        let memo_id = memo.id.clone();
        let c = clip("a clip", 2, false);
        let clip_id = c.id.clone();
        let mut entries = vec![c, memo];

        assert!(edit_memo(&mut entries, &memo_id, "  ", 5).is_err());
        assert!(edit_memo(&mut entries, &clip_id, "rewrite", 5).is_err());
        assert!(edit_memo(&mut entries, "missing", "x", 5).is_err());
        assert_eq!(entries[1].text, "keep me");
        assert_eq!(entries[0].text, "a clip");
    }

    #[test]
    fn cap_keeps_memos_and_pins_outside_the_limit() {
        let mut s = ClipboardState {
            entries: vec![
                clip("c5", 50, false),
                new_memo("memo", 45).unwrap(),
                clip("c4", 40, false),
                clip("pinned", 35, true),
                clip("c3", 30, false),
                clip("c2", 20, false),
            ],
            max_entries: 3,
            last_text: None,
            ignore_text: None,
        };
        cap_entries(&mut s);
        let texts: Vec<&str> = s.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["c5", "memo", "pinned"]);
    }

    #[test]
    fn clearing_unpinned_keeps_memos() {
        let mut entries = vec![
            clip("loose", 3, false),
            new_memo("note to self", 2).unwrap(),
            clip("pinned", 1, true),
        ];
        entries.retain(ClipEntry::survives_auto_removal);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["note to self", "pinned"]);
    }
}
