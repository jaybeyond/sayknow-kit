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

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::sync::memo_bridge::MemoSink;

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

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
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
    /// Set on a memo that sync kept as a conflict copy: the memo it split
    /// from. Absent in files written before sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_of: Option<String>,
}

impl ClipEntry {
    /// Pinned clips and memos survive the max-entries cap and "clear unpinned".
    fn survives_auto_removal(&self) -> bool {
        self.pinned || self.kind == EntryKind::Memo
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct PersistedState {
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
    /// Set when `clipboard.json` could be neither read nor preserved: every
    /// save is skipped so a file we could not keep is never overwritten.
    pub persist_blocked: AtomicBool,
    /// Where memo changes go once sync is on; `None` until G7 installs it.
    pub(crate) memo_sink: Mutex<Option<Arc<dyn MemoSink>>>,
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
            persist_blocked: AtomicBool::new(false),
            memo_sink: Mutex::new(None),
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

pub(crate) fn make_preview(s: &str) -> String {
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
/// A sync recovery key is never recorded, whatever wrote it (U1): it is
/// matched by its checksum, so ordinary text cannot match by accident.
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
    crate::sync::recovery::is_recovery_key(trimmed)
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

pub(crate) fn store_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("clipboard.json"))
}

/// Writes `bytes` to `path` atomically: a sibling tmp file
/// (`<name>.tmp-<pid>-<n>`) is written and `sync_all`ed, then renamed over
/// the target; on unix the parent directory is fsynced too. If the write,
/// fsync or rename fails, the target is untouched and the tmp file is removed
/// best-effort (a stray one may remain if that removal fails too). If only
/// the parent fsync fails after a successful rename, the call returns `Err`
/// although the new bytes are already in place (durability across power loss
/// is then not guaranteed); callers must not treat that `Err` as "old bytes
/// kept".
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, bytes, |file, bytes| file.write_all(bytes))
}

/// [`write_atomic`] with the tmp-file write injectable, so a failure in the
/// middle of it can be simulated.
fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{name}.tmp-{}-{n}", std::process::id()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let staged = write(&mut file, bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = staged.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// What loading `clipboard.json` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoadOutcome {
    /// No file yet: start empty, saves allowed.
    Missing,
    Loaded,
    /// The file did not parse; its bytes were moved to `preserved` and the
    /// history starts empty.
    Corrupt { preserved: PathBuf },
    /// The file could not be read, or a corrupt file could not be moved
    /// aside: start empty and block every save.
    Unreadable,
}

#[derive(Debug)]
pub(crate) struct LoadResult {
    pub(crate) entries: Vec<ClipEntry>,
    pub(crate) max_entries: Option<usize>,
    pub(crate) outcome: LoadOutcome,
}

impl LoadResult {
    pub(crate) fn persist_blocked(&self) -> bool {
        self.outcome == LoadOutcome::Unreadable
    }

    fn empty(outcome: LoadOutcome) -> Self {
        LoadResult { entries: Vec::new(), max_entries: None, outcome }
    }
}

/// Loads `clipboard.json` at `path`. A corrupt file is renamed to
/// `<name>.corrupt-<ts_ms>` (never overwritten); see [`LoadOutcome`].
pub(crate) fn load_from(path: &Path) -> LoadResult {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LoadResult::empty(LoadOutcome::Missing),
        Err(e) => {
            log::warn!("clipboard history unreadable at {}: {e}; saves are blocked", path.display());
            return LoadResult::empty(LoadOutcome::Unreadable);
        }
    };
    match serde_json::from_slice::<PersistedState>(&raw) {
        Ok(parsed) => LoadResult { entries: parsed.entries, max_entries: parsed.max_entries, outcome: LoadOutcome::Loaded },
        Err(parse) => match preserve_corrupt(path, now_ms()) {
            Ok(preserved) => {
                log::warn!("clipboard history corrupt ({parse}); kept as {}", preserved.display());
                LoadResult::empty(LoadOutcome::Corrupt { preserved })
            }
            Err(e) => {
                log::warn!("clipboard history corrupt ({parse}) and not preserved ({e}); saves are blocked");
                LoadResult::empty(LoadOutcome::Unreadable)
            }
        },
    }
}

/// Renames `path` to `<name>.corrupt-<ts>` (with `-<k>` appended if that
/// name is taken); an existing file is never replaced.
fn preserve_corrupt(path: &Path, ts: i64) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    for k in 0..100u32 {
        let suffix = if k == 0 { String::new() } else { format!("-{k}") };
        let target = path.with_file_name(format!("{name}.corrupt-{ts}{suffix}"));
        if target.symlink_metadata().is_ok() {
            continue;
        }
        fs::rename(path, &target)?;
        return Ok(target);
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free .corrupt name"))
}

/// Saves the history to `path` atomically. While `blocked` it writes
/// nothing and returns `Ok(false)`; `Ok(true)` means written.
pub(crate) fn save_to(path: &Path, entries: &[ClipEntry], max_entries: usize, blocked: bool) -> io::Result<bool> {
    if blocked {
        log::warn!("clipboard history not saved: the existing file could not be read or preserved");
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let state = PersistedState { entries: entries.to_vec(), max_entries: Some(max_entries) };
    let json = serde_json::to_vec(&state).map_err(io::Error::other)?;
    write_atomic(path, &json)?;
    Ok(true)
}

fn load_persisted(app: &AppHandle) -> LoadResult {
    match store_path(app) {
        Some(path) => load_from(&path),
        None => LoadResult::empty(LoadOutcome::Missing),
    }
}

/// Logs failures; never panics.
pub(crate) fn save_persisted(app: &AppHandle, handle: &ClipboardHandle, entries: &[ClipEntry], max_entries: usize) {
    let Some(path) = store_path(app) else { return };
    let blocked = handle.persist_blocked.load(Ordering::Relaxed);
    if let Err(e) = save_to(&path, entries, max_entries, blocked) {
        log::warn!("clipboard history save failed: {e}");
    }
}

fn snapshot(handle: &ClipboardHandle) -> (Vec<ClipEntry>, usize) {
    let s = handle.state.lock().unwrap();
    (s.entries.clone(), s.max_entries)
}

/// What a command did to a copy of the entries: the entry before the change
/// and the index of the entry after it (`None` = nothing / deleted).
type EntryChange = (Option<ClipEntry>, Option<usize>);

/// One entry command, store-first (G4 plan amendments): under the clipboard
/// lock, `change` edits a copy of the entries; when a memo is involved and a
/// sink is installed, the sink commits first and on error nothing changes
/// (no mutation, no save). Then the entries are replaced, the changed entry
/// takes the entity id the sink returned, and `save` runs under the same
/// lock. Returns the entry after the change; the caller emits (see
/// [`rebound`] for a changed id).
pub(crate) fn change_entries(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    change: impl FnOnce(&mut Vec<ClipEntry>) -> Result<EntryChange, String>,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<Option<ClipEntry>, String> {
    let mut s = handle.state.lock().unwrap();
    let mut next = s.entries.clone();
    let (before, slot) = change(&mut next)?;
    if let Some(sink) = sink {
        let after = slot.map(|i| &next[i]);
        let is_memo = |e: Option<&ClipEntry>| e.is_some_and(|e| e.kind == EntryKind::Memo);
        if is_memo(before.as_ref()) || is_memo(after) {
            if let (Some(id), Some(i)) = (sink.changed(before.as_ref(), after)?, slot) {
                next[i].id = id;
            }
        }
    }
    s.entries = next;
    save(&s.entries, s.max_entries);
    Ok(slot.map(|i| s.entries[i].clone()))
}

/// Whether a command on `id` left its entry under another id (the sink
/// remapped or recreated the memo). The webview still holds `id`, so the
/// command must emit `clipboard:cleared` (a full refetch) instead of an
/// upsert of the new id alone (G4 delta rule AD-B1).
pub(crate) fn rebound(id: &str, after: Option<&ClipEntry>) -> bool {
    after.is_some_and(|e| e.id != id)
}

/// With a sink installed, a command on an id the clipboard does not hold is
/// an error: the webview drifted (e.g. it missed a rebind), and a silent
/// `Ok` would drop the user's change (AD-B1). Without a sink it stays a
/// no-op, as before sync.
fn unknown_id(sink: Option<&dyn MemoSink>, id: &str) -> Result<EntryChange, String> {
    match sink {
        Some(_) => Err(format!("no entry with id {id}")),
        None => Ok((None, None)),
    }
}

pub(crate) fn create_memo_in(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    text: &str,
    ts: i64,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<ClipEntry, String> {
    let memo = new_memo(text, ts).ok_or("memo text is empty")?;
    let change = |entries: &mut Vec<ClipEntry>| {
        entries.insert(0, memo);
        Ok((None, Some(0)))
    };
    change_entries(handle, sink, change, save)?.ok_or_else(|| "memo not created".to_owned())
}

pub(crate) fn update_memo_in(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    id: &str,
    text: &str,
    ts: i64,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<ClipEntry, String> {
    let change = |entries: &mut Vec<ClipEntry>| {
        let before = entries.iter().find(|e| e.id == id && e.kind == EntryKind::Memo).cloned();
        edit_memo(entries, id, text, ts)?;
        Ok((before, Some(0)))
    };
    change_entries(handle, sink, change, save)?.ok_or_else(|| format!("no memo with id {id}"))
}

/// Applies `edit` to the entry `id` in place, if there is one.
fn edit_entry_in(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    id: &str,
    edit: impl FnOnce(&mut ClipEntry),
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<Option<ClipEntry>, String> {
    let change = |entries: &mut Vec<ClipEntry>| match entries.iter().position(|e| e.id == id) {
        Some(i) => {
            let before = entries[i].clone();
            edit(&mut entries[i]);
            Ok((Some(before), Some(i)))
        }
        None => unknown_id(sink, id),
    };
    change_entries(handle, sink, change, save)
}

pub(crate) fn toggle_pin_in(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    id: &str,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<Option<ClipEntry>, String> {
    edit_entry_in(handle, sink, id, |e| e.pinned = !e.pinned, save)
}

pub(crate) fn set_note_in(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    id: &str,
    note: Option<String>,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<Option<ClipEntry>, String> {
    edit_entry_in(handle, sink, id, |e| e.note = note, save)
}

pub(crate) fn delete_entry_in(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    id: &str,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<(), String> {
    let change = |entries: &mut Vec<ClipEntry>| {
        let Some(pos) = entries.iter().position(|e| e.id == id) else {
            return unknown_id(sink, id);
        };
        let removed = entries.remove(pos);
        entries.retain(|e| e.id != id);
        Ok((Some(removed), None))
    };
    change_entries(handle, sink, change, save).map(|_| ())
}

/// Sync cutover (G7, AD-A2) as one critical section: the sink slot and the
/// state lock are held across `prepare` (snapshot of the live entries →
/// finalize / reconcile → mirror), the entry swap, `save`, and the sink
/// install. Every memo command holds the slot for its whole run, so none can
/// slip between the mirror and the install and commit only locally.
/// `prepare` returning `Ok(None)` changes nothing (no swap, save, or
/// install). Returns whether a sink was installed.
pub(crate) fn cutover_in<E>(
    handle: &ClipboardHandle,
    prepare: impl FnOnce(&[ClipEntry]) -> Result<Option<(Vec<ClipEntry>, Arc<dyn MemoSink>)>, E>,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<bool, E> {
    let mut slot = lock_recover(&handle.memo_sink);
    let mut s = lock_recover(&handle.state);
    let Some((entries, sink)) = prepare(&s.entries)? else {
        return Ok(false);
    };
    s.entries = entries;
    save(&s.entries, s.max_entries);
    *slot = Some(sink);
    Ok(true)
}

/// The sink slot, held for a whole memo command: a cutover (which holds it
/// too) can then never land between a command reading "no sink" and that
/// command's local-only commit. Lock order: slot → state → sync store.
fn installed_sink(handle: &ClipboardHandle) -> MutexGuard<'_, Option<Arc<dyn MemoSink>>> {
    lock_recover(&handle.memo_sink)
}

/// The sink slot and the state stay valid if a holder panicked (a poisoned
/// slot must not take every later memo command down with it, A11).
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Removes the sink (sync turned off at runtime); memo commands are
/// local-only again from the next command on.
pub(crate) fn uninstall_sink(handle: &ClipboardHandle) -> bool {
    lock_recover(&handle.memo_sink).take().is_some()
}

/// Remote apply (G4 §6, G7): under the state lock, `compute` maps the current
/// entries to the mirror (it may read the store; it must not call back into
/// clipboard); when that changes anything, the entries are replaced and
/// `save` runs under the same lock. Returns whether anything changed.
pub(crate) fn mirror_in<E>(
    handle: &ClipboardHandle,
    compute: impl FnOnce(&[ClipEntry]) -> Result<Vec<ClipEntry>, E>,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<bool, E> {
    let mut s = lock_recover(&handle.state);
    let next = compute(&s.entries)?;
    if next == s.entries {
        return Ok(false);
    }
    s.entries = next;
    save(&s.entries, s.max_entries);
    Ok(true)
}

/// `wipe_clipboard_history` steps 1–4 (§4.10.2, C-3): under the lock,
/// capture the memo ids, commit one `memo.delete` per id through the sink
/// (on error nothing changes), clear the entries, then save. The caller emits.
pub(crate) fn wipe_entries(
    handle: &ClipboardHandle,
    sink: Option<&dyn MemoSink>,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<(), String> {
    let mut s = handle.state.lock().unwrap();
    let ids: Vec<String> = s.entries.iter().filter(|e| e.kind == EntryKind::Memo).map(|e| e.id.clone()).collect();
    if let Some(sink) = sink {
        if !ids.is_empty() {
            sink.delete_all(&ids)?;
        }
    }
    s.entries.clear();
    // Saved under the lock, like every memo command (store-first, AD-A4).
    save(&[], s.max_entries);
    Ok(())
}

/// Replaces every memo in `entries` with `memos` (only `kind == Memo` items
/// of it are used); clips keep their positions. Memos present in both keep
/// their slot; new ones are prepended in the given order.
pub(crate) fn splice_memos(entries: &[ClipEntry], memos: Vec<ClipEntry>) -> Vec<ClipEntry> {
    let mut incoming: Vec<ClipEntry> = memos.into_iter().filter(|e| e.kind == EntryKind::Memo).collect();
    let mut kept = Vec::with_capacity(entries.len() + incoming.len());
    for e in entries {
        if e.kind != EntryKind::Memo {
            kept.push(e.clone());
        } else if let Some(pos) = incoming.iter().position(|m| m.id == e.id) {
            kept.push(incoming.remove(pos));
        }
    }
    incoming.extend(kept);
    incoming
}

/// Remote apply (G4 §6): every memo entry becomes the mirror list `memos`,
/// clips are kept. Returns the new entries and cap for the caller's atomic
/// save and emit (G7 wiring; unused outside tests until then).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn apply_memo_projection(handle: &ClipboardHandle, memos: Vec<ClipEntry>) -> (Vec<ClipEntry>, usize) {
    let mut s = handle.state.lock().unwrap();
    s.entries = splice_memos(&s.entries, memos);
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

/// Loads the history and starts polling. Returns what the load found, which
/// sync boot (G7) needs for the migration finalize decision.
pub(crate) fn spawn_poller(app: AppHandle, handle: Arc<ClipboardHandle>) -> LoadOutcome {
    // Seed from disk so history survives restarts.
    let loaded = load_persisted(&app);
    let outcome = loaded.outcome.clone();
    handle.persist_blocked.store(loaded.persist_blocked(), Ordering::Relaxed);
    {
        let mut s = handle.state.lock().unwrap();
        s.entries = loaded.entries;
        if let Some(m) = loaded.max_entries {
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
            conflict_of: None,
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
        save_persisted(&app, &handle, &entries, max);
    });
    outcome
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
    save_persisted(&app, &handle, &snapshot_entries, max);
    Ok(())
}

#[tauri::command]
pub fn delete_clipboard_entry(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    id: String,
) -> Result<(), String> {
    let sink = installed_sink(&handle);
    delete_entry_in(&handle, sink.as_deref(), &id, |entries, max| save_persisted(&app, &handle, entries, max))
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
    let sink = installed_sink(&handle);
    let after = set_note_in(&handle, sink.as_deref(), &id, normalized, |entries, max| {
        save_persisted(&app, &handle, entries, max)
    })?;
    if rebound(&id, after.as_ref()) {
        let _ = app.emit("clipboard:cleared", ());
    }
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
        conflict_of: None,
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
    let sink = installed_sink(&handle);
    let memo = create_memo_in(&handle, sink.as_deref(), &text, now_ms(), |entries, max| {
        save_persisted(&app, &handle, entries, max)
    })?;
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
    let sink = installed_sink(&handle);
    let memo = update_memo_in(&handle, sink.as_deref(), &id, &text, now_ms(), |entries, max| {
        save_persisted(&app, &handle, entries, max)
    })?;
    // A rebound id: a refetch drops the stale row; an upsert of the new id
    // alone would leave it next to the copy (AD-B1).
    if rebound(&id, Some(&memo)) {
        let _ = app.emit("clipboard:cleared", ());
    } else {
        let _ = app.emit("clipboard:new", &memo);
    }
    Ok(memo)
}

#[tauri::command]
pub fn toggle_clipboard_pin(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
    id: String,
) -> Result<(), String> {
    let sink = installed_sink(&handle);
    let after =
        toggle_pin_in(&handle, sink.as_deref(), &id, |entries, max| save_persisted(&app, &handle, entries, max))?;
    if rebound(&id, after.as_ref()) {
        let _ = app.emit("clipboard:cleared", ());
    }
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
    save_persisted(&app, &handle, &entries, max);
    let _ = app.emit("clipboard:cleared", ());
    Ok(())
}

/// Wipe every entry, pinned or not. Used by the explicit "delete all" action.
/// Memo deletes are committed to sync first; if that fails nothing changes.
#[tauri::command]
pub fn wipe_clipboard_history(
    app: AppHandle,
    handle: tauri::State<'_, Arc<ClipboardHandle>>,
) -> Result<(), String> {
    let sink = installed_sink(&handle);
    wipe_entries(&handle, sink.as_deref(), |entries, max| save_persisted(&app, &handle, entries, max))?;
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
    save_persisted(&app, &handle, &entries, final_max);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::recovery::RecoveryKey;

    #[test]
    fn looks_sensitive_drops_any_recovery_key_form() {
        let rk = RecoveryKey::generate().unwrap().to_display();
        let spaced = rk.to_lowercase().replace('-', " ");
        let dashless = rk.replace('-', "");
        let lines = format!("  {}\n", rk.replace('-', "\n"));
        for text in [rk.as_str(), &spaced, &dashless, &lines] {
            assert!(looks_sensitive(text), "recovery key form recorded: {text:?}");
        }
    }

    #[test]
    fn looks_sensitive_keeps_a_recovery_key_shape_with_a_bad_checksum() {
        let rk = RecoveryKey::generate().unwrap().to_display();
        let mut chars: Vec<char> = rk.chars().collect();
        chars[0] = if chars[0] == 'A' { 'C' } else { 'A' };
        let typo: String = chars.into_iter().collect();
        assert!(RecoveryKey::parse(&typo).is_err());
        assert!(!looks_sensitive(&typo), "a checksum failure is ordinary text");
        for text in [
            "ABCD-EFGH-IJKL-MNOP",
            "The quick brown fox jumps over the lazy dog, again and again and again.",
            &"Q".repeat(56),
        ] {
            assert!(!looks_sensitive(text), "{text:?}");
        }
        assert!(looks_sensitive("123456"), "existing OTP rule still applies");
    }

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
            conflict_of: None,
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

    /// A scratch dir under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sayknow-clipboard-{tag}-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    fn memo_entry(id: &str, text: &str, ts: i64) -> ClipEntry {
        ClipEntry { id: id.into(), ..new_memo(text, ts).unwrap() }
    }

    #[test]
    fn save_then_load_round_trips_and_missing_is_ok() {
        let dir = Scratch::new("roundtrip");
        let path = dir.0.join("nested").join("clipboard.json");
        let missing = load_from(&path);
        assert_eq!(missing.outcome, LoadOutcome::Missing);
        assert!(!missing.persist_blocked());
        let mut memo = memo_entry("memo-1", "m", 2);
        memo.conflict_of = Some("memo-0".into());
        let entries = vec![clip("a", 3, true), memo];
        assert!(save_to(&path, &entries, 42, false).unwrap());
        let loaded = load_from(&path);
        assert_eq!(loaded.outcome, LoadOutcome::Loaded);
        assert_eq!((loaded.entries, loaded.max_entries), (entries, Some(42)));
        assert_eq!(names(&dir.0.join("nested")), ["clipboard.json"], "no tmp file left behind");
    }

    #[test]
    fn conflict_of_is_optional_and_omitted_when_unset() {
        let json = serde_json::to_string(&clip("a", 1, false)).unwrap();
        assert!(!json.contains("conflict_of"), "{json}");
        let raw = r#"{"id":"m","text":"t","preview":"t","ts":1,"pinned":false,"kind":"memo","conflict_of":"o"}"#;
        let back: ClipEntry = serde_json::from_str(raw).unwrap();
        assert_eq!(back.conflict_of.as_deref(), Some("o"));
    }

    #[test]
    fn corrupt_file_is_preserved_byte_for_byte() {
        let dir = Scratch::new("corrupt");
        let path = dir.0.join("clipboard.json");
        let bytes = b"{\"entries\": [ {\"id\": \x00 not json \xff".to_vec();
        fs::write(&path, &bytes).unwrap();
        let loaded = load_from(&path);
        let LoadOutcome::Corrupt { preserved } = &loaded.outcome else { panic!("{:?}", loaded.outcome) };
        assert!(loaded.entries.is_empty());
        assert!(!loaded.persist_blocked());
        assert!(!path.exists(), "moved aside, not copied");
        let name = preserved.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("clipboard.json.corrupt-"), "{name}");
        assert_eq!(fs::read(preserved).unwrap(), bytes);

        // A later save writes a fresh file and never touches the preserved one.
        assert!(save_to(&path, &[clip("new", 1, false)], 100, loaded.persist_blocked()).unwrap());
        assert_eq!(fs::read(preserved).unwrap(), bytes);
        assert_eq!(load_from(&path).outcome, LoadOutcome::Loaded);

        // Parseable JSON of the wrong shape is corrupt too, and an existing
        // `.corrupt` name is never overwritten.
        fs::write(&path, b"[1,2,3]").unwrap();
        let ts = now_ms();
        fs::write(dir.0.join(format!("clipboard.json.corrupt-{ts}")), b"older").unwrap();
        let second = preserve_corrupt(&path, ts).unwrap();
        assert_eq!(second.file_name().unwrap().to_string_lossy(), format!("clipboard.json.corrupt-{ts}-1"));
        assert_eq!(fs::read(&second).unwrap(), b"[1,2,3]");
        assert_eq!(fs::read(dir.0.join(format!("clipboard.json.corrupt-{ts}"))).unwrap(), b"older");
    }

    #[test]
    fn unreadable_file_blocks_saves_and_keeps_its_bytes() {
        let dir = Scratch::new("unreadable");
        // A directory in the file's place cannot be read as a file.
        let path = dir.0.join("clipboard.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), b"keep").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.outcome, LoadOutcome::Unreadable);
        assert!(loaded.persist_blocked());
        assert!(!save_to(&path, &[clip("x", 1, false)], 100, true).unwrap(), "blocked save writes nothing");
        assert_eq!(fs::read(path.join("inside")).unwrap(), b"keep");
        assert_eq!(names(&dir.0), ["clipboard.json"], "no tmp file either");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = dir.0.join("locked.json");
            let bytes = br#"{"entries":[]}"#;
            fs::write(&file, bytes).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads through mode 000; the directory case above covers it.
            if fs::read(&file).is_err() {
                let loaded = load_from(&file);
                assert_eq!(loaded.outcome, LoadOutcome::Unreadable);
                assert!(!save_to(&file, &[], 100, loaded.persist_blocked()).unwrap());
            }
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(fs::read(&file).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_file_that_cannot_be_moved_aside_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new("corrupt-locked");
        let path = dir.0.join("clipboard.json");
        fs::write(&path, b"not json").unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = fs::write(dir.0.join("probe"), b"");
        let loaded = load_from(&path);
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        if probe.is_ok() {
            return; // Running as root: a read-only dir does not stop the rename.
        }
        assert_eq!(loaded.outcome, LoadOutcome::Unreadable);
        assert!(loaded.persist_blocked());
        assert_eq!(fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn atomic_write_failure_leaves_the_target_unchanged() {
        let dir = Scratch::new("atomic");
        let path = dir.0.join("clipboard.json");
        fs::write(&path, b"original").unwrap();
        let err = write_atomic_with(&path, b"replacement bytes", |file, bytes| {
            file.write_all(&bytes[..5])?;
            Err(io::Error::other("simulated crash mid-write"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "simulated crash mid-write");
        assert_eq!(fs::read(&path).unwrap(), b"original", "target unchanged");
        let tmp: Vec<String> = names(&dir.0).into_iter().filter(|n| n.starts_with("clipboard.json.tmp-")).collect();
        assert!(tmp.is_empty(), "the partial tmp file is removed: {tmp:?}");

        write_atomic(&path, b"replacement bytes").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement bytes");
    }

    #[derive(Default)]
    struct FakeSink {
        fail: bool,
        changed: Mutex<Vec<(Option<String>, Option<String>)>>,
        deleted: Mutex<Vec<Vec<String>>>,
        /// The entity id `changed` tells the entry to take.
        rebind: Option<String>,
    }

    impl MemoSink for FakeSink {
        fn changed(&self, before: Option<&ClipEntry>, after: Option<&ClipEntry>) -> Result<Option<String>, String> {
            if self.fail {
                return Err("sink down".into());
            }
            self.changed.lock().unwrap().push((before.map(|e| e.text.clone()), after.map(|e| e.text.clone())));
            Ok(self.rebind.clone())
        }

        fn delete_all(&self, ids: &[String]) -> Result<(), String> {
            if self.fail {
                return Err("sink down".into());
            }
            self.deleted.lock().unwrap().push(ids.to_vec());
            Ok(())
        }
    }

    fn handle_with(entries: Vec<ClipEntry>) -> Arc<ClipboardHandle> {
        let handle = ClipboardHandle::new();
        handle.state.lock().unwrap().entries = entries;
        handle
    }

    fn sample() -> Vec<ClipEntry> {
        vec![clip("c1", 5, false), memo_entry("memo-1", "m1", 4), clip("pinned", 3, true), memo_entry("memo-2", "m2", 2)]
    }

    #[test]
    fn wipe_commits_memo_deletes_before_clearing_and_saving() {
        let handle = handle_with(sample());
        let sink = FakeSink::default();
        let mut saved = None;
        wipe_entries(&handle, Some(&sink), |entries, max| saved = Some((entries.to_vec(), max))).unwrap();
        assert_eq!(*sink.deleted.lock().unwrap(), vec![vec!["memo-1".to_owned(), "memo-2".to_owned()]]);
        assert!(handle.state.lock().unwrap().entries.is_empty());
        assert_eq!(saved, Some((vec![], DEFAULT_MAX_ENTRIES)));

        // Without memos the sink is not called; without a sink the wipe still runs.
        let handle = handle_with(vec![clip("c", 1, false)]);
        let sink = FakeSink::default();
        wipe_entries(&handle, Some(&sink), |_, _| {}).unwrap();
        assert!(sink.deleted.lock().unwrap().is_empty());
        let handle = handle_with(sample());
        wipe_entries(&handle, None, |_, _| {}).unwrap();
        assert!(handle.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn wipe_sink_failure_changes_nothing() {
        let handle = handle_with(sample());
        let sink = FakeSink { fail: true, ..FakeSink::default() };
        let mut saves = 0;
        let err = wipe_entries(&handle, Some(&sink), |_, _| saves += 1).unwrap_err();
        assert_eq!(err, "sink down");
        assert_eq!(saves, 0, "no file write");
        assert_eq!(handle.state.lock().unwrap().entries, sample(), "no entry cleared");
        assert!(sink.deleted.lock().unwrap().is_empty(), "0 ops");
    }

    #[test]
    fn wipe_save_goes_through_the_atomic_file() {
        let dir = Scratch::new("wipe");
        let path = dir.0.join("clipboard.json");
        save_to(&path, &sample(), 100, false).unwrap();
        let handle = handle_with(sample());
        let sink = FakeSink { fail: true, ..FakeSink::default() };
        let save = |entries: &[ClipEntry], max| {
            save_to(&path, entries, max, false).unwrap();
        };
        assert!(wipe_entries(&handle, Some(&sink), save).is_err());
        assert_eq!(load_from(&path).entries, sample(), "file untouched on sink error");
        let sink = FakeSink::default();
        let save = |entries: &[ClipEntry], max| {
            save_to(&path, entries, max, false).unwrap();
        };
        wipe_entries(&handle, Some(&sink), save).unwrap();
        assert!(load_from(&path).entries.is_empty());
    }

    type CommandFn = Box<dyn Fn(&ClipboardHandle, Option<&dyn MemoSink>, &mut dyn FnMut(&[ClipEntry], usize)) -> Result<(), String>>;

    fn command(
        f: impl Fn(&ClipboardHandle, Option<&dyn MemoSink>, &mut dyn FnMut(&[ClipEntry], usize)) -> Result<(), String> + 'static,
    ) -> CommandFn {
        Box::new(f)
    }

    /// Every memo command, run through its store-first helper.
    fn memo_commands() -> Vec<(&'static str, CommandFn)> {
        vec![
            ("create", command(|h, s, save| create_memo_in(h, s, "new", 50, save).map(|_| ()))),
            ("edit", command(|h, s, save| update_memo_in(h, s, "memo-1", "m1 edited", 50, save).map(|_| ()))),
            ("pin", command(|h, s, save| toggle_pin_in(h, s, "memo-1", save).map(|_| ()))),
            ("note", command(|h, s, save| set_note_in(h, s, "memo-1", Some("n".into()), save).map(|_| ()))),
            ("delete", command(|h, s, save| delete_entry_in(h, s, "memo-1", save))),
        ]
    }

    #[test]
    fn memo_command_sink_failure_changes_nothing_and_saves_nothing() {
        for (name, run) in memo_commands() {
            let handle = handle_with(sample());
            let sink = FakeSink { fail: true, ..FakeSink::default() };
            let mut saves = 0;
            let err = run(&handle, Some(&sink), &mut |_, _| saves += 1).unwrap_err();
            assert_eq!(err, "sink down", "{name}");
            assert_eq!(saves, 0, "{name}: no save");
            assert_eq!(handle.state.lock().unwrap().entries, sample(), "{name}: entries unchanged");
        }
    }

    #[test]
    fn memo_command_success_calls_the_sink_once_then_mutates_and_saves() {
        for (name, run) in memo_commands() {
            let handle = handle_with(sample());
            let sink = FakeSink::default();
            let mut saved = Vec::new();
            run(&handle, Some(&sink), &mut |entries, _| saved.push(entries.to_vec())).unwrap();
            assert_eq!(sink.changed.lock().unwrap().len(), 1, "{name}: one sink call");
            let entries = handle.state.lock().unwrap().entries.clone();
            assert_ne!(entries, sample(), "{name}: mutated");
            assert_eq!(saved, vec![entries], "{name}: one save of the new entries");
        }
        // Without a sink every command still runs locally.
        for (name, run) in memo_commands() {
            let handle = handle_with(sample());
            let mut saves = 0;
            run(&handle, None, &mut |_, _| saves += 1).unwrap();
            assert_eq!(saves, 1, "{name}");
            assert_ne!(handle.state.lock().unwrap().entries, sample(), "{name}");
        }
    }

    #[test]
    fn clip_commands_never_reach_the_sink() {
        let handle = handle_with(sample());
        let sink = FakeSink { fail: true, ..FakeSink::default() };
        let c1 = hash_text("c1");
        let pinned = toggle_pin_in(&handle, Some(&sink), &c1, |_, _| {}).unwrap().unwrap();
        assert!(pinned.pinned);
        let noted = set_note_in(&handle, Some(&sink), &c1, Some("n".into()), |_, _| {}).unwrap().unwrap();
        assert_eq!(noted.note.as_deref(), Some("n"));
        delete_entry_in(&handle, Some(&sink), &c1, |_, _| {}).unwrap();
        let before_unknown = handle.state.lock().unwrap().entries.clone();
        let err = delete_entry_in(&handle, Some(&sink), "no such id", |_, _| panic!("no save")).unwrap_err();
        assert_eq!(err, "no entry with id no such id", "an unknown id with a sink is never a silent Ok");
        assert_eq!(handle.state.lock().unwrap().entries, before_unknown);
        let entries = handle.state.lock().unwrap().entries.clone();
        assert_eq!(entries, sample()[1..].to_vec());
        let err = update_memo_in(&handle, Some(&sink), &hash_text("pinned"), "x", 1, |_, _| {}).unwrap_err();
        assert!(err.starts_with("no memo with id"), "clips are not editable: {err}");
    }

    #[test]
    fn memo_command_takes_the_entity_id_the_sink_returns() {
        let handle = handle_with(sample());
        let sink = FakeSink { rebind: Some("entity-1".into()), ..FakeSink::default() };
        let memo = update_memo_in(&handle, Some(&sink), "memo-1", "m1 edited", 50, |_, _| {}).unwrap();
        assert_eq!(memo.id, "entity-1");
        let entries = handle.state.lock().unwrap().entries.clone();
        assert_eq!(entries[0], memo);
        assert!(entries.iter().all(|e| e.id != "memo-1"));
    }

    /// Every command that keeps the entry, with a sink that rebinds the id.
    fn rebinding_commands() -> Vec<(&'static str, RebindFn)> {
        vec![
            ("pin", Box::new(|h, s| toggle_pin_in(h, s, "memo-1", |_, _| {}))),
            ("note", Box::new(|h, s| set_note_in(h, s, "memo-1", Some("n".into()), |_, _| {}))),
            ("edit", Box::new(|h, s| update_memo_in(h, s, "memo-1", "m1 edited", 50, |_, _| {}).map(Some))),
        ]
    }

    type RebindFn = Box<dyn Fn(&ClipboardHandle, Option<&dyn MemoSink>) -> Result<Option<ClipEntry>, String>>;

    #[test]
    fn a_rebinding_sink_is_signalled_and_the_old_id_then_fails_loudly() {
        for (name, run) in rebinding_commands() {
            let handle = handle_with(sample());
            let sink = FakeSink { rebind: Some("entity-1".into()), ..FakeSink::default() };
            let after = run(&handle, Some(&sink)).unwrap();
            assert!(rebound("memo-1", after.as_ref()), "{name}: the rebind is signalled");
            assert_eq!(after.as_ref().map(|e| e.id.as_str()), Some("entity-1"), "{name}");
            let entries = handle.state.lock().unwrap().entries.clone();
            assert_eq!(entries.iter().filter(|e| e.id == "entity-1").count(), 1, "{name}: the entry carries the new id");
            assert!(entries.iter().all(|e| e.id != "memo-1"), "{name}: no stale row");

            // The webview's stale id: every memo command errs and changes nothing.
            let calls = sink.changed.lock().unwrap().len();
            let stale: Vec<(&str, Result<(), String>)> = vec![
                ("delete", delete_entry_in(&handle, Some(&sink), "memo-1", |_, _| panic!("no save"))),
                ("pin", toggle_pin_in(&handle, Some(&sink), "memo-1", |_, _| panic!("no save")).map(|_| ())),
                ("note", set_note_in(&handle, Some(&sink), "memo-1", None, |_, _| panic!("no save")).map(|_| ())),
                ("edit", update_memo_in(&handle, Some(&sink), "memo-1", "x", 60, |_, _| panic!("no save")).map(|_| ())),
            ];
            for (cmd, r) in stale {
                assert!(r.is_err(), "{name} then {cmd} on the old id: {r:?}");
            }
            assert_eq!(handle.state.lock().unwrap().entries, entries, "{name}: unchanged");
            assert_eq!(sink.changed.lock().unwrap().len(), calls, "{name}: no sink call");
        }
    }

    #[test]
    fn an_unchanged_id_is_not_a_rebind() {
        for (name, run) in rebinding_commands() {
            let handle = handle_with(sample());
            let after = run(&handle, Some(&FakeSink::default())).unwrap();
            assert!(after.is_some(), "{name}");
            assert!(!rebound("memo-1", after.as_ref()), "{name}");
        }
        assert!(!rebound("memo-1", None), "a delete leaves no entry to rebind");
    }

    #[test]
    fn without_a_sink_an_unknown_id_keeps_the_legacy_no_op() {
        let handle = handle_with(sample());
        let mut saves = 0;
        delete_entry_in(&handle, None, "no such id", |_, _| saves += 1).unwrap();
        assert_eq!(toggle_pin_in(&handle, None, "no such id", |_, _| saves += 1).unwrap(), None);
        assert_eq!(set_note_in(&handle, None, "no such id", None, |_, _| saves += 1).unwrap(), None);
        assert_eq!(handle.state.lock().unwrap().entries, sample());
        assert_eq!(saves, 3, "the legacy path still saves");
    }

    #[test]
    fn memo_projection_replaces_memos_and_keeps_clips() {
        let handle = handle_with(sample());
        let mut m2 = memo_entry("memo-2", "m2 remote", 9);
        m2.pinned = true;
        let new = memo_entry("memo-3", "m3", 8);
        let (entries, max) = apply_memo_projection(&handle, vec![new.clone(), m2.clone(), clip("ignored", 1, false)]);
        assert_eq!(max, DEFAULT_MAX_ENTRIES);
        assert_eq!(entries, vec![new, clip("c1", 5, false), clip("pinned", 3, true), m2]);
        assert_eq!(handle.state.lock().unwrap().entries, entries);
    }
}
