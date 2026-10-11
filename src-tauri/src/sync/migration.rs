//! One-time migration of the pre-sync stores into the active vault (G4,
//! M1–M10, AC 9 and 28).
//!
//! The webview pushes its localStorage snapshot (conversations, translation
//! history) in chunks; together with a byte copy of `clipboard.json` it is
//! written to [`MigrationDir`] before anything else happens. Every later step
//! reads only those backup files, so a crash anywhere resumes from them:
//!
//! `none → backed_up → importing → imported → verified → native`
//!
//! held in `meta.migration_state`. Nothing in the migration dir and no
//! original file is ever deleted or rewritten here (M9).
//!
//! Contract deviations (G4 design, sync/migration.rs), agreed with the leader:
//! - `finalize` takes `webview_hash` (the webview's current snapshot hash)
//!   before `current`: with `current == None` it is compared with the
//!   manifest `snapshot_hash`; equal → the backup is the delta source,
//!   different → `NeedSnapshot` with nothing written.
//! - `MigrationDir { root, clipboard_src }`: the push needs the live
//!   `clipboard.json` path to back it up; it is only ever read.
//! - `finalize` below 3 failed attempts returns `VerifyFailed { attempts }`
//!   (rolled back, state stays `verified`), `Deferred` from the 3rd on.
//! - `apply_queue_item(store, vault, name_key, clock, qseq, op)` (G4's
//!   `apply_queued`, now a test-only wrapper) also takes the
//!   blob name key (a queued `MsgAdd` may carry images) and requires `native`
//!   (the queue is flushed only after M7). It is exactly once by `qseq`: an
//!   op at or below `meta.migration_flushed_qseq` writes nothing (G8 E3: a
//!   replay with the recorded op hash is `Duplicate`, a reused qseq
//!   `StaleQseq`), and the watermark plus the op hash
//!   advance in the same transaction; a register op (`Rename`, `TrPin`,
//!   `SetActive`) at or below `migration_ack_qseq` is already in the M6/M7
//!   snapshot and is only counted as flushed.
//! - `import`, `finalize`, and `apply_queue_item` stamp HLCs with the shared
//!   device clock (`&mut HlcClock`, owned by G7).
//! - `import` also runs from `imported` (re-import after a failed verify).
//! - `finalize` takes `live_memos: Option<&[ClipEntry]>`: `None` (the boot
//!   load was not trusted) emits no memo edits or deletes and verifies the
//!   backup memos. It stores the hash of the snapshot it used as
//!   `meta.migration_final_snapshot_hash` (an opaque string).
//! - A live memo whose entity is a tombstone and whose text, pin, or note
//!   differs from the imported backup is re-created under
//!   `uuidv5(NS, "memo-restore:" + id)` (a create that saw the delete would
//!   be invisible); an existing restore entity is diffed like any memo.
//! - Snapshots are parsed leniently: lossy UTF-8, and a lone surrogate
//!   escape becomes `\uFFFD` before serde. Plan and verify see the same
//!   text; the backup keeps the original bytes.
//! - `finish_push` requires `clipboard.json` to be missing or to parse;
//!   otherwise the push fails and the state stays `none`. Chunk 0 arriving
//!   when every chunk was received (a crash before the state was set)
//!   re-runs the finish when the bytes match.
//! - Tombstones are never resurrected: a conversation, translation, or memo
//!   whose projection entity is deleted is skipped by `plan_import` and
//!   counts as present in `verify` (its delete came through the queue or a
//!   remote device; the backup files still hold the data). The one exception
//!   is the live-memo restore above, which uses a new entity id.
//! - An empty message role (malformed source) is imported as `"unknown"`
//!   (the op schema requires a role); `verify` compares the same mapping.
//! - Duplicate ids inside one source: the first occurrence wins, in both
//!   `plan_import` and `verify`.
//! - A live memo whose id already names a memo entity (created natively) keeps
//!   that id; any other memo id maps to `uuidv5(NS, "memo:" + id)`.
//! - `Report` image counts cover the attachments of messages the plan adds.
//!
//! G8 (native load, approved with the leader):
//! - Every planner that creates a conversation, message, translation, or
//!   attachment records the webview id as an additive `src_id` (conv
//!   `create`, `msg.add`, `tr.add`, attachment objects; the op schema allows
//!   it as an optional string). [`chat_store`] hands it back; a `src_id`
//!   counts only when its uuidv5 is the entity id. An entity without one is
//!   exposed under its entity id (`src_fallbacks`), and `apply_queued`
//!   resolves a ref that already names an entity to that entity, so such an
//!   id never creates a second one.
//! - `verify` ignores `src_id` on translations (pre-G8 ops lack it).
//! - [`chat_store`] returns conversations with `createdAt` (first message
//!   ts, else the first `create` wall) and `updatedAt` (max message ts, else
//!   `createdAt`); messages are the active path (the `set_active` register,
//!   else the newest leaf).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;

use data_encoding::BASE64;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::causal::OpSet;
use super::crypto::{hex, hmac_sha256, SecretKey};
use super::memo_bridge::memo_ops;
use super::merge::{ConvState, EntityState, Message, Projection};
use super::ops::{canonical_json, HlcClock, Kind, OpBody, FORMAT_V};
use super::store::{now_ms, BlobRow, Store, StoreError};
use crate::clipboard::{write_atomic, ClipEntry, EntryKind, PersistedState};

/// `uuid!("6f1c2d3e-5a4b-4c8d-9e0f-a1b2c3d4e5f6")`, shared with the TS side.
pub(crate) const ID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x6f1c2d3e_5a4b_4c8d_9e0f_a1b2c3d4e5f6);

const STATE_KEY: &str = "migration_state";
const ATTEMPTS_KEY: &str = "migration_finalize_attempts";
const ACK_KEY: &str = "migration_ack_qseq";
const MEMO_IDS_KEY: &str = "migration_import_memo_ids";
const FLUSHED_KEY: &str = "migration_flushed_qseq";
const FINAL_HASH_KEY: &str = "migration_final_snapshot_hash";
/// Failed finalize attempts after which the result is `Deferred`.
pub(crate) const MAX_FINALIZE_ATTEMPTS: u32 = 3;
/// Imported for a message whose source role is empty.
const UNKNOWN_ROLE: &str = "unknown";
/// The webview's own id, carried additively in conv `create`, `msg.add`,
/// `tr.add`, and attachment payloads (G8) so a load can hand it back.
pub(crate) const SRC_ID: &str = "src_id";

fn v5(name: &str) -> String {
    uuid::Uuid::new_v5(&ID_NAMESPACE, name.as_bytes()).hyphenated().to_string()
}

pub(crate) fn conv_entity_id(conv_id: &str) -> String {
    v5(&format!("conv:{conv_id}"))
}

pub(crate) fn msg_entity_id(conv_id: &str, msg_id: &str) -> String {
    v5(&format!("msg:{conv_id}:{msg_id}"))
}

pub(crate) fn tr_entity_id(entry_id: &str) -> String {
    v5(&format!("tr:{entry_id}"))
}

pub(crate) fn memo_entity_id(entry_id: &str) -> String {
    v5(&format!("memo:{entry_id}"))
}

/// The entity a live memo is re-created under when its own entity is a
/// tombstone and the user changed it (see the module doc).
pub(crate) fn memo_restore_id(entry_id: &str) -> String {
    v5(&format!("memo-restore:{entry_id}"))
}

// ---------------------------------------------------------------------------
// State and errors

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationState {
    None,
    BackedUp,
    Importing,
    Imported,
    Verified,
    Native,
}

impl MigrationState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MigrationState::None => "none",
            MigrationState::BackedUp => "backed_up",
            MigrationState::Importing => "importing",
            MigrationState::Imported => "imported",
            MigrationState::Verified => "verified",
            MigrationState::Native => "native",
        }
    }

    /// Unknown text is an error, never a reset to `none`.
    pub(crate) fn parse(text: &str) -> Result<Self, MigrationError> {
        Ok(match text {
            "none" => MigrationState::None,
            "backed_up" => MigrationState::BackedUp,
            "importing" => MigrationState::Importing,
            "imported" => MigrationState::Imported,
            "verified" => MigrationState::Verified,
            "native" => MigrationState::Native,
            other => return Err(MigrationError::UnknownState(other.to_owned())),
        })
    }

    /// The stored state; an absent row is `none`.
    pub(crate) fn load(store: &Store) -> Result<Self, MigrationError> {
        match store.meta_get(STATE_KEY)? {
            None => Ok(MigrationState::None),
            Some(text) => MigrationState::parse(&text),
        }
    }

    /// Whether the store file at `path` already reached `native`, read
    /// without opening it as a [`Store`] (nothing created or migrated; G8
    /// E1). A missing file, table or key is `Ok(false)`; a read failure is
    /// an error, never "not native" (E1a).
    pub(crate) fn file_is_native(path: &std::path::Path) -> Result<bool, StoreError> {
        Ok(Store::peek_meta(path, STATE_KEY)?.is_some_and(|text| text == MigrationState::Native.as_str()))
    }
}

/// Test-only crash points (same pattern as `apply_rotation_then_crash`).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Crash {
    AfterBackupFiles,
    AfterImportingSet,
    InImportTx,
    AfterImported,
    AfterVerified,
    InFinalizeTx,
}

/// Uninhabited outside tests: production paths always pass `None`.
#[cfg(not(test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crash {}

/// Returns `Crashed(at)` from the enclosing function when the test-only
/// crash point `at` is selected.
macro_rules! crash_point {
    ($crash:expr, $at:ident) => {
        #[cfg(test)]
        {
            if $crash == Some(Crash::$at) {
                return Err(MigrationError::Crashed(Crash::$at));
            }
        }
        #[cfg(not(test))]
        let _ = &$crash;
    };
}

/// Whether the test-only crash point `at` is selected.
macro_rules! crash_hit {
    ($crash:expr, $at:ident) => {{
        #[cfg(test)]
        let hit = $crash == Some(Crash::$at);
        #[cfg(not(test))]
        let hit = {
            let _ = &$crash;
            false
        };
        hit
    }};
}

#[derive(Debug)]
pub(crate) enum MigrationError {
    Store(StoreError),
    Io(io::Error),
    Json(serde_json::Error),
    /// `meta.migration_state` holds text this build does not know.
    UnknownState(String),
    /// A chunk broke the push protocol; the partial snapshot was reset.
    Chunk(String),
    /// The call needs another migration state.
    WrongState(MigrationState),
    /// A backup file does not match its manifest, or a stored row is damaged.
    Corrupt(String),
    #[cfg(test)]
    Crashed(Crash),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrationError::Store(e) => write!(f, "store: {e}"),
            MigrationError::Io(e) => write!(f, "io: {e}"),
            MigrationError::Json(e) => write!(f, "json: {e}"),
            MigrationError::UnknownState(s) => write!(f, "unknown migration_state {s:?}"),
            MigrationError::Chunk(why) => write!(f, "snapshot chunk rejected: {why}"),
            MigrationError::WrongState(s) => write!(f, "not allowed in migration state {}", s.as_str()),
            MigrationError::Corrupt(why) => write!(f, "migration backup corrupt: {why}"),
            #[cfg(test)]
            MigrationError::Crashed(at) => write!(f, "simulated crash at {at:?}"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<StoreError> for MigrationError {
    fn from(e: StoreError) -> Self {
        MigrationError::Store(e)
    }
}

impl From<io::Error> for MigrationError {
    fn from(e: io::Error) -> Self {
        MigrationError::Io(e)
    }
}

impl From<serde_json::Error> for MigrationError {
    fn from(e: serde_json::Error) -> Self {
        MigrationError::Json(e)
    }
}

fn corrupt(why: impl fmt::Display) -> MigrationError {
    MigrationError::Corrupt(why.to_string())
}

// ---------------------------------------------------------------------------
// Backup directory

/// `root` is `app_data_dir/sync-migration/` (G7); `clipboard_src` is the live
/// `clipboard.json`, which is only ever read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationDir {
    pub(crate) root: PathBuf,
    pub(crate) clipboard_src: PathBuf,
}

impl MigrationDir {
    pub(crate) fn new(root: PathBuf, clipboard_src: PathBuf) -> Self {
        MigrationDir { root, clipboard_src }
    }

    /// Byte copy of `clipboard_src`; absent if the source was missing.
    pub(crate) fn clipboard_backup(&self) -> PathBuf {
        self.root.join("clipboard.json.bak")
    }

    pub(crate) fn webstore(&self) -> PathBuf {
        self.root.join("webstore.json")
    }

    pub(crate) fn manifest(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    fn partial(&self) -> PathBuf {
        self.root.join("webstore.json.partial")
    }

    fn progress(&self) -> PathBuf {
        self.root.join("webstore.json.partial.progress")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) webstore_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) clipboard_sha256: Option<String>,
    pub(crate) snapshot_hash: String,
    pub(crate) created_at: u64,
}

/// Chunk bookkeeping next to the partial file. `len` is the partial length
/// the received chunks add up to, so a torn append is cut back to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Progress {
    total: u32,
    received: u32,
    len: u64,
    last_len: u64,
    snapshot_hash: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn read_progress(dir: &MigrationDir) -> Result<Progress, MigrationError> {
    match fs::read(dir.progress()) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(|e| corrupt(format!("chunk progress: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Progress::default()),
        Err(e) => Err(e.into()),
    }
}

fn write_progress(dir: &MigrationDir, progress: &Progress) -> Result<(), MigrationError> {
    write_atomic(&dir.progress(), &serde_json::to_vec(progress)?)?;
    Ok(())
}

/// Empties the partial snapshot and its progress, then reports `why`.
fn reset_chunks(dir: &MigrationDir, why: String) -> MigrationError {
    let reset = write_atomic(&dir.partial(), b"").map_err(MigrationError::from).and_then(|()| {
        write_progress(dir, &Progress::default())
    });
    match reset {
        Ok(()) => MigrationError::Chunk(why),
        Err(e) => e,
    }
}

/// The partial bytes the progress accounts for (a torn tail is ignored).
fn partial_bytes(dir: &MigrationDir, progress: &Progress) -> Result<Option<Vec<u8>>, MigrationError> {
    let raw = match fs::read(dir.partial()) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let len = usize::try_from(progress.len).map_err(corrupt)?;
    if raw.len() < len {
        return Ok(None);
    }
    Ok(Some(raw[..len].to_vec()))
}

/// M1/M2: one chunk of the webview snapshot. Ignored (returns the state)
/// once the state is past `none`. After the last chunk the clipboard backup,
/// `webstore.json`, and `manifest.json` are written, and only then is the
/// state set to `backed_up`.
pub(crate) fn push_webstore_chunk(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    index: u32,
    total: u32,
    bytes: &[u8],
    snapshot_hash: &str,
) -> Result<MigrationState, MigrationError> {
    push_inner(store, dir, vault, index, total, bytes, snapshot_hash, None)
}

#[allow(clippy::too_many_arguments)]
fn push_inner(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    index: u32,
    total: u32,
    bytes: &[u8],
    snapshot_hash: &str,
    crash: Option<Crash>,
) -> Result<MigrationState, MigrationError> {
    let state = MigrationState::load(store)?;
    if state != MigrationState::None {
        return Ok(state);
    }
    store.device_id(vault)?;
    fs::create_dir_all(&dir.root)?;
    let mut progress = read_progress(dir)?;
    if total == 0 || index >= total {
        return Err(reset_chunks(dir, format!("chunk {index} of {total}")));
    }
    if progress.received > 0 && (progress.total != total || progress.snapshot_hash != snapshot_hash) {
        return Err(reset_chunks(dir, "total or snapshot hash changed mid-push".to_owned()));
    }
    let Some(have) = partial_bytes(dir, &progress)? else {
        return Err(reset_chunks(dir, "partial snapshot shorter than recorded".to_owned()));
    };
    if progress.received > 0 && index + 1 == progress.received {
        // A re-send of the previous chunk: a no-op when identical.
        let last = usize::try_from(progress.last_len).map_err(corrupt)?;
        if last != bytes.len() || have.len() < last || have[have.len() - last..] != *bytes {
            return Err(reset_chunks(dir, format!("chunk {index} re-sent with other bytes")));
        }
    } else if index == progress.received {
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(false).open(dir.partial())?;
        file.set_len(progress.len)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        progress = Progress {
            total,
            received: progress.received + 1,
            len: progress.len + bytes.len() as u64,
            last_len: bytes.len() as u64,
            snapshot_hash: snapshot_hash.to_owned(),
        };
        write_progress(dir, &progress)?;
    } else if index == 0 && progress.received == total && !bytes.is_empty() && have.starts_with(bytes) {
        // Every chunk was received but the state was never set (a crash
        // inside the finish): the restarted push re-runs the finish.
    } else {
        return Err(reset_chunks(dir, format!("chunk {index} out of order (expected {})", progress.received)));
    }
    if progress.received == total {
        finish_push(store, dir, &progress, crash)?;
    }
    MigrationState::load(store)
}

fn finish_push(
    store: &mut Store,
    dir: &MigrationDir,
    progress: &Progress,
    crash: Option<Crash>,
) -> Result<(), MigrationError> {
    let Some(raw) = partial_bytes(dir, progress)? else {
        return Err(reset_chunks(dir, "partial snapshot shorter than recorded".to_owned()));
    };
    if let Err(e) = parse_webstore(&raw) {
        return Err(reset_chunks(dir, format!("snapshot does not parse: {e}")));
    }
    // 1. Clipboard backup (read only; a missing source has no backup). A
    //    file that does not parse fails the push and leaves the chunks: the
    //    later mirror would otherwise overwrite the memos it holds.
    let clipboard_sha256 = match fs::read(&dir.clipboard_src) {
        Ok(original) => {
            if let Err(e) = serde_json::from_slice::<PersistedState>(&original) {
                return Err(corrupt(format!("clipboard.json does not parse: {e}")));
            }
            write_atomic(&dir.clipboard_backup(), &original)?;
            Some(sha256_hex(&original))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    // 2. Snapshot and manifest.
    write_atomic(&dir.webstore(), &raw)?;
    let manifest = Manifest {
        webstore_sha256: sha256_hex(&raw),
        clipboard_sha256,
        snapshot_hash: progress.snapshot_hash.clone(),
        created_at: now_ms(),
    };
    write_atomic(&dir.manifest(), &serde_json::to_vec_pretty(&manifest)?)?;
    crash_point!(crash, AfterBackupFiles);
    // 3. Only now is the backup the source of truth.
    store.meta_set(STATE_KEY, MigrationState::BackedUp.as_str())?;
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct ClipboardFile {
    #[serde(default)]
    entries: Vec<ClipEntry>,
}

/// The backup files, each checked against the manifest.
fn read_backup(dir: &MigrationDir) -> Result<(Manifest, WebStore, Vec<ClipEntry>), MigrationError> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(dir.manifest())?)?;
    let raw = fs::read(dir.webstore())?;
    if sha256_hex(&raw) != manifest.webstore_sha256 {
        return Err(corrupt("webstore.json does not match the manifest"));
    }
    let ws = parse_webstore(&raw)?;
    let memos = match &manifest.clipboard_sha256 {
        None => Vec::new(),
        Some(want) => {
            let raw = fs::read(dir.clipboard_backup())?;
            if sha256_hex(&raw) != *want {
                return Err(corrupt("clipboard.json.bak does not match the manifest"));
            }
            let file: ClipboardFile = serde_json::from_slice(&raw)?;
            file.entries.into_iter().filter(|e| e.kind == EntryKind::Memo).collect()
        }
    };
    Ok((manifest, ws, memos))
}

// ---------------------------------------------------------------------------
// Webview store mirrors (chat-history.ts, chat-image.ts, history.ts)

/// Any JSON number as a non-negative integer (floats floored, clamped to
/// `i64::MAX`); anything else is 0.
fn lenient_u64<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let cap = i64::MAX as u64;
    Ok(match Value::deserialize(d)? {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite() && *f > 0.0).map(|f| f.min(cap as f64) as u64))
            .unwrap_or(0)
            .min(cap),
        _ => 0,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebStore {
    #[serde(default)]
    pub(crate) conversations: Vec<WsConv>,
    #[serde(default)]
    pub(crate) history: Vec<WsTr>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WsConv {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) messages: Vec<WsMsg>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WsMsg {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) content: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub(crate) ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) images: Vec<WsImage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WsImage {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) mime_type: String,
    /// Raw base64; empty once evicted.
    #[serde(default)]
    pub(crate) data: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub(crate) width: u64,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub(crate) height: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WsTr {
    pub(crate) id: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub(crate) ts: u64,
    #[serde(default)]
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) from: String,
    #[serde(default)]
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) pinned: bool,
}

/// The UTF-16 code unit spelled by the four hex digits at `s[at..at + 4]`.
fn hex4(s: &[u8], at: usize) -> Option<u16> {
    let digits = s.get(at..at + 4)?;
    if !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

/// `bytes` as lossy UTF-8 with every lone surrogate escape (a `\uD800`–
/// `\uDFFF` escape that is not a high-low pair) replaced by `\uFFFD`. Only a
/// real escape counts: `\\` is consumed as one unit, so the `\u` in `\\u…`
/// is plain text and is left alone.
fn sanitize_json(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let s = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' {
            i += 1;
            continue;
        }
        if s.get(i + 1) != Some(&b'u') {
            // `\\`, `\"`, `\n`, ...: the escaped byte is never an escape start.
            i += 2;
            continue;
        }
        let Some(unit) = hex4(s, i + 2) else {
            i += 2;
            continue;
        };
        let paired = is_high(unit)
            && s.get(i + 6) == Some(&b'\\')
            && s.get(i + 7) == Some(&b'u')
            && hex4(s, i + 8).is_some_and(is_low);
        if paired {
            i += 12;
            continue;
        }
        if is_high(unit) || is_low(unit) {
            // `i` and `i + 6` sit on ASCII bytes, so both are char boundaries.
            out.push_str(&text[copied..i]);
            out.push_str("\\uFFFD");
            copied = i + 6;
        }
        i += 6;
    }
    out.push_str(&text[copied..]);
    out
}

/// A webview snapshot, parsed leniently (see [`sanitize_json`]). Every reader
/// of snapshot bytes goes through here, so plan and verify see one text.
pub(crate) fn parse_webstore(bytes: &[u8]) -> Result<WebStore, serde_json::Error> {
    serde_json::from_str(&sanitize_json(bytes))
}

// ---------------------------------------------------------------------------
// Planning (pure)

/// One op of a plan; op id, HLC, and ctx are assigned when it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedOp {
    pub(crate) kind: Kind,
    pub(crate) entity_id: String,
    pub(crate) op_type: String,
    pub(crate) payload: Value,
}

/// Source entities considered (duplicates once) and image outcomes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) conversations: usize,
    pub(crate) messages: usize,
    pub(crate) translations: usize,
    pub(crate) memos: usize,
    pub(crate) images: usize,
    pub(crate) evicted_images: usize,
    pub(crate) invalid_images: usize,
    /// Entities whose projection is a tombstone (never resurrected).
    pub(crate) skipped_deleted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) ops: Vec<PlannedOp>,
    pub(crate) blobs: Vec<BlobRow>,
    pub(crate) report: Report,
    /// Entity ids of the memos the plan was given.
    pub(crate) memo_ids: BTreeSet<String>,
}

struct Planner {
    ops: Vec<PlannedOp>,
    blobs: BTreeMap<String, BlobRow>,
    report: Report,
}

impl Planner {
    fn push(&mut self, kind: Kind, entity_id: &str, op_type: &str, payload: Value) {
        self.ops.push(PlannedOp { kind, entity_id: entity_id.to_owned(), op_type: op_type.to_owned(), payload });
    }
}

fn role_of(msg: &WsMsg) -> &str {
    if msg.role.is_empty() {
        UNKNOWN_ROLE
    } else {
        &msg.role
    }
}

/// A non-empty decodable image becomes a blob reference (its row goes to
/// `blobs`); an evicted (`""`) or undecodable one becomes evicted metadata.
fn attachment(img: &WsImage, name_key: &SecretKey, blobs: &mut BTreeMap<String, BlobRow>, report: &mut Report) -> Value {
    let mut obj = Map::new();
    let bytes = if img.data.is_empty() {
        report.evicted_images += 1;
        None
    } else {
        match BASE64.decode(img.data.as_bytes()) {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            _ => {
                report.invalid_images += 1;
                None
            }
        }
    };
    match bytes {
        Some(bytes) => {
            report.images += 1;
            let blob_id = hex(&hmac_sha256(name_key, &bytes));
            obj.insert("blob_id".to_owned(), json!(blob_id));
            blobs.entry(blob_id.clone()).or_insert_with(|| BlobRow {
                blob_id,
                mime: img.mime_type.clone(),
                sha256: Sha256::digest(&bytes).into(),
                bytes,
            });
        }
        None => {
            obj.insert("evicted".to_owned(), json!(true));
        }
    }
    obj.insert("mime".to_owned(), json!(img.mime_type));
    obj.insert("width".to_owned(), json!(img.width));
    obj.insert("height".to_owned(), json!(img.height));
    if let Some(name) = &img.name {
        obj.insert("name".to_owned(), json!(name));
    }
    if !img.id.is_empty() {
        obj.insert(SRC_ID.to_owned(), json!(img.id));
    }
    Value::Object(obj)
}

fn msg_payload(conv_eid: &str, msg_eid: &str, parent: Option<&str>, msg: &WsMsg, attachments: Vec<Value>) -> Value {
    let mut obj = Map::new();
    obj.insert(SRC_ID.to_owned(), json!(msg.id));
    obj.insert("conv_id".to_owned(), json!(conv_eid));
    obj.insert("msg_id".to_owned(), json!(msg_eid));
    if let Some(parent) = parent {
        obj.insert("parent_id".to_owned(), json!(parent));
    }
    obj.insert("role".to_owned(), json!(role_of(msg)));
    obj.insert("content".to_owned(), json!(msg.content));
    if let Some(model) = &msg.model {
        obj.insert("model".to_owned(), json!(model));
    }
    obj.insert("attachments".to_owned(), Value::Array(attachments));
    obj.insert("ts".to_owned(), json!(msg.ts));
    Value::Object(obj)
}

fn tr_payload(tr: &WsTr) -> Value {
    json!({
        "source": tr.source,
        "target": tr.target,
        "from": tr.from,
        "to": tr.to,
        "model": tr.model,
        "ts": tr.ts,
        "pinned": tr.pinned,
        SRC_ID: tr.id,
    })
}

fn conv_create_payload(src: &str, title: &str) -> Value {
    json!({"title": title, SRC_ID: src})
}

/// The entity id of a clipboard memo: its own id when that already names a
/// memo entity (a native memo), else the deterministic migrated id.
fn memo_id_for(p: &Projection, entry_id: &str) -> String {
    if p.entities.contains_key(&(Kind::Memo, entry_id.to_owned())) {
        entry_id.to_owned()
    } else {
        memo_entity_id(entry_id)
    }
}

fn note_of(entry: &ClipEntry) -> &str {
    entry.note.as_deref().unwrap_or("")
}

/// The entity a live memo is planned and verified under: [`memo_id_for`],
/// unless that entity is a tombstone and `entry` differs (text, pin, or
/// note) from the imported backup memo with its id (a memo absent from the
/// backup differs); then [`memo_restore_id`]. `backup = None` never
/// restores.
fn memo_target(p: &Projection, entry: &ClipEntry, backup: Option<&[ClipEntry]>) -> String {
    let id = memo_id_for(p, &entry.id);
    let Some(backup) = backup else {
        return id;
    };
    if !matches!(p.entities.get(&(Kind::Memo, id.clone())), Some(EntityState::Deleted)) {
        return id;
    }
    let imported = backup.iter().find(|b| b.kind == EntryKind::Memo && b.id == entry.id);
    let unchanged = imported
        .is_some_and(|b| b.text == entry.text && b.pinned == entry.pinned && note_of(b) == note_of(entry));
    if unchanged {
        id
    } else {
        memo_restore_id(&entry.id)
    }
}

/// The memos a plan diffs: the live entries and the backup memos they are
/// compared with for the tombstone restore (`None`: never restore).
struct MemoSource<'a> {
    live: &'a [ClipEntry],
    backup: Option<&'a [ClipEntry]>,
}

/// The additive import delta of `ws` and `memos` against `p` (see the module
/// doc). The only deletes are own-import memos absent from `memos`.
pub(crate) fn plan_import(
    p: &Projection,
    ws: &WebStore,
    memos: &[ClipEntry],
    own_import_memo_ids: &BTreeSet<String>,
    name_key: &SecretKey,
) -> Plan {
    plan_inner(p, ws, Some(MemoSource { live: memos, backup: None }), own_import_memo_ids, name_key)
}

/// [`plan_import`]; `memos = None` plans no memo op at all.
fn plan_inner(
    p: &Projection,
    ws: &WebStore,
    memos: Option<MemoSource<'_>>,
    own_import_memo_ids: &BTreeSet<String>,
    name_key: &SecretKey,
) -> Plan {
    let mut pl = Planner { ops: Vec::new(), blobs: BTreeMap::new(), report: Report::default() };

    let mut seen_convs = BTreeSet::new();
    for conv in &ws.conversations {
        if !seen_convs.insert(conv.id.as_str()) {
            continue;
        }
        pl.report.conversations += 1;
        let cid = conv_entity_id(&conv.id);
        let state = match p.entities.get(&(Kind::Conv, cid.clone())) {
            Some(EntityState::Deleted) => {
                pl.report.skipped_deleted += 1;
                continue;
            }
            Some(EntityState::Conv(c)) => Some(c),
            _ => None,
        };
        match state {
            None => pl.push(Kind::Conv, &cid, "create", conv_create_payload(&conv.id, &conv.title)),
            Some(c) if c.title != conv.title => pl.push(Kind::Conv, &cid, "rename", json!({"title": conv.title})),
            Some(_) => {}
        }
        let mut seen_msgs = BTreeSet::new();
        let mut prev: Option<String> = None;
        for msg in &conv.messages {
            if !seen_msgs.insert(msg.id.as_str()) {
                continue;
            }
            pl.report.messages += 1;
            let mid = msg_entity_id(&conv.id, &msg.id);
            if !state.is_some_and(|c| c.messages.contains_key(&mid)) {
                let attachments: Vec<Value> =
                    msg.images.iter().map(|img| attachment(img, name_key, &mut pl.blobs, &mut pl.report)).collect();
                let payload = msg_payload(&cid, &mid, prev.as_deref(), msg, attachments);
                pl.push(Kind::Conv, &cid, "msg.add", payload);
            }
            prev = Some(mid);
        }
        if let Some(leaf) = prev {
            if state.and_then(|c| c.active_leaf.as_deref()) != Some(leaf.as_str()) {
                pl.push(Kind::Conv, &cid, "set_active", json!({"leaf": leaf}));
            }
        }
    }

    let mut seen_trs = BTreeSet::new();
    for tr in &ws.history {
        if !seen_trs.insert(tr.id.as_str()) {
            continue;
        }
        pl.report.translations += 1;
        let tid = tr_entity_id(&tr.id);
        match p.entities.get(&(Kind::Tr, tid.clone())) {
            Some(EntityState::Deleted) => pl.report.skipped_deleted += 1,
            Some(EntityState::Tr(t)) => {
                if t.pinned != tr.pinned {
                    pl.push(Kind::Tr, &tid, "tr.pin", json!({"pinned": tr.pinned}));
                }
            }
            _ => pl.push(Kind::Tr, &tid, "tr.add", tr_payload(tr)),
        }
    }

    let mut memo_ids = BTreeSet::new();
    let Some(memos) = memos else {
        return Plan { ops: pl.ops, blobs: pl.blobs.into_values().collect(), report: pl.report, memo_ids };
    };
    for entry in memos.live.iter().filter(|e| e.kind == EntryKind::Memo) {
        let id = memo_target(p, entry, memos.backup);
        if !memo_ids.insert(id.clone()) {
            continue;
        }
        pl.report.memos += 1;
        let head = match p.entities.get(&(Kind::Memo, id.clone())) {
            Some(EntityState::Deleted) => {
                pl.report.skipped_deleted += 1;
                continue;
            }
            Some(EntityState::Memo(m)) => Some(m),
            _ => None,
        };
        let mut after = entry.clone();
        after.id = id.clone();
        for (op_type, payload) in memo_ops(&id, head, None, Some(&after)) {
            pl.push(Kind::Memo, &id, op_type, payload);
        }
    }
    for id in own_import_memo_ids {
        if !memo_ids.contains(id) && matches!(p.entities.get(&(Kind::Memo, id.clone())), Some(EntityState::Memo(_))) {
            pl.push(Kind::Memo, id, "delete", json!({}));
        }
    }

    Plan { ops: pl.ops, blobs: pl.blobs.into_values().collect(), report: pl.report, memo_ids }
}

/// Fresh UUIDv7 op ids and HLC stamps from the device clock; the store fills
/// device and ctx.
fn to_bodies(ops: Vec<PlannedOp>, clock: &mut HlcClock, now: u64) -> Vec<OpBody> {
    ops.into_iter()
        .map(|op| OpBody {
            v: FORMAT_V,
            op_id: uuid::Uuid::now_v7().hyphenated().to_string(),
            origin_device: String::new(),
            hlc: clock.tick(now),
            ctx: BTreeMap::new(),
            obs: Vec::new(),
            kind: op.kind,
            entity_id: op.entity_id,
            op_type: op.op_type,
            payload: op.payload,
        })
        .collect()
}

fn read_memo_ids(store: &Store) -> Result<BTreeSet<String>, MigrationError> {
    match store.meta_get(MEMO_IDS_KEY)? {
        None => Ok(BTreeSet::new()),
        Some(text) => serde_json::from_str(&text).map_err(|e| corrupt(format!("{MEMO_IDS_KEY}: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Verification (pure)

/// A translation payload without its register (`pinned`) and its additive
/// `src_id`, which a pre-G8 op lacks.
fn strip_pinned(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("pinned");
        obj.remove(SRC_ID);
    }
    v
}

/// Every source entity maps to a projection entity with equal fields;
/// returns the mismatches and fills `report` with the checked counts. A live
/// memo is looked up where the plan put it (`backup` as in [`memo_target`]).
fn check(
    p: &Projection,
    ws: &WebStore,
    memos: &[ClipEntry],
    backup: Option<&[ClipEntry]>,
    report: &mut Report,
) -> Vec<String> {
    let mut bad = Vec::new();
    let mut seen_convs = BTreeSet::new();
    for conv in &ws.conversations {
        if !seen_convs.insert(conv.id.as_str()) {
            continue;
        }
        report.conversations += 1;
        let c = match p.entities.get(&(Kind::Conv, conv_entity_id(&conv.id))) {
            Some(EntityState::Deleted) => {
                report.skipped_deleted += 1;
                continue;
            }
            Some(EntityState::Conv(c)) => c,
            _ => {
                bad.push(format!("conversation {} missing", conv.id));
                continue;
            }
        };
        if c.title != conv.title {
            bad.push(format!("conversation {} title", conv.id));
        }
        let mut seen_msgs = BTreeSet::new();
        for msg in &conv.messages {
            if !seen_msgs.insert(msg.id.as_str()) {
                continue;
            }
            report.messages += 1;
            let Some(m) = c.messages.get(&msg_entity_id(&conv.id, &msg.id)) else {
                bad.push(format!("message {}/{} missing", conv.id, msg.id));
                continue;
            };
            if m.role != role_of(msg)
                || m.content != msg.content
                || m.model != msg.model
                || m.ts != msg.ts
                || m.attachments.len() != msg.images.len()
            {
                bad.push(format!("message {}/{} differs", conv.id, msg.id));
            }
        }
    }
    let mut seen_trs = BTreeSet::new();
    for tr in &ws.history {
        if !seen_trs.insert(tr.id.as_str()) {
            continue;
        }
        report.translations += 1;
        match p.entities.get(&(Kind::Tr, tr_entity_id(&tr.id))) {
            Some(EntityState::Deleted) => report.skipped_deleted += 1,
            Some(EntityState::Tr(t)) => {
                if strip_pinned(t.data.clone()) != strip_pinned(tr_payload(tr)) || t.pinned != tr.pinned {
                    bad.push(format!("translation {} differs", tr.id));
                }
            }
            _ => bad.push(format!("translation {} missing", tr.id)),
        }
    }
    let mut seen_memos = BTreeSet::new();
    for entry in memos.iter().filter(|e| e.kind == EntryKind::Memo) {
        let id = memo_target(p, entry, backup);
        if !seen_memos.insert(id.clone()) {
            continue;
        }
        report.memos += 1;
        match p.entities.get(&(Kind::Memo, id)) {
            Some(EntityState::Deleted) => report.skipped_deleted += 1,
            Some(EntityState::Memo(m)) => {
                if m.text != entry.text || m.pinned != entry.pinned || m.note != entry.note.as_deref().unwrap_or("") {
                    bad.push(format!("memo {} differs", entry.id));
                }
            }
            _ => bad.push(format!("memo {} missing", entry.id)),
        }
    }
    bad
}

/// What finalize must find after inserting the delta from `current` over
/// the backup: titles and pins follow `current`; a message or translation
/// already present keeps its first (backup) version, since import only adds.
fn union_store(backup: &WebStore, current: &WebStore) -> WebStore {
    let cur_convs: BTreeMap<&str, &WsConv> =
        current.conversations.iter().rev().map(|c| (c.id.as_str(), c)).collect();
    let mut conversations = Vec::new();
    let mut done = BTreeSet::new();
    for conv in &backup.conversations {
        if !done.insert(conv.id.as_str()) {
            continue;
        }
        let mut merged = conv.clone();
        if let Some(cur) = cur_convs.get(conv.id.as_str()) {
            merged.title = cur.title.clone();
            let have: BTreeSet<&str> = conv.messages.iter().map(|m| m.id.as_str()).collect();
            merged.messages.extend(cur.messages.iter().filter(|m| !have.contains(m.id.as_str())).cloned());
        }
        conversations.push(merged);
    }
    conversations.extend(current.conversations.iter().filter(|c| !done.contains(c.id.as_str())).cloned());

    let cur_trs: BTreeMap<&str, &WsTr> = current.history.iter().rev().map(|t| (t.id.as_str(), t)).collect();
    let mut history = Vec::new();
    let mut done = BTreeSet::new();
    for tr in &backup.history {
        if !done.insert(tr.id.as_str()) {
            continue;
        }
        let mut merged = tr.clone();
        if let Some(cur) = cur_trs.get(tr.id.as_str()) {
            merged.pinned = cur.pinned;
        }
        history.push(merged);
    }
    history.extend(current.history.iter().filter(|t| !done.contains(t.id.as_str())).cloned());
    WebStore { conversations, history }
}

// ---------------------------------------------------------------------------
// Import, verify, finalize

/// M4: `backed_up | importing | imported` → `importing` (own write) → one
/// batch with the plan from the backup files, `migration_state = imported`,
/// and the own-import memo ids. Re-running emits only what is missing.
pub(crate) fn import(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    now: u64,
) -> Result<Report, MigrationError> {
    import_inner(store, dir, vault, name_key, clock, now, None)
}

fn import_inner(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    now: u64,
    crash: Option<Crash>,
) -> Result<Report, MigrationError> {
    let state = MigrationState::load(store)?;
    if !matches!(state, MigrationState::BackedUp | MigrationState::Importing | MigrationState::Imported) {
        return Err(MigrationError::WrongState(state));
    }
    store.device_id(vault)?;
    store.meta_set(STATE_KEY, MigrationState::Importing.as_str())?;
    crash_point!(crash, AfterImportingSet);
    let (_, ws, memos) = read_backup(dir)?;
    let own = read_memo_ids(store)?;
    let p = store.projection(vault)?;
    let plan = plan_import(&p, &ws, &memos, &own, name_key);
    let ids: BTreeSet<String> = own.union(&plan.memo_ids).cloned().collect();
    let meta = vec![
        (STATE_KEY.to_owned(), MigrationState::Imported.as_str().to_owned()),
        (MEMO_IDS_KEY.to_owned(), serde_json::to_string(&ids)?),
    ];
    let abort = crash_hit!(crash, InImportTx);
    let bodies = to_bodies(plan.ops, clock, now);
    let (stored, _) = store.insert_local_batch_checked(vault, bodies, plan.blobs, meta, |_| !abort)?;
    if stored.is_none() {
        #[cfg(test)]
        return Err(MigrationError::Crashed(Crash::InImportTx));
        #[cfg(not(test))]
        return Err(corrupt("import transaction rolled back"));
    }
    crash_point!(crash, AfterImported);
    Ok(plan.report)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verify {
    /// State is `verified`.
    Passed(Report),
    /// State stays `imported`; the next boot re-imports.
    Failed(Vec<String>),
}

/// M5: every backup entity is in the projection with equal fields.
pub(crate) fn verify(store: &mut Store, dir: &MigrationDir, vault: &str) -> Result<Verify, MigrationError> {
    verify_inner(store, dir, vault, None)
}

fn verify_inner(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    crash: Option<Crash>,
) -> Result<Verify, MigrationError> {
    let state = MigrationState::load(store)?;
    if !matches!(state, MigrationState::Imported | MigrationState::Verified) {
        return Err(MigrationError::WrongState(state));
    }
    store.device_id(vault)?;
    let (_, ws, memos) = read_backup(dir)?;
    let p = store.projection(vault)?;
    let mut report = Report::default();
    let bad = check(&p, &ws, &memos, None, &mut report);
    if !bad.is_empty() {
        return Ok(Verify::Failed(bad));
    }
    if state == MigrationState::Imported {
        store.meta_set(STATE_KEY, MigrationState::Verified.as_str())?;
    }
    crash_point!(crash, AfterVerified);
    Ok(Verify::Passed(report))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finalize {
    /// M6: the webview changed since the backup; send the current snapshot.
    NeedSnapshot,
    /// M7: native; the webview drops its queue up to `ack_qseq`.
    Native { ack_qseq: u64 },
    /// Re-verify failed and was rolled back (state stays `verified`).
    VerifyFailed { attempts: u32, mismatches: Vec<String> },
    /// The `MAX_FINALIZE_ATTEMPTS`th failure or later: retried next boot.
    Deferred { attempts: u32, mismatches: Vec<String> },
}

/// M6–M7: plans the delta from `current` (or the backup when the webview
/// hash still matches the manifest) plus `live_memos`, inserts it,
/// re-verifies the union, and sets `native` with `ack_qseq = queue_tail`
/// and `migration_final_snapshot_hash`, all in one transaction.
/// `live_memos = None` (the boot load was not trusted, see the module doc)
/// emits no memo op and verifies the backup memos.
///
/// Which boot load G7 passes (G4 delta-review rules): `Some(live)` for
/// `Loaded`, and for `Missing` with no recorded clipboard
/// (`manifest.clipboard_sha256 == None`); `None` only for `Corrupt` (bytes
/// preserved as `.corrupt-<ts>`) or for `Missing` with a recorded clipboard.
/// On `Unreadable` G7 does not call `finalize` at all that boot: the state
/// stays `verified`, the M3 queue keeps holding, and the next boot retries.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    webview_hash: &str,
    current: Option<(&[u8], &str)>,
    live_memos: Option<&[ClipEntry]>,
    queue_tail: u64,
) -> Result<Finalize, MigrationError> {
    finalize_inner(store, dir, vault, name_key, clock, webview_hash, current, live_memos, queue_tail, None)
}

fn read_u64(store: &Store, key: &str) -> Result<Option<u64>, MigrationError> {
    match store.meta_get(key)? {
        None => Ok(None),
        Some(text) => text.parse().map(Some).map_err(|_| corrupt(format!("{key} = {text:?}"))),
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_inner(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    webview_hash: &str,
    current: Option<(&[u8], &str)>,
    live_memos: Option<&[ClipEntry]>,
    queue_tail: u64,
    crash: Option<Crash>,
) -> Result<Finalize, MigrationError> {
    let state = MigrationState::load(store)?;
    if state == MigrationState::Native {
        let ack = read_u64(store, ACK_KEY)?.ok_or_else(|| corrupt("native without migration_ack_qseq"))?;
        return Ok(Finalize::Native { ack_qseq: ack });
    }
    if state != MigrationState::Verified {
        return Err(MigrationError::WrongState(state));
    }
    store.device_id(vault)?;
    let (manifest, backup, backup_memos) = read_backup(dir)?;
    let (source, final_hash) = match current {
        None if webview_hash != manifest.snapshot_hash => return Ok(Finalize::NeedSnapshot),
        None => (backup.clone(), webview_hash),
        Some((bytes, hash)) => (parse_webstore(bytes)?, hash),
    };
    let union = union_store(&backup, &source);
    let own = read_memo_ids(store)?;
    let p = store.projection(vault)?;
    let memos = live_memos.map(|live| MemoSource { live, backup: Some(&backup_memos) });
    let plan = plan_inner(&p, &source, memos, &own, name_key);
    let ids: BTreeSet<String> = own.union(&plan.memo_ids).cloned().collect();
    let meta = vec![
        (STATE_KEY.to_owned(), MigrationState::Native.as_str().to_owned()),
        (ACK_KEY.to_owned(), queue_tail.to_string()),
        (MEMO_IDS_KEY.to_owned(), serde_json::to_string(&ids)?),
        (FINAL_HASH_KEY.to_owned(), final_hash.to_owned()),
    ];
    let abort = crash_hit!(crash, InFinalizeTx);
    let mut mismatches = Vec::new();
    let (check_memos, check_backup) = match live_memos {
        Some(live) => (live, Some(backup_memos.as_slice())),
        None => (backup_memos.as_slice(), None),
    };
    let bodies = to_bodies(plan.ops, clock, now_ms());
    let (stored, _) = store.insert_local_batch_checked(vault, bodies, plan.blobs, meta, |after| {
        mismatches = check(after, &union, check_memos, check_backup, &mut Report::default());
        mismatches.is_empty() && !abort
    })?;
    if stored.is_some() {
        return Ok(Finalize::Native { ack_qseq: queue_tail });
    }
    if mismatches.is_empty() {
        #[cfg(test)]
        return Err(MigrationError::Crashed(Crash::InFinalizeTx));
        #[cfg(not(test))]
        return Err(corrupt("finalize transaction rolled back"));
    }
    let attempts = read_u64(store, ATTEMPTS_KEY)?.unwrap_or(0).saturating_add(1);
    store.meta_set(ATTEMPTS_KEY, &attempts.to_string())?;
    let attempts = u32::try_from(attempts).unwrap_or(u32::MAX);
    if attempts >= MAX_FINALIZE_ATTEMPTS {
        Ok(Finalize::Deferred { attempts, mismatches })
    } else {
        Ok(Finalize::VerifyFailed { attempts, mismatches })
    }
}

// ---------------------------------------------------------------------------
// M8 queue flush (native only)

/// A webview change queued during migration, by source ids (G8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub(crate) enum QueuedOp {
    MsgAdd { conv: String, msg: WsMsg, parent: Option<String> },
    ConvCreate { conv: String, title: String },
    Rename { conv: String, title: String },
    ConvDelete { conv: String },
    SetActive { conv: String, leaf: String },
    TrAdd { tr: WsTr },
    TrPin { tr: String, pinned: bool },
    TrDelete { tr: String },
    TrClear { observed_ids: Vec<String> },
}

/// Whether a queue item changed the projection (the tests' view of
/// [`QueueApply`]).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) changed: bool,
}

/// The outcome of one queue item (G8 E3), the `sync_queue_apply` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum QueueApply {
    /// Above the watermark: applied (`changed` when the projection moved);
    /// the watermark is now this qseq.
    Applied { changed: bool },
    /// The same qseq with the same op hash was already applied.
    Duplicate,
    /// At or below `watermark` with a different or unknown op hash: a reused
    /// qseq. Nothing was written; the webview renumbers from `watermark + 1`.
    StaleQseq { watermark: u64 },
}

/// The highest queue sequence number `apply_queue_item` has flushed, if any.
pub(crate) fn flushed_qseq(store: &Store) -> Result<Option<u64>, MigrationError> {
    read_u64(store, FLUSHED_KEY)
}

/// sha256 of the canonical JSON of a queued op (E3): what a replay of the
/// same qseq must match.
pub(crate) fn queue_op_hash(op: &QueuedOp) -> Result<[u8; 32], MigrationError> {
    let canonical = canonical_json(&serde_json::to_value(op)?).map_err(StoreError::from)?;
    Ok(Sha256::digest(canonical).into())
}

/// [`apply_queue_item`] for tests that only need whether anything changed:
/// a duplicate or stale item is `changed: false`.
#[cfg(test)]
pub(crate) fn apply_queued(
    store: &mut Store,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    qseq: u64,
    op: &QueuedOp,
) -> Result<Applied, MigrationError> {
    Ok(match apply_queue_item(store, vault, name_key, clock, qseq, op)? {
        QueueApply::Applied { changed } => Applied { changed },
        QueueApply::Duplicate | QueueApply::StaleQseq { .. } => Applied { changed: false },
    })
}

/// Applies queue item `qseq` (G8 flushes in qseq order and drops an item
/// only after this returns). Exactly once by qseq (E3): an item at or below
/// `migration_flushed_qseq` writes nothing and is `Duplicate` when its op
/// hash equals the one recorded for that qseq, else `StaleQseq`. A register
/// op (`Rename`, `TrPin`, `SetActive`) at or below `migration_ack_qseq` is
/// already reflected by the M6/M7 snapshot and is only counted as flushed;
/// add and delete ops follow the id-based no-op rule. The watermark and the
/// op hash (the newest [`super::store::QUEUE_HASH_WINDOW`] qseqs) are
/// written in the same transaction as the ops (AC 28 exactly once).
pub(crate) fn apply_queue_item(
    store: &mut Store,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    qseq: u64,
    op: &QueuedOp,
) -> Result<QueueApply, MigrationError> {
    let state = MigrationState::load(store)?;
    if state != MigrationState::Native {
        return Err(MigrationError::WrongState(state));
    }
    let op_hash = queue_op_hash(op)?;
    if let Some(watermark) = flushed_qseq(store)?.filter(|&flushed| qseq <= flushed) {
        return Ok(if store.queue_op_hash(qseq)? == Some(op_hash) {
            QueueApply::Duplicate
        } else {
            QueueApply::StaleQseq { watermark }
        });
    }
    let watermark = vec![(FLUSHED_KEY.to_owned(), qseq.to_string())];
    let register = matches!(op, QueuedOp::Rename { .. } | QueuedOp::TrPin { .. } | QueuedOp::SetActive { .. });
    if register && read_u64(store, ACK_KEY)?.is_some_and(|ack| qseq <= ack) {
        store.insert_queue_batch(vault, qseq, &op_hash, Vec::new(), Vec::new(), watermark)?;
        return Ok(QueueApply::Applied { changed: false });
    }
    let p = store.projection(vault)?;
    let conv_eid = |conv: &str| resolve_entity(&p, Kind::Conv, conv_entity_id(conv), conv);
    let tr_eid = |tr: &str| resolve_entity(&p, Kind::Tr, tr_entity_id(tr), tr);
    let mut pl = Planner { ops: Vec::new(), blobs: BTreeMap::new(), report: Report::default() };
    match op {
        QueuedOp::MsgAdd { conv, msg, parent } => {
            let cid = conv_eid(conv);
            let mid = resolve_msg(&p, &cid, conv, &msg.id);
            let present = match p.entities.get(&(Kind::Conv, cid.clone())) {
                Some(EntityState::Deleted) => true,
                Some(EntityState::Conv(c)) => c.messages.contains_key(&mid),
                _ => false,
            };
            if !present {
                let parent = parent.as_deref().map(|pid| resolve_msg(&p, &cid, conv, pid));
                let attachments: Vec<Value> =
                    msg.images.iter().map(|img| attachment(img, name_key, &mut pl.blobs, &mut pl.report)).collect();
                let payload = msg_payload(&cid, &mid, parent.as_deref(), msg, attachments);
                pl.push(Kind::Conv, &cid, "msg.add", payload);
            }
        }
        QueuedOp::ConvCreate { conv, title } => {
            let cid = conv_eid(conv);
            if !p.entities.contains_key(&(Kind::Conv, cid.clone())) {
                pl.push(Kind::Conv, &cid, "create", conv_create_payload(conv, title));
            }
        }
        QueuedOp::Rename { conv, title } => {
            let cid = conv_eid(conv);
            match p.entities.get(&(Kind::Conv, cid.clone())) {
                Some(EntityState::Conv(c)) if c.title != *title => {
                    pl.push(Kind::Conv, &cid, "rename", json!({"title": title}));
                }
                None => pl.push(Kind::Conv, &cid, "create", conv_create_payload(conv, title)),
                _ => {}
            }
        }
        QueuedOp::ConvDelete { conv } => {
            let cid = conv_eid(conv);
            if matches!(p.entities.get(&(Kind::Conv, cid.clone())), Some(EntityState::Conv(_))) {
                pl.push(Kind::Conv, &cid, "delete", json!({}));
            }
        }
        QueuedOp::SetActive { conv, leaf } => {
            let cid = conv_eid(conv);
            let mid = resolve_msg(&p, &cid, conv, leaf);
            if let Some(EntityState::Conv(c)) = p.entities.get(&(Kind::Conv, cid.clone())) {
                if c.messages.contains_key(&mid) && c.active_leaf.as_deref() != Some(mid.as_str()) {
                    pl.push(Kind::Conv, &cid, "set_active", json!({"leaf": mid}));
                }
            }
        }
        QueuedOp::TrAdd { tr } => {
            let tid = tr_eid(&tr.id);
            if !p.entities.contains_key(&(Kind::Tr, tid.clone())) {
                pl.push(Kind::Tr, &tid, "tr.add", tr_payload(tr));
            }
        }
        QueuedOp::TrPin { tr, pinned } => {
            let tid = tr_eid(tr);
            if matches!(p.entities.get(&(Kind::Tr, tid.clone())), Some(EntityState::Tr(t)) if t.pinned != *pinned) {
                pl.push(Kind::Tr, &tid, "tr.pin", json!({"pinned": pinned}));
            }
        }
        QueuedOp::TrDelete { tr } => {
            let tid = tr_eid(tr);
            if matches!(p.entities.get(&(Kind::Tr, tid.clone())), Some(EntityState::Tr(_))) {
                pl.push(Kind::Tr, &tid, "tr.delete", json!({}));
            }
        }
        QueuedOp::TrClear { observed_ids } => {
            let unique: BTreeSet<String> = observed_ids.iter().map(|tr| tr_eid(tr)).collect();
            for tid in unique {
                if matches!(p.entities.get(&(Kind::Tr, tid.clone())), Some(EntityState::Tr(_))) {
                    pl.push(Kind::Tr, &tid, "tr.delete", json!({}));
                }
            }
        }
    }
    // A no-op item still advances the watermark and records its hash.
    let changed = !pl.ops.is_empty();
    let bodies = to_bodies(pl.ops, clock, now_ms());
    store.insert_queue_batch(vault, qseq, &op_hash, bodies, pl.blobs.into_values().collect(), watermark)?;
    Ok(QueueApply::Applied { changed })
}

/// The acknowledged queue tail of the M7 cutover, if any.
pub(crate) fn ack_qseq(store: &Store) -> Result<Option<u64>, MigrationError> {
    read_u64(store, ACK_KEY)
}

/// The entity a queued source id names: `derived` (the uuidv5 of the source
/// id) when that exists, else the ref itself when it already names an entity
/// of `kind` (a load exposed it without a source id, G8), else `derived`.
/// So an id handed back by [`chat_store`] never creates a second entity.
fn resolve_entity(p: &Projection, kind: Kind, derived: String, raw: &str) -> String {
    if !p.entities.contains_key(&(kind, derived.clone())) && p.entities.contains_key(&(kind, raw.to_owned())) {
        raw.to_owned()
    } else {
        derived
    }
}

/// [`resolve_entity`] for a message of the conversation entity `cid`, named
/// by the queued conversation ref `conv` and message ref `msg`.
fn resolve_msg(p: &Projection, cid: &str, conv: &str, msg: &str) -> String {
    let derived = msg_entity_id(conv, msg);
    let has = |id: &str| matches!(p.entities.get(&(Kind::Conv, cid.to_owned())), Some(EntityState::Conv(c)) if c.messages.contains_key(id));
    if !has(&derived) && has(msg) {
        msg.to_owned()
    } else {
        derived
    }
}

// ---------------------------------------------------------------------------
// Native load (G8 `sync_chat_load`, pure)

/// A conversation as the webview stores it, read back from the projection.
/// `messages` is the active path, root first.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatConv {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) messages: Vec<WsMsg>,
}

/// The `sync_chat_load` result: a [`WebStore`] shape plus conversation times.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatStore {
    pub(crate) conversations: Vec<ChatConv>,
    pub(crate) history: Vec<WsTr>,
    /// Live conversations, messages, and translations exposed under their
    /// entity id because no op carried a verifiable `src_id`.
    #[serde(skip)]
    pub(crate) src_fallbacks: usize,
}

/// `src_id` candidates of the stored conv `create` and `msg.add` ops. A
/// candidate counts only when its uuidv5 is the entity id (a join copy or a
/// planted op cannot claim another source id).
#[derive(Debug, Default)]
pub(crate) struct SrcIndex {
    convs: BTreeMap<String, BTreeSet<String>>,
    msgs: BTreeMap<(String, String), BTreeSet<String>>,
    /// Earliest `create` HLC wall per conversation entity.
    created: BTreeMap<String, u64>,
}

impl SrcIndex {
    pub(crate) fn of(set: &OpSet) -> Self {
        let mut index = SrcIndex::default();
        for e in set.iter().filter(|e| e.body.kind == Kind::Conv) {
            let src = e.body.payload.get(SRC_ID).and_then(Value::as_str);
            match e.body.op_type.as_str() {
                "create" => {
                    let wall = e.body.hlc.wall_ms;
                    index.created.entry(e.body.entity_id.clone()).and_modify(|w| *w = (*w).min(wall)).or_insert(wall);
                    if let Some(src) = src {
                        index.convs.entry(e.body.entity_id.clone()).or_default().insert(src.to_owned());
                    }
                }
                "msg.add" => {
                    let mid = e.body.payload.get("msg_id").and_then(Value::as_str);
                    if let (Some(src), Some(mid)) = (src, mid) {
                        index.msgs.entry((e.body.entity_id.clone(), mid.to_owned())).or_default().insert(src.to_owned());
                    }
                }
                _ => {}
            }
        }
        index
    }

    fn conv_src(&self, cid: &str) -> Option<&str> {
        self.convs.get(cid)?.iter().find(|src| conv_entity_id(src) == cid).map(String::as_str)
    }

    /// `conv_key` is the id the conversation is exposed under.
    fn msg_src(&self, cid: &str, conv_key: &str, mid: &str) -> Option<&str> {
        self.msgs
            .get(&(cid.to_owned(), mid.to_owned()))?
            .iter()
            .find(|src| msg_entity_id(conv_key, src) == mid)
            .map(String::as_str)
    }
}

/// Root → `leaf` by `parent_id`; a missing parent ends the walk and a cycle
/// is cut at the first repeat.
fn active_path(c: &ConvState) -> Vec<(&str, &Message)> {
    let mut path = Vec::new();
    let mut seen = BTreeSet::new();
    let mut at = c.active_leaf.as_deref();
    while let Some(id) = at {
        let Some((key, m)) = c.messages.get_key_value(id) else { break };
        if !seen.insert(key.as_str()) {
            break;
        }
        path.push((key.as_str(), m));
        at = m.parent_id.as_deref();
    }
    path.reverse();
    path
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn number(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// An attachment as a webview image: the blob as base64, `""` when the blob
/// is evicted or not stored here.
fn chat_image<F>(att: &Value, fallback_id: String, blob: &mut F) -> Result<WsImage, StoreError>
where
    F: FnMut(&str) -> Result<Option<Vec<u8>>, StoreError>,
{
    let data = match att.get("blob_id").and_then(Value::as_str) {
        Some(id) => blob(id)?.map(|bytes| BASE64.encode(&bytes)).unwrap_or_default(),
        None => String::new(),
    };
    let id = att.get(SRC_ID).and_then(Value::as_str).map_or(fallback_id, str::to_owned);
    Ok(WsImage {
        id,
        mime_type: text(att, "mime"),
        data,
        width: number(att, "width"),
        height: number(att, "height"),
        name: att.get("name").and_then(Value::as_str).map(str::to_owned),
    })
}

/// Conversations (active paths) and translation history of `p` in webview
/// terms, under the source ids of [`SrcIndex`]; an entity without one keeps
/// its entity id and counts in `src_fallbacks`. Conversations newest first
/// by `updatedAt` (max message ts, else created); history newest first.
pub(crate) fn chat_store<F>(p: &Projection, index: &SrcIndex, mut blob: F) -> Result<ChatStore, StoreError>
where
    F: FnMut(&str) -> Result<Option<Vec<u8>>, StoreError>,
{
    let mut out = ChatStore::default();
    for ((kind, eid), state) in &p.entities {
        match (kind, state) {
            (Kind::Conv, EntityState::Conv(c)) => {
                let conv_key = match index.conv_src(eid) {
                    Some(src) => src.to_owned(),
                    None => {
                        out.src_fallbacks += 1;
                        eid.clone()
                    }
                };
                let mut messages = Vec::new();
                for (mid, m) in active_path(c) {
                    let id = match index.msg_src(eid, &conv_key, mid) {
                        Some(src) => src.to_owned(),
                        None => {
                            out.src_fallbacks += 1;
                            mid.to_owned()
                        }
                    };
                    let mut images = Vec::with_capacity(m.attachments.len());
                    for (n, att) in m.attachments.iter().enumerate() {
                        images.push(chat_image(att, format!("{id}:{n}"), &mut blob)?);
                    }
                    messages.push(WsMsg {
                        id,
                        role: m.role.clone(),
                        content: m.content.clone(),
                        ts: m.ts,
                        model: m.model.clone(),
                        images,
                    });
                }
                let first = c.messages.values().map(|m| m.ts).min();
                let last = c.messages.values().map(|m| m.ts).max();
                let created_at = first.or_else(|| index.created.get(eid).copied()).unwrap_or(0);
                out.conversations.push(ChatConv {
                    id: conv_key,
                    title: c.title.clone(),
                    created_at,
                    updated_at: last.unwrap_or(created_at),
                    messages,
                });
            }
            (Kind::Tr, EntityState::Tr(t)) => {
                let src = t.data.get(SRC_ID).and_then(Value::as_str).filter(|src| tr_entity_id(src) == *eid);
                let id = match src {
                    Some(src) => src.to_owned(),
                    None => {
                        out.src_fallbacks += 1;
                        eid.clone()
                    }
                };
                out.history.push(WsTr {
                    id,
                    ts: number(&t.data, "ts"),
                    source: text(&t.data, "source"),
                    target: text(&t.data, "target"),
                    from: text(&t.data, "from"),
                    to: text(&t.data, "to"),
                    model: text(&t.data, "model"),
                    pinned: t.pinned,
                });
            }
            _ => {}
        }
    }
    out.conversations.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    out.history.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Test-only crash wrappers

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_webstore_chunk_crashing(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    index: u32,
    total: u32,
    bytes: &[u8],
    snapshot_hash: &str,
    crash: Option<Crash>,
) -> Result<MigrationState, MigrationError> {
    push_inner(store, dir, vault, index, total, bytes, snapshot_hash, crash)
}

#[cfg(test)]
pub(crate) fn import_crashing(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    now: u64,
    crash: Option<Crash>,
) -> Result<Report, MigrationError> {
    import_inner(store, dir, vault, name_key, clock, now, crash)
}

#[cfg(test)]
pub(crate) fn verify_crashing(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    crash: Option<Crash>,
) -> Result<Verify, MigrationError> {
    verify_inner(store, dir, vault, crash)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_crashing(
    store: &mut Store,
    dir: &MigrationDir,
    vault: &str,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    webview_hash: &str,
    current: Option<(&[u8], &str)>,
    live_memos: Option<&[ClipEntry]>,
    queue_tail: u64,
    crash: Option<Crash>,
) -> Result<Finalize, MigrationError> {
    finalize_inner(store, dir, vault, name_key, clock, webview_hash, current, live_memos, queue_tail, crash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{make_preview, save_to};
    use crate::sync::crypto::{derive_subkey, Purpose};
    use crate::sync::memo_bridge::mirror;
    use crate::sync::rotation::UploadState;
    use crate::sync::sim_tests::{create, identity, op, TempDir, EPOCH, M, VAULT};
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};

    fn name_key(seed: u8) -> SecretKey {
        derive_subkey(&SecretKey::from_bytes([seed; 32]), VAULT, EPOCH, Purpose::BlobName)
    }

    fn entry(id: &str, text: &str, pinned: bool, note: Option<&str>, kind: EntryKind) -> ClipEntry {
        ClipEntry {
            id: id.to_owned(),
            text: text.to_owned(),
            preview: make_preview(text),
            ts: 100,
            pinned,
            note: note.map(str::to_owned),
            kind,
            conflict_of: None,
        }
    }

    fn memo(id: &str, text: &str, pinned: bool, note: Option<&str>) -> ClipEntry {
        entry(id, text, pinned, note, EntryKind::Memo)
    }

    fn msg(id: &str, role: &str, content: &str, ts: u64) -> WsMsg {
        WsMsg { id: id.to_owned(), role: role.to_owned(), content: content.to_owned(), ts, ..Default::default() }
    }

    fn tr(id: &str, source: &str, pinned: bool) -> WsTr {
        WsTr {
            id: id.to_owned(),
            ts: 5,
            source: source.to_owned(),
            target: format!("{source}-t"),
            from: "ko".to_owned(),
            to: "en".to_owned(),
            model: "m".to_owned(),
            pinned,
        }
    }

    fn source() -> WebStore {
        let mut answer = msg("m2", "assistant", "hello", 2);
        answer.model = Some("gpt".to_owned());
        WebStore {
            conversations: vec![
                WsConv { id: "c1".to_owned(), title: "first".to_owned(), messages: vec![msg("m1", "user", "hi", 1), answer] },
                WsConv { id: "c2".to_owned(), title: "second".to_owned(), messages: vec![msg("n1", "user", "yo", 3)] },
            ],
            history: vec![tr("t1", "a", false), tr("t2", "b", true)],
        }
    }

    fn clip_entries() -> Vec<ClipEntry> {
        vec![
            memo("a", "memo a", false, None),
            entry("k", "a clip", false, None, EntryKind::Clip),
            memo("b", "memo b", true, Some("note b")),
        ]
    }

    struct Env {
        dir: MigrationDir,
        db: PathBuf,
        store: Store,
        clipboard_original: Vec<u8>,
        tmp: TempDir,
        /// The device clock (G7 owns it in production).
        clock: HlcClock,
    }

    fn env(tag: &str, entries: &[ClipEntry]) -> Env {
        let tmp = TempDir::new(tag);
        let clipboard_src = tmp.0.join("clipboard.json");
        save_to(&clipboard_src, entries, 500, false).unwrap();
        let dir = MigrationDir::new(tmp.0.join("sync-migration"), clipboard_src.clone());
        let db = tmp.0.join("sync").join("sync.db");
        let mut store = Store::open(&db).unwrap();
        store.init_identity(&identity("a")).unwrap();
        store.set_active_vault(VAULT).unwrap();
        let clipboard_original = fs::read(&clipboard_src).unwrap();
        Env { dir, db, store, clipboard_original, tmp, clock: HlcClock::default() }
    }

    impl Env {
        /// A process restart: the store is closed and reopened from disk.
        fn reopen(&mut self) {
            self.store = Store::open_in_memory().unwrap();
            self.store = Store::open(&self.db).unwrap();
        }

        fn state(&self) -> MigrationState {
            MigrationState::load(&self.store).unwrap()
        }
    }

    fn ws_bytes(ws: &WebStore) -> Vec<u8> {
        serde_json::to_vec(ws).unwrap()
    }

    const SNAP: &str = "snap-1";

    fn push_all(e: &mut Env, bytes: &[u8], crash: Option<Crash>) -> Result<MigrationState, MigrationError> {
        let third = bytes.len().div_ceil(3);
        let chunks: Vec<&[u8]> = bytes.chunks(third).collect();
        let total = chunks.len() as u32;
        let mut last = MigrationState::None;
        for (i, chunk) in chunks.iter().enumerate() {
            last = push_webstore_chunk_crashing(&mut e.store, &e.dir, VAULT, i as u32, total, chunk, SNAP, crash)?;
        }
        Ok(last)
    }

    /// Runs the boot sequence from whatever state is stored up to `native`.
    /// Returns the verify report when this run verified.
    fn drive(e: &mut Env, bytes: &[u8], crash: Option<Crash>) -> Result<Option<Report>, MigrationError> {
        let key = name_key(1);
        let mut report = None;
        for _ in 0..12 {
            match e.state() {
                MigrationState::None => {
                    // The webview restarts its push from chunk 0; a stale
                    // complete progress rejects it once and resets.
                    match push_all(e, bytes, crash) {
                        Err(MigrationError::Chunk(_)) => {
                            push_all(e, bytes, crash)?;
                        }
                        other => {
                            other?;
                        }
                    }
                }
                MigrationState::BackedUp | MigrationState::Importing => {
                    import_crashing(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, 1_000, crash)?;
                }
                MigrationState::Imported => match verify_crashing(&mut e.store, &e.dir, VAULT, crash)? {
                    Verify::Passed(r) => report = Some(r),
                    Verify::Failed(bad) => panic!("verify failed: {bad:?}"),
                },
                MigrationState::Verified => {
                    let snap = serde_json::from_slice::<Manifest>(&fs::read(e.dir.manifest()).unwrap())
                        .unwrap()
                        .snapshot_hash;
                    let live: Vec<ClipEntry> = clip_entries();
                    let out = finalize_crashing(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, &snap, None, Some(&live), 9, crash)?;
                    assert_eq!(out, Finalize::Native { ack_qseq: 9 });
                }
                MigrationState::Native => return Ok(report),
            }
        }
        panic!("migration did not converge");
    }

    /// Steps up to `verified` without finalizing.
    fn to_verified(e: &mut Env, bytes: &[u8]) {
        assert_eq!(push_all(e, bytes, None).unwrap(), MigrationState::BackedUp);
        import(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, 1_000).unwrap();
        assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
        assert_eq!(e.state(), MigrationState::Verified);
    }

    /// (live conversations, messages, live translations, live memos).
    fn counts(p: &Projection) -> (usize, usize, usize, usize) {
        let (mut c, mut m, mut t, mut n) = (0, 0, 0, 0);
        for state in p.entities.values() {
            match state {
                EntityState::Conv(conv) => {
                    c += 1;
                    m += conv.messages.len();
                }
                EntityState::Tr(_) => t += 1,
                EntityState::Memo(_) => n += 1,
                _ => {}
            }
        }
        (c, m, t, n)
    }

    /// (kind, entity, type, msg_id) of every stored op.
    fn op_sigs(store: &Store) -> Vec<(Kind, String, String, String)> {
        let (set, _) = store.load_set(VAULT).unwrap();
        let mut sigs: Vec<_> = set
            .iter()
            .map(|e| {
                let sub = e.body.payload.get("msg_id").and_then(Value::as_str).unwrap_or_default().to_owned();
                (e.body.kind, e.body.entity_id.clone(), e.body.op_type.clone(), sub)
            })
            .collect();
        sigs.sort();
        sigs
    }

    /// No message, conversation, translation, or memo was created twice.
    fn assert_no_duplicates(store: &Store) {
        let sigs: Vec<_> = op_sigs(store)
            .into_iter()
            .filter(|(_, _, t, _)| matches!(t.as_str(), "create" | "msg.add" | "tr.add"))
            .collect();
        let unique: BTreeSet<_> = sigs.iter().cloned().collect();
        assert_eq!(sigs.len(), unique.len(), "duplicate creates: {sigs:?}");
    }

    fn op_count(store: &Store) -> usize {
        op_sigs(store).len()
    }

    fn conv<'a>(p: &'a Projection, id: &str) -> &'a crate::sync::merge::ConvState {
        match p.entities.get(&(Kind::Conv, conv_entity_id(id))) {
            Some(EntityState::Conv(c)) => c,
            other => panic!("conversation {id}: {other:?}"),
        }
    }

    #[test]
    fn deterministic_ids_match_the_shared_uuidv5_vectors() {
        // Python: uuid.uuid5(UUID("6f1c2d3e-5a4b-4c8d-9e0f-a1b2c3d4e5f6"), name)
        assert_eq!(ID_NAMESPACE.hyphenated().to_string(), "6f1c2d3e-5a4b-4c8d-9e0f-a1b2c3d4e5f6");
        assert_eq!(conv_entity_id("c1"), "16efa2a9-030b-5b25-bc71-affdd7d5dd48");
        assert_eq!(msg_entity_id("c1", "m1"), "8c1a1ac8-2d67-5dc5-aca3-8e5a5d21b44b");
        assert_eq!(tr_entity_id("t1"), "7fb385e6-0b77-58f4-892b-314dc9a4b812");
        assert_eq!(memo_entity_id("x"), "dfc94200-a5e2-5669-9a44-4cb4e805e742");
    }

    #[test]
    fn ac9_28_crash_at_every_point_then_rerun_reaches_native_exactly_once() {
        let ws = source();
        let bytes = ws_bytes(&ws);
        for at in [
            Crash::AfterBackupFiles,
            Crash::AfterImportingSet,
            Crash::InImportTx,
            Crash::AfterImported,
            Crash::AfterVerified,
            Crash::InFinalizeTx,
        ] {
            let mut e = env("mig-crash", &clip_entries());
            match drive(&mut e, &bytes, Some(at)) {
                Err(MigrationError::Crashed(got)) => assert_eq!(got, at),
                other => panic!("{at:?}: expected a crash, got {other:?}"),
            }
            let crashed_state = e.state();
            assert_ne!(crashed_state, MigrationState::Native, "{at:?}");
            e.reopen();
            let report = drive(&mut e, &bytes, None).unwrap();
            assert_eq!(e.state(), MigrationState::Native, "{at:?}");
            if let Some(r) = report {
                assert_eq!((r.conversations, r.messages, r.translations, r.memos), (2, 3, 2, 2), "{at:?}");
            }
            let p = e.store.projection(VAULT).unwrap();
            assert_eq!(counts(&p), (2, 3, 2, 2), "{at:?}");
            assert_no_duplicates(&e.store);
            // Originals and backups are byte-identical.
            assert_eq!(fs::read(&e.dir.clipboard_src).unwrap(), e.clipboard_original, "{at:?}");
            assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), e.clipboard_original, "{at:?}");
            assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes, "{at:?}");
            assert_eq!(e.store.meta_get(ACK_KEY).unwrap().as_deref(), Some("9"));
        }
    }

    #[test]
    fn crash_before_backup_state_leaves_none_and_the_rerun_completes() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-none", &clip_entries());
        let out = push_all(&mut e, &bytes, Some(Crash::AfterBackupFiles));
        assert!(matches!(out, Err(MigrationError::Crashed(Crash::AfterBackupFiles))), "{out:?}");
        e.reopen();
        assert_eq!(e.state(), MigrationState::None);
        assert!(e.store.meta_get(STATE_KEY).unwrap().is_none());
        // Nothing may read the half-made backup.
        assert!(matches!(
            import(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, 1),
            Err(MigrationError::WrongState(MigrationState::None))
        ));
        assert_eq!(op_count(&e.store), 0);
        // Re-sending the last chunk is a no-op append that finishes the backup.
        let third = bytes.len().div_ceil(3);
        let chunks: Vec<&[u8]> = bytes.chunks(third).collect();
        let last = chunks.len() as u32 - 1;
        let state =
            push_webstore_chunk(&mut e.store, &e.dir, VAULT, last, last + 1, chunks[last as usize], SNAP).unwrap();
        assert_eq!(state, MigrationState::BackedUp);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
    }

    #[test]
    fn unreadable_clipboard_source_writes_no_backup_and_stays_none() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-unreadable", &clip_entries());
        // A directory where the file should be: not NotFound, so not "missing".
        let dir = MigrationDir::new(e.dir.root.clone(), e.tmp.0.clone());
        let third = bytes.len().div_ceil(3);
        let chunks: Vec<&[u8]> = bytes.chunks(third).collect();
        let total = chunks.len() as u32;
        let mut out = Ok(MigrationState::None);
        for (i, chunk) in chunks.iter().enumerate() {
            out = push_webstore_chunk(&mut e.store, &dir, VAULT, i as u32, total, chunk, SNAP);
        }
        assert!(matches!(out, Err(MigrationError::Io(_))), "{out:?}");
        assert_eq!(e.state(), MigrationState::None);
        assert!(!dir.manifest().exists() && !dir.webstore().exists() && !dir.clipboard_backup().exists());
    }

    #[test]
    fn missing_clipboard_source_has_no_backup_and_no_memos() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-missing", &[]);
        let dir = MigrationDir::new(e.dir.root.clone(), e.tmp.0.join("absent.json"));
        e.dir = dir;
        drive(&mut e, &bytes, None).unwrap();
        let manifest: Manifest = serde_json::from_slice(&fs::read(e.dir.manifest()).unwrap()).unwrap();
        assert_eq!(manifest.clipboard_sha256, None);
        assert!(!e.dir.clipboard_backup().exists());
        assert!(!e.dir.clipboard_src.exists());
        // drive() finalizes with clip_entries() as the live memos.
        assert_eq!(counts(&e.store.projection(VAULT).unwrap()), (2, 3, 2, 2));
    }

    #[test]
    fn crash_after_backed_up_resumes_from_the_backup_files() {
        let ws = source();
        let bytes = ws_bytes(&ws);
        let mut e = env("mig-resume", &clip_entries());
        assert_eq!(push_all(&mut e, &bytes, None).unwrap(), MigrationState::BackedUp);
        // The originals change after the backup; the import must not see it.
        let edited = vec![memo("a", "edited after backup", false, None)];
        save_to(&e.dir.clipboard_src, &edited, 500, false).unwrap();
        let edited_bytes = fs::read(&e.dir.clipboard_src).unwrap();
        e.reopen();
        assert_eq!(e.state(), MigrationState::BackedUp);
        import(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, 1_000).unwrap();
        let p = e.store.projection(VAULT).unwrap();
        match p.entities.get(&(Kind::Memo, memo_entity_id("a"))) {
            Some(EntityState::Memo(m)) => assert_eq!(m.text, "memo a"),
            other => panic!("{other:?}"),
        }
        assert_eq!(counts(&p), (2, 3, 2, 2));
        assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), e.clipboard_original);
        assert_eq!(fs::read(&e.dir.clipboard_src).unwrap(), edited_bytes);
    }

    #[test]
    fn push_after_imported_is_ignored() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-ignored", &clip_entries());
        push_all(&mut e, &bytes, None).unwrap();
        import(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, 1_000).unwrap();
        assert_eq!(e.state(), MigrationState::Imported);
        let ops = op_count(&e.store);
        let other = ws_bytes(&WebStore::default());
        let state = push_webstore_chunk(&mut e.store, &e.dir, VAULT, 0, 1, &other, "snap-2").unwrap();
        assert_eq!(state, MigrationState::Imported);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
        let manifest: Manifest = serde_json::from_slice(&fs::read(e.dir.manifest()).unwrap()).unwrap();
        assert_eq!(manifest.snapshot_hash, SNAP);
        assert_eq!(op_count(&e.store), ops);
        assert_eq!(e.state(), MigrationState::Imported);
    }

    #[test]
    fn chunk_protocol_appends_in_order_and_resets_on_anything_else() {
        let bytes = ws_bytes(&source());
        let third = bytes.len().div_ceil(3);
        let c: Vec<&[u8]> = bytes.chunks(third).collect();
        assert_eq!(c.len(), 3);
        let mut e = env("mig-chunks", &clip_entries());
        let push = |e: &mut Env, i: u32, total: u32, b: &[u8]| {
            push_webstore_chunk(&mut e.store, &e.dir, VAULT, i, total, b, SNAP)
        };
        let partial = |e: &Env| fs::read(e.dir.partial()).unwrap();

        assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None);
        assert_eq!(partial(&e), c[0]);
        // Identical re-send: no-op.
        assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None);
        assert_eq!(partial(&e), c[0]);
        assert_eq!(push(&mut e, 1, 3, c[1]).unwrap(), MigrationState::None);
        assert_eq!(partial(&e), [c[0], c[1]].concat());
        // Re-send with other bytes: error and reset.
        assert!(matches!(push(&mut e, 1, 3, b"other"), Err(MigrationError::Chunk(_))));
        assert!(partial(&e).is_empty());
        // Out of order (skipping ahead): error and reset.
        assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None);
        assert!(matches!(push(&mut e, 2, 3, c[2]), Err(MigrationError::Chunk(_))));
        assert!(partial(&e).is_empty());
        // A changed total mid-push: error and reset.
        assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None);
        assert!(matches!(push(&mut e, 1, 4, c[1]), Err(MigrationError::Chunk(_))));
        assert!(matches!(push(&mut e, 0, 0, c[0]), Err(MigrationError::Chunk(_))));
        assert_eq!(e.state(), MigrationState::None);
        assert!(!e.dir.manifest().exists() && !e.dir.webstore().exists());
        // In order: the state moves only with the last chunk.
        assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None);
        assert_eq!(push(&mut e, 1, 3, c[1]).unwrap(), MigrationState::None);
        assert_eq!(e.state(), MigrationState::None);
        assert!(!e.dir.manifest().exists());
        assert_eq!(push(&mut e, 2, 3, c[2]).unwrap(), MigrationState::BackedUp);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
        let manifest: Manifest = serde_json::from_slice(&fs::read(e.dir.manifest()).unwrap()).unwrap();
        assert_eq!(manifest.webstore_sha256, sha256_hex(&bytes));
        assert_eq!(manifest.clipboard_sha256, Some(sha256_hex(&e.clipboard_original)));
        assert_eq!(manifest.snapshot_hash, SNAP);
        // A snapshot that does not parse never becomes the backup.
        let mut e = env("mig-badjson", &clip_entries());
        assert!(matches!(push(&mut e, 0, 1, b"{not json"), Err(MigrationError::Chunk(_))));
        assert_eq!(e.state(), MigrationState::None);
        assert!(!e.dir.webstore().exists());
    }

    #[test]
    fn m6_hash_mismatch_needs_a_snapshot_then_the_delta_lands_exactly_once() {
        let ws = source();
        let bytes = ws_bytes(&ws);
        let mut e = env("mig-m6", &clip_entries());
        to_verified(&mut e, &bytes);
        let before = op_count(&e.store);
        let key = name_key(1);
        let live = clip_entries();
        let out = finalize(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, "changed", None, Some(&live), 4).unwrap();
        assert_eq!(out, Finalize::NeedSnapshot);
        assert_eq!(op_count(&e.store), before);
        assert_eq!(e.state(), MigrationState::Verified);
        assert_eq!(e.store.meta_get(ATTEMPTS_KEY).unwrap(), None);

        // Newer snapshot: m2 regenerated as sibling m2r (branch), m3 after it,
        // a new translation, a new conversation, and a renamed one.
        let mut current = ws.clone();
        current.conversations[0].messages =
            vec![msg("m1", "user", "hi", 1), msg("m2r", "assistant", "hello again", 4), msg("m3", "user", "more", 5)];
        current.conversations[1].title = "second renamed".to_owned();
        current.conversations.push(WsConv {
            id: "c3".to_owned(),
            title: "third".to_owned(),
            messages: vec![msg("x1", "user", "new", 6)],
        });
        current.history.push(tr("t3", "c", false));
        let cur_bytes = ws_bytes(&current);
        let out = finalize(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, "changed", Some((&cur_bytes, "changed")), Some(&live), 7)
            .unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 7 });
        assert_eq!(e.state(), MigrationState::Native);
        assert_eq!(e.store.meta_get(ACK_KEY).unwrap().as_deref(), Some("7"));

        let p = e.store.projection(VAULT).unwrap();
        let c1 = conv(&p, "c1");
        let id = |m: &str| msg_entity_id("c1", m);
        assert_eq!(c1.messages.len(), 4);
        assert_eq!(c1.messages[&id("m2")].parent_id, Some(id("m1")));
        assert_eq!(c1.messages[&id("m2r")].parent_id, Some(id("m1")));
        assert_eq!(c1.messages[&id("m3")].parent_id, Some(id("m2r")));
        assert_eq!(c1.active_leaf, Some(id("m3")));
        assert_eq!(conv(&p, "c2").title, "second renamed");
        assert_eq!(conv(&p, "c3").messages.len(), 1);
        assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t3"))), Some(EntityState::Tr(_))));
        assert_eq!(counts(&p), (3, 6, 3, 2));
        assert_no_duplicates(&e.store);
        let sigs = op_sigs(&e.store);
        for (kind, entity, op_type, sub) in [
            (Kind::Conv, conv_entity_id("c1"), "msg.add", id("m2r")),
            (Kind::Conv, conv_entity_id("c1"), "msg.add", id("m3")),
            (Kind::Conv, conv_entity_id("c3"), "msg.add", msg_entity_id("c3", "x1")),
            (Kind::Tr, tr_entity_id("t3"), "tr.add", String::new()),
        ] {
            let n = sigs.iter().filter(|s| (s.0, &s.1, s.2.as_str(), &s.3) == (kind, &entity, op_type, &sub)).count();
            assert_eq!(n, 1, "{entity} {op_type} {sub}");
        }
        // Native is terminal: a repeated finalize writes nothing.
        let ops = op_count(&e.store);
        let again = finalize(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, "changed", Some((&cur_bytes, "changed")), Some(&live), 8);
        assert_eq!(again.unwrap(), Finalize::Native { ack_qseq: 7 });
        assert_eq!(op_count(&e.store), ops);
    }

    #[test]
    fn finalize_verify_failure_rolls_back_and_defers_at_three_attempts() {
        let ws = source();
        let bytes = ws_bytes(&ws);
        let mut e = env("mig-defer", &clip_entries());
        to_verified(&mut e, &bytes);
        // Another device already added message "cx" with other content.
        let (cid, mid) = (conv_entity_id("c1"), msg_entity_id("c1", "cx"));
        let planted = op(
            900,
            1_000,
            Kind::Conv,
            &cid,
            "msg.add",
            json!({"conv_id": cid, "msg_id": mid, "parent_id": msg_entity_id("c1", "m2"), "role": "user",
                   "content": "planted", "attachments": [], "ts": 9}),
        );
        e.store.insert_local_op(VAULT, planted, UploadState::Never).unwrap();
        let before = op_count(&e.store);
        let hash_before = e.store.projection(VAULT).unwrap().hash;

        let mut current = ws.clone();
        current.conversations[0].messages.push(msg("cx", "user", "real", 9));
        current.history.push(tr("t3", "c", false));
        let cur_bytes = ws_bytes(&current);
        let live = clip_entries();
        let key = name_key(1);
        for attempt in 1..=4u32 {
            let out = finalize(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, "h", Some((&cur_bytes, "h")), Some(&live), 3).unwrap();
            match out {
                Finalize::VerifyFailed { attempts, mismatches } if attempt < 3 => {
                    assert_eq!(attempts, attempt);
                    assert!(mismatches.iter().any(|m| m.contains("c1/cx")), "{mismatches:?}");
                }
                Finalize::Deferred { attempts, mismatches } if attempt >= 3 => {
                    assert_eq!(attempts, attempt);
                    assert!(!mismatches.is_empty());
                }
                other => panic!("attempt {attempt}: {other:?}"),
            }
            assert_eq!(e.state(), MigrationState::Verified);
            assert_eq!(op_count(&e.store), before, "rolled back");
            assert_eq!(e.store.projection(VAULT).unwrap().hash, hash_before);
            assert_eq!(e.store.meta_get(ACK_KEY).unwrap(), None);
            assert_eq!(e.store.meta_get(ATTEMPTS_KEY).unwrap(), Some(attempt.to_string()));
        }
        let p = e.store.projection(VAULT).unwrap();
        assert!(!p.entities.contains_key(&(Kind::Tr, tr_entity_id("t3"))));
    }

    #[test]
    fn queue_flush_crash_halfway_then_full_reflush_applies_each_op_once() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-queue", &clip_entries());
        let key = name_key(1);
        // Not native yet: the queue is held.
        assert!(matches!(
            apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 1, &QueuedOp::ConvDelete { conv: "c2".to_owned() }),
            Err(MigrationError::WrongState(MigrationState::None))
        ));
        drive(&mut e, &bytes, None).unwrap();
        let queue = vec![
            QueuedOp::ConvCreate { conv: "c9".to_owned(), title: "q".to_owned() },
            QueuedOp::MsgAdd { conv: "c9".to_owned(), msg: msg("q1", "user", "one", 10), parent: None },
            QueuedOp::MsgAdd { conv: "c9".to_owned(), msg: msg("q2", "assistant", "two", 11), parent: Some("q1".to_owned()) },
            QueuedOp::Rename { conv: "c9".to_owned(), title: "q renamed".to_owned() },
            QueuedOp::SetActive { conv: "c9".to_owned(), leaf: "q1".to_owned() },
            QueuedOp::TrAdd { tr: tr("t9", "z", false) },
            QueuedOp::TrPin { tr: "t9".to_owned(), pinned: true },
            QueuedOp::TrDelete { tr: "t1".to_owned() },
            QueuedOp::TrClear { observed_ids: vec!["t2".to_owned(), "t1".to_owned(), "t2".to_owned(), "nope".to_owned()] },
            QueuedOp::ConvDelete { conv: "c2".to_owned() },
            QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m4", "user", "late", 12), parent: Some("m2".to_owned()) },
        ];
        // The queue round-trips through the webview JSON shape.
        let text = serde_json::to_string(&queue).unwrap();
        assert!(text.contains(r#""op":"tr_clear""#) && text.contains(r#""observedIds""#), "{text}");
        assert_eq!(serde_json::from_str::<Vec<QueuedOp>>(&text).unwrap(), queue);

        for (q, qseq) in queue[..6].iter().zip(10..) {
            assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, qseq, q).unwrap(), Applied { changed: true }, "{q:?}");
        }
        e.reopen(); // crash mid-flush
        for (q, qseq) in queue.iter().zip(10..) {
            apply_queued(&mut e.store, VAULT, &key, &mut e.clock, qseq, q).unwrap();
        }
        assert_no_duplicates(&e.store);
        let sigs = op_sigs(&e.store);
        let n = |kind: Kind, entity: &str, t: &str| sigs.iter().filter(|s| s.0 == kind && s.1 == entity && s.2 == t).count();
        assert_eq!(n(Kind::Conv, &conv_entity_id("c9"), "create"), 1);
        assert_eq!(n(Kind::Conv, &conv_entity_id("c9"), "rename"), 1);
        assert_eq!(n(Kind::Conv, &conv_entity_id("c9"), "msg.add"), 2);
        assert_eq!(n(Kind::Tr, &tr_entity_id("t9"), "tr.pin"), 1);
        assert_eq!(n(Kind::Tr, &tr_entity_id("t1"), "tr.delete"), 1);
        assert_eq!(n(Kind::Tr, &tr_entity_id("t2"), "tr.delete"), 1);
        assert_eq!(n(Kind::Tr, &tr_entity_id("nope"), "tr.delete"), 0);
        assert_eq!(n(Kind::Conv, &conv_entity_id("c2"), "delete"), 1);
        assert_eq!(n(Kind::Conv, &conv_entity_id("c1"), "msg.add"), 3);

        let p = e.store.projection(VAULT).unwrap();
        let c9 = conv(&p, "c9");
        assert_eq!(c9.title, "q renamed");
        assert_eq!(c9.messages[&msg_entity_id("c9", "q2")].parent_id, Some(msg_entity_id("c9", "q1")));
        assert_eq!(c9.active_leaf, Some(msg_entity_id("c9", "q1")));
        assert_eq!(p.entities.get(&(Kind::Conv, conv_entity_id("c2"))), Some(&EntityState::Deleted));
        assert_eq!(p.entities.get(&(Kind::Tr, tr_entity_id("t1"))), Some(&EntityState::Deleted));
        assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t9"))), Some(EntityState::Tr(t)) if t.pinned));
        // Re-applying everything changes nothing.
        let ops = op_count(&e.store);
        for (q, qseq) in queue.iter().zip(10..) {
            assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, qseq, q).unwrap(), Applied { changed: false }, "{q:?}");
        }
        assert_eq!(op_count(&e.store), ops);
    }

    #[test]
    fn images_evicted_and_invalid_become_metadata_and_blob_ids_are_keyed() {
        let png = b"\x89PNG fake image bytes".to_vec();
        let img = |id: &str, data: String| WsImage {
            id: id.to_owned(),
            mime_type: "image/png".to_owned(),
            data,
            width: 3,
            height: 2,
            name: Some(format!("{id}.png")),
        };
        let mut m = msg("m1", "user", "look", 1);
        m.images = vec![img("ev", String::new()), img("bad", "!!not base64!!".to_owned()), img("ok", BASE64.encode(&png))];
        let ws = WebStore {
            conversations: vec![WsConv { id: "c1".to_owned(), title: "t".to_owned(), messages: vec![m] }],
            history: vec![],
        };
        let empty = Store::open_in_memory().unwrap().projection(VAULT).unwrap();
        let k1 = name_key(1);
        let plan = plan_import(&empty, &ws, &[], &BTreeSet::new(), &k1);
        assert_eq!((plan.report.images, plan.report.evicted_images, plan.report.invalid_images), (1, 1, 1));
        assert!(plan.ops.iter().all(|o| !o.op_type.contains("delete")), "{:?}", plan.ops);
        assert_eq!(plan.blobs.len(), 1);
        let blob = &plan.blobs[0];
        assert_eq!(blob.blob_id, hex(&hmac_sha256(&k1, &png)));
        assert_eq!((blob.mime.as_str(), blob.bytes.as_slice()), ("image/png", png.as_slice()));
        assert_eq!(blob.sha256, <[u8; 32]>::from(Sha256::digest(&png)));
        let add = plan.ops.iter().find(|o| o.op_type == "msg.add").unwrap();
        let atts = add.payload["attachments"].as_array().unwrap();
        // G8: each attachment also carries its webview id as `src_id`.
        assert_eq!(
            atts[0],
            json!({"evicted": true, "mime": "image/png", "width": 3, "height": 2, "name": "ev.png", "src_id": "ev"})
        );
        assert_eq!(
            atts[1],
            json!({"evicted": true, "mime": "image/png", "width": 3, "height": 2, "name": "bad.png", "src_id": "bad"})
        );
        assert_eq!(
            atts[2],
            json!({"blob_id": blob.blob_id, "mime": "image/png", "width": 3, "height": 2, "name": "ok.png", "src_id": "ok"})
        );
        // Deterministic for the same key, different under another key.
        assert_eq!(plan_import(&empty, &ws, &[], &BTreeSet::new(), &k1).blobs[0].blob_id, blob.blob_id);
        assert_ne!(plan_import(&empty, &ws, &[], &BTreeSet::new(), &name_key(2)).blobs[0].blob_id, blob.blob_id);

        // Through import: the blob row is stored and mapped `pending`.
        let mut e = env("mig-images", &clip_entries());
        drive(&mut e, &ws_bytes(&ws), None).unwrap();
        let state: String = e
            .store
            .conn()
            .query_row("SELECT remote_state FROM vault_blob_map WHERE vault_id = ?1 AND blob_id = ?2", [VAULT, blob.blob_id.as_str()], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "pending");
        let stored: Vec<u8> =
            e.store.conn().query_row("SELECT bytes FROM blobs WHERE blob_id = ?1", [&blob.blob_id], |r| r.get(0)).unwrap();
        assert_eq!(stored, png);
        let p = e.store.projection(VAULT).unwrap();
        assert_eq!(conv(&p, "c1").messages[&msg_entity_id("c1", "m1")].attachments.len(), 3);
        assert!(op_sigs(&e.store).iter().all(|s| !s.2.contains("delete")));
    }

    #[test]
    fn memo_delta_deletes_only_absent_own_imports_never_remote_memos() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-memo", &clip_entries());
        // A memo from another device, never in this device's backup.
        e.store.insert_local_op(VAULT, create(800, 1_000, M, "remote memo"), UploadState::Never).unwrap();
        to_verified(&mut e, &bytes);
        let own = read_memo_ids(&e.store).unwrap();
        assert_eq!(own, BTreeSet::from([memo_entity_id("a"), memo_entity_id("b")]));

        // Live clipboard: "a" edited, "b" deleted, a new memo "n"; the remote
        // memo is not in it either.
        let live = vec![
            memo("a", "memo a v2", false, None),
            entry("k", "a clip", false, None, EntryKind::Clip),
            memo("n", "new memo", false, Some("nn")),
        ];
        let p = e.store.projection(VAULT).unwrap();
        let mut stale = own.clone();
        stale.insert(memo_entity_id("never-existed"));
        let plan = plan_import(&p, &WebStore::default(), &live, &stale, &name_key(1));
        let deletes: Vec<&str> =
            plan.ops.iter().filter(|o| o.op_type == "delete").map(|o| o.entity_id.as_str()).collect();
        assert_eq!(deletes, vec![memo_entity_id("b").as_str()]);

        let snap = serde_json::from_slice::<Manifest>(&fs::read(e.dir.manifest()).unwrap()).unwrap().snapshot_hash;
        let out = finalize(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, &snap, None, Some(&live), 2).unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 2 });
        let p = e.store.projection(VAULT).unwrap();
        let memo_state = |id: &str| p.entities.get(&(Kind::Memo, id.to_owned())).cloned();
        assert!(matches!(memo_state(&memo_entity_id("a")), Some(EntityState::Memo(m)) if m.text == "memo a v2"));
        assert_eq!(memo_state(&memo_entity_id("b")), Some(EntityState::Deleted));
        assert!(matches!(memo_state(M), Some(EntityState::Memo(m)) if m.text == "remote memo"));
        assert!(matches!(memo_state(&memo_entity_id("n")), Some(EntityState::Memo(m)) if m.note == "nn"));
        let deleted: Vec<String> =
            op_sigs(&e.store).into_iter().filter(|s| s.2 == "delete").map(|s| s.1).collect();
        assert_eq!(deleted, vec![memo_entity_id("b")]);
        // The mirror shows exactly those memos (ids remapped) and keeps the clip.
        let mirrored = mirror(&live, &p, 0);
        let mut ids: Vec<&str> =
            mirrored.iter().filter(|e| e.kind == EntryKind::Memo).map(|e| e.id.as_str()).collect();
        ids.sort();
        let mut want = [memo_entity_id("a"), memo_entity_id("n"), M.to_owned()];
        want.sort();
        assert_eq!(ids, want.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(mirrored.iter().any(|e| e.kind == EntryKind::Clip && e.id == "k"));
        // A native memo keeps its own id on a later plan.
        let again = plan_import(&p, &WebStore::default(), &[memo(M, "remote memo", false, None)], &BTreeSet::new(), &name_key(1));
        assert!(again.ops.is_empty(), "{:?}", again.ops);
    }

    #[test]
    fn unknown_migration_state_is_an_error_never_a_reset() {
        for s in [
            MigrationState::None,
            MigrationState::BackedUp,
            MigrationState::Importing,
            MigrationState::Imported,
            MigrationState::Verified,
            MigrationState::Native,
        ] {
            assert_eq!(MigrationState::parse(s.as_str()).unwrap(), s);
        }
        assert!(matches!(MigrationState::parse("NONE"), Err(MigrationError::UnknownState(_))));
        let bytes = ws_bytes(&source());
        let mut e = env("mig-unknown", &clip_entries());
        e.store.meta_set(STATE_KEY, "migrated-v9").unwrap();
        assert!(matches!(MigrationState::load(&e.store), Err(MigrationError::UnknownState(s)) if s == "migrated-v9"));
        assert!(matches!(
            push_webstore_chunk(&mut e.store, &e.dir, VAULT, 0, 1, &bytes, SNAP),
            Err(MigrationError::UnknownState(_))
        ));
        assert!(matches!(import(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, 1), Err(MigrationError::UnknownState(_))));
        assert!(matches!(verify(&mut e.store, &e.dir, VAULT), Err(MigrationError::UnknownState(_))));
        assert!(matches!(
            apply_queued(&mut e.store, VAULT, &name_key(1), &mut e.clock, 1, &QueuedOp::TrDelete { tr: "t1".to_owned() }),
            Err(MigrationError::UnknownState(_))
        ));
        assert_eq!(e.store.meta_get(STATE_KEY).unwrap().as_deref(), Some("migrated-v9"));
        assert!(!e.dir.root.exists());
        assert_eq!(op_count(&e.store), 0);
    }

    #[test]
    fn webstore_mirror_ignores_unknown_fields_and_defaults_missing_ones() {
        let raw = br#"{"conversations":[{"id":"c","title":"t","createdAt":1,"updatedAt":2.5,
            "messages":[{"id":"m","role":"user","content":"x","ts":1700000000000.7,
            "images":[{"id":"i","mimeType":"image/png","data":"","width":10.0,"height":-3}]}]}],
            "history":[{"id":"h","ts":3,"source":"s","target":"t","from":"ko","to":"en","model":"m"}],
            "settings":{"ignored":true}}"#;
        let ws: WebStore = serde_json::from_slice(raw).unwrap();
        let m = &ws.conversations[0].messages[0];
        assert_eq!((m.ts, m.model.clone()), (1_700_000_000_000, None));
        assert_eq!((m.images[0].width, m.images[0].height), (10, 0));
        assert!(!ws.history[0].pinned);
    }

    fn memo_at(p: &Projection, id: &str) -> Option<EntityState> {
        p.entities.get(&(Kind::Memo, id.to_owned())).cloned()
    }

    fn delete_ops(store: &Store) -> Vec<(Kind, String)> {
        op_sigs(store).into_iter().filter(|s| s.2.contains("delete")).map(|s| (s.0, s.1)).collect()
    }

    #[test]
    fn finalize_with_shrunken_and_evicted_current_emits_no_delete_and_keeps_everything() {
        let png = b"\x89PNG evicted later".to_vec();
        let mut ws = source();
        ws.conversations[0].messages[0].images = vec![WsImage {
            id: "i1".to_owned(),
            mime_type: "image/png".to_owned(),
            data: BASE64.encode(&png),
            width: 4,
            height: 4,
            name: None,
        }];
        let mut e = env("mig-shrunk", &clip_entries());
        to_verified(&mut e, &ws_bytes(&ws));
        // Quota eviction in the webview: c2 and t1 dropped, the image blanked.
        let mut current = ws.clone();
        current.conversations.retain(|c| c.id != "c2");
        current.conversations[0].messages[0].images[0].data.clear();
        current.history.retain(|t| t.id != "t1");
        let cur = ws_bytes(&current);
        let live = clip_entries();
        let key = name_key(1);
        let out =
            finalize(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, "cur-hash", Some((&cur, "cur-hash")), Some(&live), 5)
                .unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 5 });
        assert!(delete_ops(&e.store).is_empty(), "{:?}", delete_ops(&e.store));
        let p = e.store.projection(VAULT).unwrap();
        assert_eq!(counts(&p), (2, 3, 2, 2));
        let atts = &conv(&p, "c1").messages[&msg_entity_id("c1", "m1")].attachments;
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["blob_id"], json!(hex(&hmac_sha256(&key, &png))));
        assert!(matches!(p.entities.get(&(Kind::Conv, conv_entity_id("c2"))), Some(EntityState::Conv(_))));
        assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t1"))), Some(EntityState::Tr(_))));
        assert_eq!(e.store.meta_get(FINAL_HASH_KEY).unwrap().as_deref(), Some("cur-hash"));
    }

    #[test]
    fn finalize_without_a_trusted_live_load_keeps_the_imported_memos() {
        // Corrupt or unreadable clipboard at this boot: the handle is empty,
        // so G7 passes `None` and no memo is edited or deleted.
        let mut e = env("mig-nolive", &clip_entries());
        to_verified(&mut e, &ws_bytes(&source()));
        let snap = serde_json::from_slice::<Manifest>(&fs::read(e.dir.manifest()).unwrap()).unwrap().snapshot_hash;
        let ops = op_count(&e.store);
        let out = finalize(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, &snap, None, None, 6).unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 6 });
        assert_eq!(op_count(&e.store), ops);
        assert!(delete_ops(&e.store).is_empty());
        let p = e.store.projection(VAULT).unwrap();
        assert!(matches!(memo_at(&p, &memo_entity_id("a")), Some(EntityState::Memo(m)) if m.text == "memo a"));
        assert!(
            matches!(memo_at(&p, &memo_entity_id("b")), Some(EntityState::Memo(m)) if m.text == "memo b" && m.pinned && m.note == "note b")
        );
        assert_eq!(e.store.meta_get(FINAL_HASH_KEY).unwrap(), Some(snap));
        // The same call with an empty trusted load would have deleted both.
        let p0 = e.store.projection(VAULT).unwrap();
        let own = read_memo_ids(&e.store).unwrap();
        assert_eq!(plan_import(&p0, &WebStore::default(), &[], &own, &name_key(1)).ops.len(), 2);
    }

    #[test]
    fn live_memo_edited_after_a_remote_delete_is_restored_under_a_new_id_once() {
        let mut e = env("mig-restore", &clip_entries());
        to_verified(&mut e, &ws_bytes(&source()));
        // Both own-import memos are deleted remotely before finalize; the
        // user edited "a" in that window and left "b" as imported.
        for (n, id) in [(901, "a"), (902, "b")] {
            let del = op(n, 2_000, Kind::Memo, &memo_entity_id(id), "delete", json!({}));
            e.store.insert_local_op(VAULT, del, UploadState::Never).unwrap();
        }
        let live = vec![memo("a", "memo a v2", true, Some("n2")), memo("b", "memo b", true, Some("note b"))];
        let snap = serde_json::from_slice::<Manifest>(&fs::read(e.dir.manifest()).unwrap()).unwrap().snapshot_hash;
        let out = finalize(&mut e.store, &e.dir, VAULT, &name_key(1), &mut e.clock, &snap, None, Some(&live), 1).unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 1 });
        let p = e.store.projection(VAULT).unwrap();
        let restored = memo_restore_id("a");
        assert_eq!(restored, v5("memo-restore:a"));
        assert!(matches!(
            memo_at(&p, &restored),
            Some(EntityState::Memo(m)) if m.text == "memo a v2" && m.pinned && m.note == "n2"
        ));
        assert_eq!(memo_at(&p, &memo_entity_id("a")), Some(EntityState::Deleted));
        assert_eq!(memo_at(&p, &memo_entity_id("b")), Some(EntityState::Deleted));
        assert_eq!(memo_at(&p, &memo_restore_id("b")), None, "an unchanged memo is never restored");
        let sigs = op_sigs(&e.store);
        let restore_ops: Vec<&str> = sigs.iter().filter(|s| s.1 == restored).map(|s| s.2.as_str()).collect();
        assert_eq!(restore_ops, vec!["create", "note"]);
        assert!(read_memo_ids(&e.store).unwrap().contains(&restored));
        // Idempotent: the restore entity is now diffed like any memo.
        let (_, _, backup) = read_backup(&e.dir).unwrap();
        let own = read_memo_ids(&e.store).unwrap();
        let again =
            plan_inner(&p, &WebStore::default(), Some(MemoSource { live: &live, backup: Some(&backup) }), &own, &name_key(1));
        assert!(again.ops.is_empty(), "{:?}", again.ops);
        let mut edited = live.clone();
        edited[0].text = "memo a v3".to_owned();
        let more =
            plan_inner(&p, &WebStore::default(), Some(MemoSource { live: &edited, backup: Some(&backup) }), &own, &name_key(1));
        let got: Vec<(&str, &str)> = more.ops.iter().map(|o| (o.entity_id.as_str(), o.op_type.as_str())).collect();
        assert_eq!(got, vec![(restored.as_str(), "edit")]);
        // The mirror shows the restored memo once.
        let mirrored = mirror(&live, &p, 0);
        let memos: Vec<&str> = mirrored.iter().filter(|m| m.kind == EntryKind::Memo).map(|m| m.id.as_str()).collect();
        assert_eq!(memos, vec![restored.as_str()]);
    }

    #[test]
    fn failed_verify_stays_imported_and_a_re_import_fills_the_gap() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-reimport", &clip_entries());
        assert_eq!(push_all(&mut e, &bytes, None).unwrap(), MigrationState::BackedUp);
        let key = name_key(1);
        let crashed = import_crashing(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, 1_000, Some(Crash::InImportTx));
        assert!(matches!(crashed, Err(MigrationError::Crashed(Crash::InImportTx))));
        // A state that claims `imported` with nothing stored.
        e.store.meta_set(STATE_KEY, MigrationState::Imported.as_str()).unwrap();
        match verify(&mut e.store, &e.dir, VAULT).unwrap() {
            Verify::Failed(bad) => {
                assert!(bad.iter().any(|b| b == "conversation c1 missing"), "{bad:?}");
                assert!(bad.iter().any(|b| b == "memo a missing"), "{bad:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(e.state(), MigrationState::Imported);
        assert_eq!(op_count(&e.store), 0);
        import(&mut e.store, &e.dir, VAULT, &key, &mut e.clock, 1_000).unwrap();
        assert_eq!(e.state(), MigrationState::Imported);
        assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
        assert_eq!(e.state(), MigrationState::Verified);
        assert_eq!(counts(&e.store.projection(VAULT).unwrap()), (2, 3, 2, 2));
        assert_no_duplicates(&e.store);
    }

    #[test]
    fn torn_partial_append_is_cut_back_to_the_recorded_length() {
        let bytes = ws_bytes(&source());
        let c: Vec<&[u8]> = bytes.chunks(bytes.len().div_ceil(3)).collect();
        let mut e = env("mig-torn", &clip_entries());
        let push = |e: &mut Env, i: u32, b: &[u8]| push_webstore_chunk(&mut e.store, &e.dir, VAULT, i, 3, b, SNAP);
        assert_eq!(push(&mut e, 0, c[0]).unwrap(), MigrationState::None);
        // A crash mid-append of chunk 1 left a torn tail behind the progress.
        let mut f = fs::OpenOptions::new().append(true).open(e.dir.partial()).unwrap();
        f.write_all(b"\"torn tail").unwrap();
        drop(f);
        e.reopen();
        assert_eq!(push(&mut e, 1, c[1]).unwrap(), MigrationState::None);
        assert_eq!(fs::read(e.dir.partial()).unwrap(), [c[0], c[1]].concat());
        assert_eq!(push(&mut e, 2, c[2]).unwrap(), MigrationState::BackedUp);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
    }

    #[test]
    fn unparseable_clipboard_at_finish_stays_none_and_chunk_zero_retries_the_finish() {
        let bytes = ws_bytes(&source());
        let mut e = env("mig-badclip", &clip_entries());
        fs::write(&e.dir.clipboard_src, b"{\"entries\": [oops").unwrap();
        let out = push_all(&mut e, &bytes, None);
        assert!(matches!(out, Err(MigrationError::Corrupt(_))), "{out:?}");
        assert_eq!(e.state(), MigrationState::None);
        assert_eq!(fs::read(&e.dir.clipboard_src).unwrap(), b"{\"entries\": [oops");
        assert!(!e.dir.clipboard_backup().exists() && !e.dir.webstore().exists() && !e.dir.manifest().exists());
        assert_eq!(op_count(&e.store), 0);
        // Repaired: the webview restarts its push at chunk 0, which re-runs
        // the finish because every chunk was already received (A10).
        fs::write(&e.dir.clipboard_src, &e.clipboard_original).unwrap();
        let c0 = &bytes[..bytes.len().div_ceil(3)];
        let state = push_webstore_chunk(&mut e.store, &e.dir, VAULT, 0, 3, c0, SNAP).unwrap();
        assert_eq!(state, MigrationState::BackedUp);
        assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), e.clipboard_original);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
    }

    #[test]
    fn chunk_zero_after_a_full_received_count_reruns_the_finish() {
        let bytes = ws_bytes(&source());
        let c: Vec<&[u8]> = bytes.chunks(bytes.len().div_ceil(3)).collect();
        let mut e = env("mig-a10", &clip_entries());
        let out = push_all(&mut e, &bytes, Some(Crash::AfterBackupFiles));
        assert!(matches!(out, Err(MigrationError::Crashed(Crash::AfterBackupFiles))), "{out:?}");
        e.reopen();
        // Other bytes at index 0 are a new snapshot: reset as before.
        let mut e2 = env("mig-a10-other", &clip_entries());
        assert!(matches!(push_all(&mut e2, &bytes, Some(Crash::AfterBackupFiles)), Err(MigrationError::Crashed(_))));
        let r = push_webstore_chunk(&mut e2.store, &e2.dir, VAULT, 0, 3, b"{\"other\"", SNAP);
        assert!(matches!(r, Err(MigrationError::Chunk(_))), "{r:?}");
        assert_eq!(e2.state(), MigrationState::None);
        // The same chunk 0 completes without a reset cycle.
        assert_eq!(push_webstore_chunk(&mut e.store, &e.dir, VAULT, 0, 3, c[0], SNAP).unwrap(), MigrationState::BackedUp);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
        // The rest of the restarted push is ignored once backed up.
        assert_eq!(push_webstore_chunk(&mut e.store, &e.dir, VAULT, 1, 3, c[1], SNAP).unwrap(), MigrationState::BackedUp);
    }

    #[test]
    fn lone_surrogate_escapes_are_replaced_only_when_they_are_real_escapes() {
        let cases: [(&str, &str); 9] = [
            // A lone high, a lone low, a high before a non-low.
            (r#""a\ud83d""#, r#""a\uFFFD""#),
            (r#""\uDC00x""#, r#""\uFFFDx""#),
            (r#""\ud83d\u0041""#, r#""\uFFFD\u0041""#),
            (r#""\ud83d\ud83d\ude00""#, r#""\uFFFD\ud83d\ude00""#),
            // A pair, a BMP escape, and invalid hex stay as they are.
            (r#""\ud83d\ude00""#, r#""\ud83d\ude00""#),
            (r#""\u00e9\n""#, r#""\u00e9\n""#),
            (r#""\uZZZZ""#, r#""\uZZZZ""#),
            // An escaped backslash: the following `u` is plain text.
            (r#""\\ud83d""#, r#""\\ud83d""#),
            (r#""\\\ud83d""#, r#""\\\uFFFD""#),
        ];
        for (input, want) in cases {
            assert_eq!(sanitize_json(input.as_bytes()), want, "{input}");
        }
        assert_eq!(sanitize_json(b"\"\xff\""), "\"\u{FFFD}\"");
        // Through the parser: the escaped-backslash title keeps its text, the
        // lone surrogate becomes U+FFFD, and both reach the same WebStore.
        let raw = br#"{"conversations":[{"id":"c","title":"a\\ud83d","messages":[]},
            {"id":"d","title":"b\ud83d","messages":[]}]}"#;
        let ws = parse_webstore(raw).unwrap();
        assert_eq!(ws.conversations[0].title, "a\\ud83d");
        assert_eq!(ws.conversations[1].title, "b\u{FFFD}");
        assert!(serde_json::from_slice::<WebStore>(raw).is_err());
    }

    // -- G8 native load ------------------------------------------------------

    fn load_with(store: &Store, mut blob: impl FnMut(&str) -> Option<Vec<u8>>) -> ChatStore {
        let (set, forks) = store.load_set(VAULT).unwrap();
        let p = crate::sync::merge::project(&set, &forks).unwrap();
        chat_store(&p, &SrcIndex::of(&set), |id| Ok(blob(id))).unwrap()
    }

    fn load(store: &Store) -> ChatStore {
        load_with(store, |id| store.blob_bytes(id).unwrap().map(|(_, bytes)| bytes))
    }

    /// `ChatConv` minus the times, ordered like the source.
    fn as_webstore(mut chat: ChatStore) -> WebStore {
        chat.conversations.sort_by(|a, b| a.id.cmp(&b.id));
        chat.history.sort_by(|a, b| a.id.cmp(&b.id));
        WebStore {
            conversations: chat
                .conversations
                .into_iter()
                .map(|c| WsConv { id: c.id, title: c.title, messages: c.messages })
                .collect(),
            history: chat.history,
        }
    }

    fn png_store() -> (WebStore, Vec<u8>) {
        let png = b"\x89PNG round trip".to_vec();
        let img = |id: &str, data: String| WsImage {
            id: id.to_owned(),
            mime_type: "image/png".to_owned(),
            data,
            width: 4,
            height: 3,
            name: Some(format!("{id}.png")),
        };
        let mut ws = source();
        ws.conversations[0].messages[0].images = vec![img("ok", BASE64.encode(&png)), img("ev", String::new())];
        (ws, png)
    }

    #[test]
    fn g8_import_then_load_round_trips_source_ids_and_fields() {
        let (ws, png) = png_store();
        let mut e = env("g8-roundtrip", &clip_entries());
        drive(&mut e, &ws_bytes(&ws), None).unwrap();

        let chat = load(&e.store);
        assert_eq!(chat.src_fallbacks, 0);
        // updatedAt = max message ts; newest first.
        let times: Vec<(&str, u64, u64)> =
            chat.conversations.iter().map(|c| (c.id.as_str(), c.created_at, c.updated_at)).collect();
        assert_eq!(times, vec![("c2", 3, 3), ("c1", 1, 2)]);
        let ids: Vec<&str> = chat.history.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2"], "same ts: by id");
        let c1_images = &chat.conversations[1].messages[0].images;
        assert_eq!(BASE64.decode(c1_images[0].data.as_bytes()).unwrap(), png);
        assert_eq!(c1_images[1].data, "", "evicted at import stays evicted");
        // Everything else equals the input (evicted images keep `data: ""`).
        assert_eq!(as_webstore(chat), ws);

        // The JSON is the webview shape.
        let json = serde_json::to_value(load(&e.store)).unwrap();
        let c1 = &json["conversations"][1];
        assert_eq!((c1["id"].as_str(), c1["createdAt"].as_u64(), c1["updatedAt"].as_u64()), (Some("c1"), Some(1), Some(2)));
        assert_eq!(c1["messages"][0]["images"][0]["mimeType"], "image/png");
        assert_eq!(c1["messages"][1]["model"], "gpt");
        assert!(c1["messages"][0].get("model").is_none());
        assert_eq!(json["history"][1]["pinned"], true);
        assert!(json.get("srcFallbacks").is_none() && json.get("src_fallbacks").is_none());
    }

    #[test]
    fn g8_a_blob_evicted_after_import_loads_as_empty_data() {
        let (ws, png) = png_store();
        let mut e = env("g8-evicted", &clip_entries());
        drive(&mut e, &ws_bytes(&ws), None).unwrap();
        let blob_id = hex(&hmac_sha256(&name_key(1), &png));
        let asked = std::cell::RefCell::new(Vec::new());
        let chat = load_with(&e.store, |id| {
            asked.borrow_mut().push(id.to_owned());
            None
        });
        assert_eq!(asked.into_inner(), vec![blob_id], "only the stored attachment is looked up");
        let c1 = chat.conversations.iter().find(|c| c.id == "c1").unwrap();
        let img = &c1.messages[0].images[0];
        assert_eq!(
            (img.id.as_str(), img.data.as_str(), img.mime_type.as_str(), img.width, img.height, img.name.as_deref()),
            ("ok", "", "image/png", 4, 3, Some("ok.png"))
        );
    }

    #[test]
    fn g8_load_follows_the_active_leaf_else_the_newest_leaf() {
        let mut e = env("g8-active", &clip_entries());
        drive(&mut e, &ws_bytes(&source()), None).unwrap();
        let key = name_key(1);
        // A regenerate: m3 is a sibling of m2 and the newest leaf, but the
        // imported `set_active` register still names m2.
        let regen = QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m3", "assistant", "again", 10), parent: Some("m1".to_owned()) };
        assert!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 10, &regen).unwrap().changed);
        let path = |e: &Env| -> Vec<String> {
            load(&e.store).conversations.into_iter().find(|c| c.id == "c1").unwrap().messages.into_iter().map(|m| m.id).collect()
        };
        assert_eq!(path(&e), vec!["m1", "m2"]);
        let pick = QueuedOp::SetActive { conv: "c1".to_owned(), leaf: "m3".to_owned() };
        assert!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 11, &pick).unwrap().changed);
        assert_eq!(path(&e), vec!["m1", "m3"], "siblings kept, the active one shown");
        let c1 = conv(&e.store.projection(VAULT).unwrap(), "c1").messages.len();
        assert_eq!(c1, 3);

        // No `set_active` at all: the newest leaf (merge's fallback).
        let mut s = mem_store();
        let cid = conv_entity_id("k");
        let add = |n: u64, id: &str, parent: Option<&str>, ts: u64| {
            let mut p = json!({"conv_id": cid, "msg_id": msg_entity_id("k", id), "role": "user", "content": id,
                               "attachments": [], "ts": ts, "src_id": id});
            if let Some(parent) = parent {
                p["parent_id"] = json!(msg_entity_id("k", parent));
            }
            op(n, 100 + n, Kind::Conv, &cid, "msg.add", p)
        };
        for body in [
            op(1, 100, Kind::Conv, &cid, "create", json!({"title": "k", "src_id": "k"})),
            add(2, "r", None, 1),
            add(3, "a", Some("r"), 5),
            add(4, "b", Some("r"), 7),
        ] {
            s.insert_local_op(VAULT, body, UploadState::Never).unwrap();
        }
        let chat = load(&s);
        let ids: Vec<&str> = chat.conversations[0].messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!((chat.conversations[0].id.as_str(), ids), ("k", vec!["r", "b"]));
    }

    #[test]
    fn g8_active_path_stops_at_a_missing_parent_and_a_cycle() {
        let m = |parent: Option<&str>, ts: u64| crate::sync::merge::Message {
            parent_id: parent.map(str::to_owned),
            role: "user".to_owned(),
            content: String::new(),
            model: None,
            attachments: Vec::new(),
            ts,
            op_id: String::new(),
        };
        let conv = |leaf: &str, msgs: Vec<(&str, crate::sync::merge::Message)>| ConvState {
            title: String::new(),
            active_leaf: Some(leaf.to_owned()),
            messages: msgs.into_iter().map(|(id, m)| (id.to_owned(), m)).collect(),
        };
        let ids = |c: &ConvState| active_path(c).into_iter().map(|(id, _)| id.to_owned()).collect::<Vec<_>>();
        assert_eq!(ids(&conv("b", vec![("a", m(Some("gone"), 1)), ("b", m(Some("a"), 2))])), vec!["a", "b"]);
        let cyclic = conv("c", vec![("a", m(Some("b"), 1)), ("b", m(Some("a"), 2)), ("c", m(Some("a"), 3))]);
        assert_eq!(ids(&cyclic), vec!["b", "a", "c"]);
        assert!(ids(&conv("x", vec![("a", m(None, 1))])).is_empty(), "an unknown leaf shows nothing");
    }

    #[test]
    fn g8_queued_creates_round_trip_through_load_under_their_source_ids() {
        let mut e = env("g8-queue-src", &clip_entries());
        drive(&mut e, &ws_bytes(&source()), None).unwrap();
        let key = name_key(1);
        let mut image = WsImage { id: "img-1".to_owned(), mime_type: "image/png".to_owned(), ..Default::default() };
        image.data = BASE64.encode(b"pixels");
        let mut first = msg("q1", "user", "one", 20);
        first.images = vec![image];
        let queue = [
            QueuedOp::ConvCreate { conv: "web-conv".to_owned(), title: "queued".to_owned() },
            QueuedOp::MsgAdd { conv: "web-conv".to_owned(), msg: first, parent: None },
            QueuedOp::MsgAdd { conv: "web-conv".to_owned(), msg: msg("q2", "assistant", "two", 21), parent: Some("q1".to_owned()) },
            // q2 is already the newest leaf: a no-op that still flushes.
            QueuedOp::SetActive { conv: "web-conv".to_owned(), leaf: "q2".to_owned() },
            QueuedOp::TrAdd { tr: tr("web-tr", "w", true) },
        ];
        let changed: Vec<bool> =
            queue.iter().zip(10..).map(|(q, qseq)| apply_queued(&mut e.store, VAULT, &key, &mut e.clock, qseq, q).unwrap().changed).collect();
        assert_eq!(changed, vec![true, true, true, false, true]);
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(14));
        let chat = load(&e.store);
        assert_eq!(chat.src_fallbacks, 0);
        let c = &chat.conversations[0];
        assert_eq!((c.id.as_str(), c.title.as_str(), c.created_at, c.updated_at), ("web-conv", "queued", 20, 21));
        let ids: Vec<&str> = c.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["q1", "q2"]);
        assert_eq!((c.messages[0].images[0].id.as_str(), c.messages[0].images[0].data.as_str()), ("img-1", "cGl4ZWxz"));
        let t = chat.history.iter().find(|t| t.id == "web-tr").unwrap();
        assert_eq!(*t, tr("web-tr", "w", true));
    }

    /// An entity whose ops carry no `src_id` (none exist before release; a
    /// guard all the same): load exposes its entity id and counts it, and a
    /// queue op naming that id addresses the same entity, never a new one.
    #[test]
    fn g8_an_entity_without_src_id_loads_under_its_entity_id_and_queue_ops_reuse_it() {
        let mut e = env("g8-fallback", &clip_entries());
        drive(&mut e, &ws_bytes(&WebStore::default()), None).unwrap();
        let key = name_key(1);
        let cid = "0b0e6c55-7b2c-4f0e-9a3d-3a1f6b6c0001";
        let mid = "0b0e6c55-7b2c-4f0e-9a3d-3a1f6b6c0002";
        let tid = "0b0e6c55-7b2c-4f0e-9a3d-3a1f6b6c0003";
        for body in [
            op(901, 100, Kind::Conv, cid, "create", json!({"title": "legacy"})),
            op(902, 101, Kind::Conv, cid, "msg.add",
               json!({"conv_id": cid, "msg_id": mid, "role": "user", "content": "old", "attachments": [], "ts": 4})),
            op(903, 102, Kind::Tr, tid, "tr.add", json!({"source": "s", "target": "t", "ts": 4, "pinned": false})),
        ] {
            e.store.insert_local_op(VAULT, body, UploadState::Never).unwrap();
        }
        let chat = load(&e.store);
        assert_eq!(chat.src_fallbacks, 3);
        assert_eq!((chat.conversations[0].id.as_str(), chat.conversations[0].messages[0].id.as_str()), (cid, mid));
        assert_eq!(chat.history[0].id, tid);

        let convs_before = counts(&e.store.projection(VAULT).unwrap()).0;
        let queue = [
            // Already there under that id: no second conversation.
            QueuedOp::ConvCreate { conv: cid.to_owned(), title: "dup".to_owned() },
            QueuedOp::Rename { conv: cid.to_owned(), title: "renamed".to_owned() },
            QueuedOp::MsgAdd { conv: cid.to_owned(), msg: msg("new", "assistant", "reply", 5), parent: Some(mid.to_owned()) },
            QueuedOp::MsgAdd { conv: cid.to_owned(), msg: msg(mid, "user", "again", 4), parent: None },
            QueuedOp::TrPin { tr: tid.to_owned(), pinned: true },
        ];
        let changed: Vec<bool> =
            queue.iter().zip(10..).map(|(q, qseq)| apply_queued(&mut e.store, VAULT, &key, &mut e.clock, qseq, q).unwrap().changed).collect();
        assert_eq!(changed, vec![false, true, true, false, true]);
        let p = e.store.projection(VAULT).unwrap();
        assert_eq!(counts(&p).0, convs_before, "no conversation entity was created");
        assert!(!p.entities.contains_key(&(Kind::Conv, conv_entity_id(cid))));
        assert!(!p.entities.contains_key(&(Kind::Tr, tr_entity_id(tid))));
        let Some(EntityState::Conv(c)) = p.entities.get(&(Kind::Conv, cid.to_owned())) else { panic!("conversation") };
        assert_eq!(c.title, "renamed");
        let reply = msg_entity_id(cid, "new");
        assert_eq!(c.messages[&reply].parent_id.as_deref(), Some(mid));

        let chat = load(&e.store);
        let ids: Vec<&str> = chat.conversations[0].messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec![mid, "new"], "the new reply is addressed by its own source id");
        assert!(chat.history[0].pinned);
        assert_eq!(chat.src_fallbacks, 3);
    }

    #[test]
    fn queue_items_are_idempotent_by_qseq_and_register_ops_under_the_ack_are_skipped() {
        let mut e = env("mig-qseq", &clip_entries());
        drive(&mut e, &ws_bytes(&source()), None).unwrap();
        let key = name_key(1);
        assert_eq!(flushed_qseq(&e.store).unwrap(), None);
        let rename = QueuedOp::Rename { conv: "c1".to_owned(), title: "queued".to_owned() };
        // qseq 5 <= ack 9: the snapshot already holds it; only counted.
        let ops = op_count(&e.store);
        assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 5, &rename).unwrap(), Applied { changed: false });
        assert_eq!(op_count(&e.store), ops);
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(5));
        // A delete under the ack still applies by the id rule.
        let del = QueuedOp::ConvDelete { conv: "c2".to_owned() };
        assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 6, &del).unwrap(), Applied { changed: true });
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(6));
        // At or below the watermark: skipped whatever the op.
        let tr_del = QueuedOp::TrDelete { tr: "t1".to_owned() };
        assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 6, &tr_del).unwrap(), Applied { changed: false });
        assert!(matches!(e.store.projection(VAULT).unwrap().entities.get(&(Kind::Tr, tr_entity_id("t1"))), Some(EntityState::Tr(_))));
        // Above the ack: a register op applies and moves the watermark.
        assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 10, &rename).unwrap(), Applied { changed: true });
        assert_eq!(conv(&e.store.projection(VAULT).unwrap(), "c1").title, "queued");
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(10));
    }

    #[test]
    fn e3_a_replayed_qseq_is_duplicate_and_a_reused_one_is_stale() {
        let mut e = env("mig-e3-dup", &clip_entries());
        drive(&mut e, &ws_bytes(&source()), None).unwrap();
        let key = name_key(1);
        let rename = QueuedOp::Rename { conv: "c1".to_owned(), title: "queued".to_owned() };
        let del = QueuedOp::ConvDelete { conv: "c2".to_owned() };
        // The register skip under the ack records the hash too.
        assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 5, &rename).unwrap(), QueueApply::Applied { changed: false });
        assert_eq!(e.store.queue_op_hash(5).unwrap(), Some(queue_op_hash(&rename).unwrap()));
        assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 5, &rename).unwrap(), QueueApply::Duplicate);
        let ops = op_count(&e.store);
        assert_eq!(
            apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 5, &del).unwrap(),
            QueueApply::StaleQseq { watermark: 5 }
        );
        assert_eq!(op_count(&e.store), ops, "a stale item writes nothing");
        assert!(matches!(e.store.projection(VAULT).unwrap().entities.get(&(Kind::Conv, conv_entity_id("c2"))), Some(EntityState::Conv(_))));
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(5));
        // Unknown below the watermark (never applied here): stale.
        assert_eq!(
            apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 4, &rename).unwrap(),
            QueueApply::StaleQseq { watermark: 5 }
        );
        assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 6, &del).unwrap(), QueueApply::Applied { changed: true });
        assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 6, &del).unwrap(), QueueApply::Duplicate);
        // The compatibility wrapper reports both as unchanged.
        assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 6, &del).unwrap(), Applied { changed: false });
        assert_eq!(apply_queued(&mut e.store, VAULT, &key, &mut e.clock, 6, &rename).unwrap(), Applied { changed: false });
    }

    #[test]
    fn e3_the_op_hash_window_keeps_the_newest_1024_qseqs() {
        use crate::sync::store::QUEUE_HASH_WINDOW;
        let mut e = env("mig-e3-window", &clip_entries());
        drive(&mut e, &ws_bytes(&source()), None).unwrap();
        let key = name_key(1);
        // A pin of an unknown translation: a no-op that still flushes.
        let noop = QueuedOp::TrPin { tr: "absent".to_owned(), pinned: true };
        let first = 100u64;
        let last = first + QUEUE_HASH_WINDOW as u64 + 4;
        for qseq in first..=last {
            assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, qseq, &noop).unwrap(), QueueApply::Applied { changed: false });
        }
        let kept = e.store.queue_hash_qseqs().unwrap();
        assert_eq!(kept.len(), QUEUE_HASH_WINDOW);
        assert_eq!((kept[0], *kept.last().unwrap()), (last + 1 - QUEUE_HASH_WINDOW as u64, last));
        // Inside the window: a replay. Evicted: unknown, so stale.
        let oldest_kept = kept[0];
        assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, oldest_kept, &noop).unwrap(), QueueApply::Duplicate);
        assert_eq!(
            apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, oldest_kept - 1, &noop).unwrap(),
            QueueApply::StaleQseq { watermark: last }
        );
        assert_eq!(
            apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, first, &noop).unwrap(),
            QueueApply::StaleQseq { watermark: last }
        );
        // Gaps count entries, not qseq distance.
        let far = last + 10_000;
        apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, far, &noop).unwrap();
        let kept = e.store.queue_hash_qseqs().unwrap();
        assert_eq!((kept.len(), kept[0], *kept.last().unwrap()), (QUEUE_HASH_WINDOW, oldest_kept + 1, far));
    }

    #[test]
    fn e3_the_hash_and_the_watermark_commit_with_the_apply_or_not_at_all() {
        let mut e = env("mig-e3-atomic", &clip_entries());
        drive(&mut e, &ws_bytes(&source()), None).unwrap();
        let key = name_key(1);
        let del = QueuedOp::ConvDelete { conv: "c2".to_owned() };
        let rename = QueuedOp::Rename { conv: "c1".to_owned(), title: "late".to_owned() };
        // Fail the hash write of qseqs 5 and 11: everything else rolls back.
        e.store
            .conn()
            .execute_batch(
                "CREATE TRIGGER fail_hash BEFORE INSERT ON queue_op_hashes WHEN NEW.qseq IN (5, 11)
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let ops = op_count(&e.store);
        let before = e.store.projection(VAULT).unwrap().hash;
        assert!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 11, &del).is_err());
        assert_eq!(op_count(&e.store), ops, "no op without its hash");
        assert_eq!(e.store.projection(VAULT).unwrap().hash, before);
        assert_eq!(flushed_qseq(&e.store).unwrap(), None, "no watermark without its hash");
        assert_eq!(e.store.queue_op_hash(11).unwrap(), None);
        // The register skip under the ack (qseq 5 <= ack 9) is atomic too.
        assert!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 5, &rename).is_err());
        assert_eq!(flushed_qseq(&e.store).unwrap(), None);
        e.store.conn().execute_batch("DROP TRIGGER fail_hash;").unwrap();
        // The retry applies once, with its watermark and hash together.
        assert_eq!(apply_queue_item(&mut e.store, VAULT, &key, &mut e.clock, 11, &del).unwrap(), QueueApply::Applied { changed: true });
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(11));
        assert_eq!(e.store.queue_op_hash(11).unwrap(), Some(queue_op_hash(&del).unwrap()));
        assert!(op_count(&e.store) > ops);
    }

    // -- property: subset-then-full converges with one full import ---------

    /// The projection with op ids (fresh per run) removed.
    fn normalized(p: &Projection) -> Vec<u8> {
        fn strip(v: &mut Value) {
            match v {
                Value::Object(map) => {
                    map.remove("op_id");
                    map.remove("head_op_id");
                    map.values_mut().for_each(strip);
                }
                Value::Array(items) => items.iter_mut().for_each(strip),
                _ => {}
            }
        }
        let mut all = Map::new();
        for ((kind, id), state) in &p.entities {
            let mut v = serde_json::to_value(state).unwrap();
            strip(&mut v);
            all.insert(format!("{}/{id}", kind.as_str()), v);
        }
        Sha256::digest(serde_json::to_vec(&Value::Object(all)).unwrap()).to_vec()
    }

    type ConvSpec = (usize, usize, bool);
    type TrSpec = (bool, bool);
    type MemoSpec = (bool, bool, bool);

    /// (full, subset): the subset holds an earlier state of the same data:
    /// message prefixes, fewer translations and memos, older titles.
    fn stores(convs: &[ConvSpec], trs: &[TrSpec], memos: &[MemoSpec]) -> ((WebStore, Vec<ClipEntry>), (WebStore, Vec<ClipEntry>)) {
        let (mut full, mut sub) = (WebStore::default(), WebStore::default());
        for (i, &(n, cut, keep)) in convs.iter().enumerate() {
            let messages: Vec<WsMsg> =
                (0..n).map(|j| msg(&format!("m{j}"), if j % 2 == 0 { "user" } else { "" }, &format!("c{i}m{j}"), j as u64)).collect();
            let c = WsConv { id: format!("c{i}"), title: format!("title {i}"), messages };
            if keep {
                let mut early = c.clone();
                early.messages.truncate(cut % (n + 1));
                if cut % 2 == 1 {
                    early.title = format!("old {i}");
                }
                sub.conversations.push(early);
            }
            full.conversations.push(c);
        }
        for (i, &(pinned, keep)) in trs.iter().enumerate() {
            let t = tr(&format!("t{i}"), &format!("s{i}"), pinned);
            if keep {
                sub.history.push(t.clone());
            }
            full.history.push(t);
        }
        let (mut full_m, mut sub_m) = (Vec::new(), Vec::new());
        for (i, &(pinned, note, keep)) in memos.iter().enumerate() {
            let e = memo(&format!("memo{i}"), &format!("text {i}"), pinned, note.then_some("n"));
            if keep {
                sub_m.push(e.clone());
            }
            full_m.push(e);
        }
        ((full, full_m), (sub, sub_m))
    }

    fn mem_store() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        store.init_identity(&identity("a")).unwrap();
        store.set_active_vault(VAULT).unwrap();
        store
    }

    fn apply_plan(store: &mut Store, plan: Plan) {
        store.insert_local_batch(VAULT, to_bodies(plan.ops, &mut HlcClock::default(), 1_000), plan.blobs, Vec::new()).unwrap();
    }

    #[test]
    fn prop_subset_then_full_import_converges_and_is_additive() {
        let mut config = Config::with_cases(64);
        config.rng_seed = RngSeed::Fixed(0x6D16_4A7E);
        config.failure_persistence = None;
        let mut runner = TestRunner::new(config);
        let strategy = (
            proptest::collection::vec((0usize..4, 0usize..5, any::<bool>()), 0..4),
            proptest::collection::vec((any::<bool>(), any::<bool>()), 0..4),
            proptest::collection::vec((any::<bool>(), any::<bool>(), any::<bool>()), 0..4),
        );
        let key = name_key(1);
        runner
            .run(&strategy, |(convs, trs, memos)| {
                let ((full, full_m), (sub, sub_m)) = stores(&convs, &trs, &memos);

                let mut one = mem_store();
                let p0 = one.projection(VAULT).unwrap();
                let plan = plan_import(&p0, &full, &full_m, &BTreeSet::new(), &key);
                let full_ids = plan.memo_ids.clone();
                prop_assert!(plan.ops.iter().all(|o| !o.op_type.contains("delete")));
                apply_plan(&mut one, plan);

                let mut two = mem_store();
                let p0 = two.projection(VAULT).unwrap();
                let first = plan_import(&p0, &sub, &sub_m, &BTreeSet::new(), &key);
                prop_assert!(first.ops.iter().all(|o| !o.op_type.contains("delete")));
                let own = first.memo_ids.clone();
                apply_plan(&mut two, first);
                let p1 = two.projection(VAULT).unwrap();
                let second = plan_import(&p1, &full, &full_m, &own, &key);
                prop_assert!(second.ops.iter().all(|o| !o.op_type.contains("delete")));
                apply_plan(&mut two, second);

                let (a, b) = (one.projection(VAULT).unwrap(), two.projection(VAULT).unwrap());
                prop_assert_eq!(normalized(&a), normalized(&b));
                prop_assert_eq!(counts(&a), counts(&b));
                // Idempotent: the full set again plans nothing.
                for p in [&a, &b] {
                    let again = plan_import(p, &full, &full_m, &full_ids, &key);
                    prop_assert!(again.ops.is_empty(), "{:?}", again.ops);
                    prop_assert!(again.blobs.is_empty());
                }
                Ok(())
            })
            .unwrap();
    }
}
