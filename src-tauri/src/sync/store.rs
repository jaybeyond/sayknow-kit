//! Local sync store: SQLite in WAL mode at `<app_data_dir>/sync/sync.db`
//! (§4.1, §4.5, §4.2.1 R5–R7).
//!
//! Every vault-scoped row carries `vault_id` and every query filters by it.
//! Writes that must agree with each other share one transaction: a local op,
//! its single dot (I1), and the projection; a remote segment from cross-check
//! to cursor; a rotation from the first dot delete to `vault_identity`.
//!
//! Contract deviations (G3 design, store.rs):
//! - §4.1 extension `pending_segments(vault_id, device_id, first_seq,
//!   last_seq, bytes, created_at)`: `device_id` is in the key because seqs
//!   restart at 1 after a rotation; `created_at` drives the 30-day compaction
//!   trigger. Rows stay after publish as the ledger of own sealed segments
//!   until compaction confirms their remote delete (`forget_segments`).
//! - §4.1 extension `chain_nodes(vault_id, device_id, head, last_seq)`: every
//!   known chain head of a forked stream, so writes made on either variant
//!   chain before the fork was noticed still apply on every reader.
//! - Compaction trim (architect B2 / red-team p2_p2): `offer_deletable` is the
//!   one deletable predicate (own current id, not forked or retired, every op
//!   `published`, `last <= limit`, the limit being the verified snapshot
//!   frontier and every active cursor). The last offered limit per (vault,
//!   own device) is kept in memory, not in a table, so a verify stays
//!   read-only and a restart only forgets offers (never widens them).
//!   `forget_segments` re-evaluates that predicate on the current state under
//!   that limit and refuses anything it would not offer, changing nothing.
//! - `entities` / `msg_tree` are global in §4.1 (not vault-scoped), so they
//!   mirror only `meta.active_vault_id`. `reproject` of another vault returns
//!   its hash and rewrites its `memo_lineage` but leaves those tables alone.
//! - The projection is recomputed in full inside each write transaction
//!   (§4.4 "기본은 전체"); entity-scoped re-projection is a later optimisation.
//! - `rotation_input(vault, remote)` takes the verified remote body hashes per
//!   seq from the caller (R2 decrypts `log/D/`); `remote_index` holds object
//!   digests, not per-op body hashes. `local_compacted` is always empty: local
//!   compaction never deletes op rows, only remote segments.
//! - `apply_rotation` also takes that remote map, to record forks (amendment
//!   A5) inside the R5–R7 transaction: for each seq > S where we kept a
//!   published op and the remote set is not exactly ours, a fork at that seq
//!   is recorded when the remote holds other content, and `applied_seq` is
//!   clamped below the first such seq (list lag included) so the retired
//!   stream reread covers it.
//! - `OwnOp.payload` is the canonical payload *with* its op references
//!   (amendment: refs live inside the payload); `OwnOp.refs` is
//!   `ops::refs(payload)`. A remapped ref is written back into the payload
//!   with `ops::remap_ref`, which recomputes `content_hash`.
//! - Reissued X ops keep their old `ops` row (state `rejected`, no dot) next
//!   to a `quarantine` row `op/<old_op_id>`: kept locally, never loaded into
//!   the op set, never uploaded.
//! - Failure taxonomy (§4.5 / §4.6.1 / AC 7): decrypt, AAD, cross-check, and
//!   non-canonical plaintext → quarantine row only, cursor bytes unchanged.
//!   Chain break with a wrong `prev_seg_hash`, schema violation (including
//!   integers SQLite cannot store, ctx values included), foreign
//!   `origin_device`, or a segment that does not encode → quarantine row +
//!   cursor `held`. Classification is by cause: only decoding or validating
//!   the incoming segment blames it. A failure after that (loading or
//!   re-projecting local rows, a damaged stored row → `Corrupt`, I/O) is a
//!   local fault: `Err`, rolled back, no quarantine, stream untouched. A gap
//!   (missing segment) → cursor `held` without a
//!   quarantine row (the next object is not at fault). A future version
//!   (envelope version byte, segment or op `v`) → cursor `held`, no
//!   quarantine row; `ApplyOutcome::FutureVersion` is the update notice.
//! - A same-op-id/different-content op inside an otherwise valid segment is
//!   quarantined alone (`op/<op_id>`) and its dot skipped; the rest applies.
//! - A `held` cursor refuses further segments (`Skipped`), except a fork
//!   whose variants attach to the known chain. A `forked` cursor applies any
//!   segment continuing one of its known heads; one that continues a known
//!   head at the wrong seq is held + quarantined; one that continues no known
//!   head yet is list lag (architect B1) → `Deferred`: nothing written, no
//!   quarantine, cursor unchanged, retried when the engine relists the
//!   forked stream in full next cycle. Only a non-forked stream uses the
//!   simple-break `held` rule.
//!   `retired` streams have no known chain head after rotation, so their
//!   first segment is accepted on seq continuity alone and overlaps resolve
//!   per dot.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::causal::{Forks, OpSet};
use super::crypto::{hex, CryptoError, SecretKey};
use super::envelope::{
    check_segment, open, segment_ref, ObjectAddress, ObjectKind, SegmentHeader, ENVELOPE_VERSION,
};
use super::identity::{genesis_chain_head, DeviceIdentity, LocalIdentity};
use super::merge::{project, EntityState, Projection, PROJECTION_VERSION};
use super::ops::{
    body_hash, canonical_json, check_plaintext, content_hash, refs, remap_ref, segment_hash, Dot, Hlc, Kind, OpBody,
    OpError, SegOp, Segment, FORMAT_V, MAX_STORED,
};
use super::rotation::{self, OpHashes, OwnOp, RotationInput, RotationPlan, UploadState};

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE entities(kind TEXT NOT NULL, id TEXT NOT NULL, state_json TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0, evicted_local INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(kind, id));
CREATE TABLE msg_tree(conv_id TEXT NOT NULL, msg_id TEXT PRIMARY KEY, parent_id TEXT, created_hlc TEXT);
CREATE TABLE blobs(blob_id TEXT PRIMARY KEY, mime TEXT, bytes BLOB, sha256 BLOB, local_path TEXT);
CREATE TABLE migration_queue(qseq INTEGER PRIMARY KEY, op_json TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE checkpoints(id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL, path TEXT NOT NULL, frontier_json TEXT NOT NULL);
CREATE TABLE sync_stats(name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE vault_identity(vault_id TEXT PRIMARY KEY, device_id TEXT NOT NULL, generation INTEGER NOT NULL,
    sealed_seq INTEGER NOT NULL, chain_head BLOB NOT NULL, binding_fp TEXT);
CREATE TABLE ops(vault_id TEXT NOT NULL, op_id TEXT NOT NULL, origin_device TEXT NOT NULL,
    hlc_wall INTEGER NOT NULL, hlc_ctr INTEGER NOT NULL, ctx_frontier TEXT NOT NULL, obs TEXT,
    entity_kind TEXT NOT NULL, entity_id TEXT NOT NULL, op_type TEXT NOT NULL, payload BLOB NOT NULL,
    content_hash BLOB NOT NULL, body_hash BLOB NOT NULL, upload_state TEXT NOT NULL,
    PRIMARY KEY(vault_id, op_id));
CREATE TABLE op_dots(vault_id TEXT NOT NULL, device_id TEXT NOT NULL, seq INTEGER NOT NULL, op_id TEXT NOT NULL,
    PRIMARY KEY(vault_id, device_id, seq, op_id),
    FOREIGN KEY(vault_id, op_id) REFERENCES ops(vault_id, op_id));
CREATE INDEX op_dots_by_op ON op_dots(vault_id, op_id);
CREATE TABLE forks(vault_id TEXT NOT NULL, device_id TEXT NOT NULL, fork_start INTEGER NOT NULL,
    detected_at INTEGER NOT NULL, PRIMARY KEY(vault_id, device_id));
CREATE TABLE memo_lineage(vault_id TEXT NOT NULL, memo_id TEXT NOT NULL, op_id TEXT NOT NULL,
    base_op_id TEXT, routed_to TEXT NOT NULL, PRIMARY KEY(vault_id, op_id));
CREATE TABLE vault_blob_map(vault_id TEXT NOT NULL, blob_id TEXT NOT NULL, local_blob TEXT, remote_state TEXT,
    PRIMARY KEY(vault_id, blob_id));
CREATE TABLE cursors(vault_id TEXT NOT NULL, remote_device_id TEXT NOT NULL, applied_seq INTEGER NOT NULL,
    chain_head BLOB, snapshot_id TEXT, state TEXT NOT NULL, PRIMARY KEY(vault_id, remote_device_id));
CREATE TABLE remote_index(vault_id TEXT NOT NULL, key TEXT NOT NULL, provider_ref TEXT, size INTEGER, etag TEXT,
    content_hash BLOB, seen_at INTEGER, PRIMARY KEY(vault_id, key));
CREATE TABLE retired_devices(vault_id TEXT NOT NULL, device_id TEXT NOT NULL, retired_at INTEGER NOT NULL,
    reason TEXT NOT NULL, verified_prefix_S INTEGER NOT NULL, PRIMARY KEY(vault_id, device_id));
CREATE TABLE quarantine(vault_id TEXT NOT NULL, key TEXT NOT NULL, reason TEXT NOT NULL, seen_at INTEGER NOT NULL,
    PRIMARY KEY(vault_id, key));
-- §4.1 extension: sealed own segments, persisted before any put (never resealed).
CREATE TABLE pending_segments(vault_id TEXT NOT NULL, device_id TEXT NOT NULL, first_seq INTEGER NOT NULL,
    last_seq INTEGER NOT NULL, bytes BLOB NOT NULL, created_at INTEGER NOT NULL,
    PRIMARY KEY(vault_id, device_id, first_seq));
"#, r#"
-- §4.1 extension: every known chain head of a forked stream (§4.2.3 (a)).
CREATE TABLE chain_nodes(vault_id TEXT NOT NULL, device_id TEXT NOT NULL, head BLOB NOT NULL,
    last_seq INTEGER NOT NULL, PRIMARY KEY(vault_id, device_id, head));
"#, r#"
-- G8 E3: sha256 of each applied queue op, the newest QUEUE_HASH_WINDOW qseqs.
CREATE TABLE queue_op_hashes(qseq INTEGER PRIMARY KEY, op_hash BLOB NOT NULL);
"#, r#"
-- G10 A15: `fork_range_obs` looks ops up by entity; lineage routing looks
-- memo_lineage up by `routed_to`.
CREATE INDEX ops_by_entity ON ops(vault_id, entity_kind, entity_id);
CREATE INDEX memo_lineage_by_route ON memo_lineage(vault_id, routed_to);
"#];

/// How many applied queue qseqs keep their op hash (E3): a replay inside
/// the window is told apart from a reused qseq.
pub(crate) const QUEUE_HASH_WINDOW: usize = 1024;

#[derive(Debug)]
pub(crate) enum StoreError {
    Sql(rusqlite::Error),
    Op(OpError),
    Rotation(rotation::RotationError),
    /// A stored row does not decode; the database was edited or damaged.
    Corrupt(String),
    /// No `vault_identity` row for the vault.
    NoIdentity,
    /// The call contradicts stored state (wrong device, reseal, ...).
    Invalid(String),
    Io(std::io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "sqlite: {e}"),
            StoreError::Op(e) => write!(f, "op: {e}"),
            StoreError::Rotation(e) => write!(f, "rotation: {e:?}"),
            StoreError::Corrupt(why) => write!(f, "corrupt store row: {why}"),
            StoreError::NoIdentity => f.write_str("no device identity for this vault"),
            StoreError::Invalid(why) => write!(f, "invalid store call: {why}"),
            StoreError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<OpError> for StoreError {
    fn from(e: OpError) -> Self {
        StoreError::Op(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

fn corrupt(why: impl fmt::Display) -> StoreError {
    StoreError::Corrupt(why.to_string())
}

pub(crate) fn upload_state_text(state: UploadState) -> &'static str {
    match state {
        UploadState::Never => "never",
        UploadState::Attempted => "attempted",
        UploadState::Published => "published",
        UploadState::Rejected => "rejected",
    }
}

fn parse_upload_state(text: &str) -> Result<UploadState, StoreError> {
    match text {
        "never" => Ok(UploadState::Never),
        "attempted" => Ok(UploadState::Attempted),
        "published" => Ok(UploadState::Published),
        "rejected" => Ok(UploadState::Rejected),
        other => Err(corrupt(format!("upload_state {other}"))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorState {
    Ok,
    Held,
    Forked,
    Retired,
}

impl CursorState {
    fn text(self) -> &'static str {
        match self {
            CursorState::Ok => "ok",
            CursorState::Held => "held",
            CursorState::Forked => "forked",
            CursorState::Retired => "retired",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "ok" => Ok(CursorState::Ok),
            "held" => Ok(CursorState::Held),
            "forked" => Ok(CursorState::Forked),
            "retired" => Ok(CursorState::Retired),
            other => Err(corrupt(format!("cursor state {other}"))),
        }
    }
}

/// How far another device's stream (or our retired id) has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub(crate) applied_seq: u64,
    /// `None` once a rotation or fork leaves no single chain head.
    pub(crate) chain_head: Option<[u8; 32]>,
    pub(crate) state: CursorState,
}

impl Cursor {
    fn genesis(vault: &str, device: &str) -> Self {
        Cursor { applied_seq: 0, chain_head: Some(genesis_chain_head(vault, device)), state: CursorState::Ok }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    Applied { hash: [u8; 32], fork_started: bool, conflicts: usize },
    /// Already applied; nothing written.
    Duplicate,
    /// The stream is held or forked; nothing written.
    Skipped(CursorState),
    /// A forked stream's segment that attaches to no known head yet (list
    /// lag): nothing written, no quarantine, cursor untouched; the engine
    /// relists the forked stream in full and retries it next cycle.
    Deferred { reason: String },
    /// Rolled back; the stream is now `held`.
    Held { reason: String },
    /// Written by a newer build; the stream is `held` until the app updates.
    FutureVersion(u64),
    /// Rolled back; a quarantine row was written, the cursor is untouched.
    Quarantined { reason: String },
}

enum Step {
    Done(ApplyOutcome),
    Hold { reason: String, quarantine: bool },
    Future(u64),
}

/// (content_hash, body_hash, upload_state) of a stored op.
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) type OpState = ([u8; 32], [u8; 32], UploadState);

/// An own op as stored by [`Store::insert_local_op`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalOp {
    pub(crate) body: OpBody,
    pub(crate) dot: Dot,
    pub(crate) projection_hash: [u8; 32],
}

/// A local blob written by [`Store::insert_local_batch`]; `blob_id` is the
/// content-addressed name (HMAC hex) and the map row starts `pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobRow {
    pub(crate) blob_id: String,
    pub(crate) mime: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) sha256: [u8; 32],
}

/// A sealed own segment, persisted before any put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSegment {
    pub(crate) device_id: String,
    pub(crate) first_seq: u64,
    pub(crate) last_seq: u64,
    pub(crate) bytes: Vec<u8>,
}

/// Owned inputs of [`rotation::plan_rotation`], read from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RotationSource {
    pub(crate) old_device: String,
    pub(crate) ops: Vec<OwnOp>,
    pub(crate) remote: BTreeMap<u64, Vec<[u8; 32]>>,
    pub(crate) local_compacted: BTreeMap<u64, [u8; 32]>,
}

impl RotationSource {
    pub(crate) fn input<'a>(&'a self, new_device: &'a str) -> RotationInput<'a> {
        RotationInput {
            old_device: &self.old_device,
            new_device,
            ops: &self.ops,
            remote: &self.remote,
            local_compacted: &self.local_compacted,
        }
    }
}

/// Why a sealed object could not be turned into a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SealedError {
    /// Quarantine: decrypt/AAD failure, bad plaintext, or cross-check mismatch.
    Corrupt(String),
    FutureVersion(u64),
}

/// Opens, decodes, and cross-checks one sealed segment (§4.6.1 N-7).
pub(crate) fn open_segment(key: &SecretKey, address: &ObjectAddress, envelope: &[u8]) -> Result<Segment, SealedError> {
    if address.kind != ObjectKind::Segment {
        return Err(SealedError::Corrupt("not a segment address".to_owned()));
    }
    // A newer envelope version is a hold (§4.5), not a corrupt object.
    let plain = open(key, address, envelope).map_err(|e| match e {
        CryptoError::UnsupportedVersion(v) if v > ENVELOPE_VERSION => SealedError::FutureVersion(u64::from(v)),
        e => SealedError::Corrupt(format!("{e:?}")),
    })?;
    let segment = match Segment::decode(&plain) {
        Ok(segment) => segment,
        Err(OpError::FutureVersion(v)) => return Err(SealedError::FutureVersion(v)),
        Err(e) => return Err(SealedError::Corrupt(e.to_string())),
    };
    let seqs: Vec<u64> = segment.ops.iter().map(|op| op.seq).collect();
    let header = SegmentHeader {
        device_id: &segment.device_id,
        first_seq: segment.first_seq,
        last_seq: segment.last_seq,
        op_seqs: &seqs,
    };
    check_segment(&header, address).map_err(|e| SealedError::Corrupt(format!("{e:?}")))?;
    Ok(segment)
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn h32(bytes: Vec<u8>) -> Result<[u8; 32], StoreError> {
    bytes.try_into().map_err(|_| corrupt("hash is not 32 bytes"))
}

fn json_text(value: &Value) -> Result<String, StoreError> {
    String::from_utf8(canonical_json(value)?).map_err(corrupt)
}

fn ctx_text(ctx: &BTreeMap<String, u64>) -> Result<String, StoreError> {
    json_text(&serde_json::to_value(ctx).map_err(corrupt)?)
}

fn obs_text(obs: &[String]) -> Result<Option<String>, StoreError> {
    if obs.is_empty() {
        return Ok(None);
    }
    Ok(Some(json_text(&serde_json::to_value(obs).map_err(corrupt)?)?))
}

fn seg_key(device: &str, first: u64, last: u64) -> String {
    format!("seg/{device}/{}", segment_ref(first, last))
}

/// Raw `ops` columns, decoded by [`RawOp::body`] outside the row closure.
struct RawOp {
    op_id: String,
    origin: String,
    wall: u64,
    ctr: u32,
    ctx: String,
    obs: Option<String>,
    kind: String,
    entity_id: String,
    op_type: String,
    payload: Vec<u8>,
}

const OP_COLS: &str =
    "o.op_id, o.origin_device, o.hlc_wall, o.hlc_ctr, o.ctx_frontier, o.obs, o.entity_kind, o.entity_id, o.op_type, o.payload";

impl RawOp {
    fn read(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Self> {
        Ok(RawOp {
            op_id: row.get(at)?,
            origin: row.get(at + 1)?,
            wall: row.get(at + 2)?,
            ctr: row.get(at + 3)?,
            ctx: row.get(at + 4)?,
            obs: row.get(at + 5)?,
            kind: row.get(at + 6)?,
            entity_id: row.get(at + 7)?,
            op_type: row.get(at + 8)?,
            payload: row.get(at + 9)?,
        })
    }

    fn body(self) -> Result<OpBody, StoreError> {
        Ok(OpBody {
            v: FORMAT_V,
            op_id: self.op_id,
            origin_device: self.origin,
            hlc: Hlc { wall_ms: self.wall, ctr: self.ctr },
            ctx: serde_json::from_str(&self.ctx).map_err(corrupt)?,
            obs: match self.obs {
                Some(text) => serde_json::from_str(&text).map_err(corrupt)?,
                None => Vec::new(),
            },
            kind: parse_kind(&self.kind)?,
            entity_id: self.entity_id,
            op_type: self.op_type,
            payload: serde_json::from_slice(&self.payload).map_err(corrupt)?,
        })
    }
}

fn parse_kind(text: &str) -> Result<Kind, StoreError> {
    // The inverse of `Kind::as_str`, without a serde round trip per row.
    match text {
        "memo" => Ok(Kind::Memo),
        "conv" => Ok(Kind::Conv),
        "tr" => Ok(Kind::Tr),
        "policy" => Ok(Kind::Policy),
        other => Err(corrupt(format!("entity kind {other}"))),
    }
}

fn insert_op_row(
    conn: &Connection,
    vault: &str,
    body: &OpBody,
    hashes: OpHashes,
    state: UploadState,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO ops(vault_id, op_id, origin_device, hlc_wall, hlc_ctr, ctx_frontier, obs, entity_kind,
             entity_id, op_type, payload, content_hash, body_hash, upload_state)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            vault,
            body.op_id,
            body.origin_device,
            body.hlc.wall_ms,
            body.hlc.ctr,
            ctx_text(&body.ctx)?,
            obs_text(&body.obs)?,
            body.kind.as_str(),
            body.entity_id,
            body.op_type,
            canonical_json(&body.payload)?,
            hashes.content.to_vec(),
            hashes.body.to_vec(),
            upload_state_text(state),
        ],
    )?;
    Ok(())
}

fn hashes_of(body: &OpBody) -> Result<OpHashes, StoreError> {
    Ok(OpHashes { content: content_hash(body)?, body: body_hash(body)? })
}

/// Stored op body and content hash.
fn read_op(conn: &Connection, vault: &str, op_id: &str) -> Result<Option<(OpBody, [u8; 32])>, StoreError> {
    let row = conn
        .query_row(
            &format!("SELECT {OP_COLS}, o.content_hash FROM ops o WHERE o.vault_id = ?1 AND o.op_id = ?2"),
            params![vault, op_id],
            |r| Ok((RawOp::read(r, 0)?, r.get::<_, Vec<u8>>(10)?)),
        )
        .optional()?;
    match row {
        Some((raw, ch)) => Ok(Some((raw.body()?, h32(ch)?))),
        None => Ok(None),
    }
}

fn load_forks(conn: &Connection, vault: &str) -> Result<Forks, StoreError> {
    let mut stmt = conn.prepare("SELECT device_id, fork_start FROM forks WHERE vault_id = ?1")?;
    let rows = stmt.query_map(params![vault], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
    Ok(rows.collect::<Result<Forks, _>>()?)
}

/// Every (body, dot) of the vault, loaded into a fresh op set.
fn load_set(conn: &Connection, vault: &str) -> Result<(OpSet, Forks), StoreError> {
    // Cached: this is the hottest statement in the store (every memo command,
    // every applied segment and every projection read runs it).
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT d.device_id, d.seq, {OP_COLS} FROM op_dots d
         JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
         WHERE d.vault_id = ?1 ORDER BY d.device_id, d.seq, d.op_id"
    ))?;
    let rows = stmt.query_map(params![vault], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?, RawOp::read(r, 2)?)))?;
    let mut set = OpSet::new();
    for row in rows {
        let (device, seq, raw) = row?;
        // Every row here was validated before it was stored, so an op error
        // (a float payload, an id that no longer parses) is local damage.
        set.insert(raw.body()?, Dot { device, seq }).map_err(|e| corrupt(format!("stored op: {e}")))?;
    }
    Ok((set, load_forks(conn, vault)?))
}

fn upsert_fork(conn: &Connection, vault: &str, device: &str, start: u64) -> Result<bool, StoreError> {
    let current: Option<u64> = conn
        .query_row(
            "SELECT fork_start FROM forks WHERE vault_id = ?1 AND device_id = ?2",
            params![vault, device],
            |r| r.get(0),
        )
        .optional()?;
    match current {
        Some(existing) if existing <= start => Ok(false),
        Some(_) => {
            conn.execute(
                "UPDATE forks SET fork_start = ?3, detected_at = ?4 WHERE vault_id = ?1 AND device_id = ?2",
                params![vault, device, start, now_ms()],
            )?;
            Ok(true)
        }
        None => {
            conn.execute(
                "INSERT INTO forks(vault_id, device_id, fork_start, detected_at) VALUES(?1, ?2, ?3, ?4)",
                params![vault, device, start, now_ms()],
            )?;
            Ok(true)
        }
    }
}

/// Writes a dot; a second op id at the same dot records a fork (§4.4).
fn record_dot(conn: &Connection, vault: &str, dot: &Dot, op_id: &str) -> Result<bool, StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO op_dots(vault_id, device_id, seq, op_id) VALUES(?1, ?2, ?3, ?4)",
        params![vault, dot.device, dot.seq, op_id],
    )?;
    let others: u64 = conn.query_row(
        "SELECT COUNT(*) FROM op_dots WHERE vault_id = ?1 AND device_id = ?2 AND seq = ?3 AND op_id <> ?4",
        params![vault, dot.device, dot.seq, op_id],
        |r| r.get(0),
    )?;
    if others > 0 {
        upsert_fork(conn, vault, &dot.device, dot.seq)
    } else {
        Ok(false)
    }
}

/// §4.5 step 4–6 for one remote op. Returns (conflict, fork_started).
fn store_remote_op(conn: &Connection, vault: &str, body: &OpBody, dot: &Dot) -> Result<(bool, bool), StoreError> {
    let hashes = hashes_of(body)?;
    match read_op(conn, vault, &body.op_id)? {
        Some((_, stored_hash)) if stored_hash != hashes.content => return Ok((true, false)),
        Some((mut stored, _)) => {
            // I6 alias: union ctx and obs, keep the smaller origin (as OpSet does).
            let mut changed = false;
            for (d, s) in &body.ctx {
                let slot = stored.ctx.entry(d.clone()).or_insert(0);
                if *s > *slot {
                    *slot = *s;
                    changed = true;
                }
            }
            let obs: BTreeSet<String> = stored.obs.iter().chain(&body.obs).cloned().collect();
            if obs.len() != stored.obs.len() {
                stored.obs = obs.into_iter().collect();
                changed = true;
            }
            if body.origin_device < stored.origin_device {
                stored.origin_device = body.origin_device.clone();
                changed = true;
            }
            if changed {
                conn.execute(
                    "UPDATE ops SET origin_device = ?3, ctx_frontier = ?4, obs = ?5 WHERE vault_id = ?1 AND op_id = ?2",
                    params![vault, stored.op_id, stored.origin_device, ctx_text(&stored.ctx)?, obs_text(&stored.obs)?],
                )?;
            }
        }
        None => insert_op_row(conn, vault, body, hashes, UploadState::Published)?,
    }
    Ok((false, record_dot(conn, vault, dot, &body.op_id)?))
}

fn read_cursor(conn: &Connection, vault: &str, device: &str) -> Result<Option<Cursor>, StoreError> {
    let row = conn
        .query_row(
            "SELECT applied_seq, chain_head, state FROM cursors WHERE vault_id = ?1 AND remote_device_id = ?2",
            params![vault, device],
            |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()?;
    match row {
        Some((applied_seq, head, state)) => Ok(Some(Cursor {
            applied_seq,
            chain_head: head.map(h32).transpose()?,
            state: CursorState::parse(&state)?,
        })),
        None => Ok(None),
    }
}

fn put_cursor(conn: &Connection, vault: &str, device: &str, cursor: &Cursor) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO cursors(vault_id, remote_device_id, applied_seq, chain_head, snapshot_id, state)
         VALUES(?1, ?2, ?3, ?4, NULL, ?5)
         ON CONFLICT(vault_id, remote_device_id) DO UPDATE SET
             applied_seq = excluded.applied_seq, chain_head = excluded.chain_head, state = excluded.state",
        params![vault, device, cursor.applied_seq, cursor.chain_head.map(|h| h.to_vec()), cursor.state.text()],
    )?;
    Ok(())
}

fn put_quarantine(conn: &Connection, vault: &str, key: &str, reason: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO quarantine(vault_id, key, reason, seen_at) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(vault_id, key) DO UPDATE SET reason = excluded.reason, seen_at = excluded.seen_at",
        params![vault, key, reason, now_ms()],
    )?;
    Ok(())
}

/// Known chain heads of a stream: `head → last_seq`. A forked stream (also
/// once held) keeps every head in `chain_nodes`; otherwise the cursor's one.
fn known_heads(
    conn: &Connection,
    vault: &str,
    device: &str,
    cursor: &Cursor,
) -> Result<BTreeMap<[u8; 32], u64>, StoreError> {
    let mut heads = BTreeMap::new();
    if cursor.chain_head.is_none() {
        let mut stmt = conn.prepare("SELECT head, last_seq FROM chain_nodes WHERE vault_id = ?1 AND device_id = ?2")?;
        let rows = stmt
            .query_map(params![vault, device], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (head, last) in rows {
            heads.insert(h32(head)?, last);
        }
    }
    if let Some(head) = cursor.chain_head {
        heads.insert(head, cursor.applied_seq);
    }
    Ok(heads)
}

fn put_node(conn: &Connection, vault: &str, device: &str, head: &[u8; 32], last: u64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO chain_nodes(vault_id, device_id, head, last_seq) VALUES(?1, ?2, ?3, ?4)",
        params![vault, device, head.to_vec(), last],
    )?;
    Ok(())
}

/// Fault classification is by cause (cleaner 1): only decoding or
/// validating the incoming segment and its ops can blame the remote object
/// (quarantine + held). This hashes the incoming segment; anything after the
/// schema step (loading or re-projecting local rows, cursor I/O) is a local
/// fault, returned as `Err` with the transaction rolled back.
fn incoming_hash(segment: &Segment) -> Result<[u8; 32], Step> {
    segment_hash(segment).map_err(|e| Step::Hold { reason: format!("segment does not encode: {e}"), quarantine: true })
}

/// The incoming seqs a forked-stream lookup binds as SQL integers (dedup,
/// anchors) must fit i64. Checked before that lookup, so a bind failure there
/// can only be local. The bound is `ops::MAX_STORED`, the one `Segment::validate`
/// enforces later.
fn unstorable_seq(segment: &Segment) -> Option<Step> {
    (segment.last_seq > MAX_STORED || segment.ops.iter().any(|op| op.seq > MAX_STORED))
        .then(|| Step::Hold { reason: "segment seq beyond the storable range".to_owned(), quarantine: true })
}

/// Highest seq applied without gaps, per device (cursors only).
fn cursor_frontier(conn: &Connection, vault: &str) -> Result<BTreeMap<String, u64>, StoreError> {
    let mut stmt =
        conn.prepare("SELECT remote_device_id, applied_seq FROM cursors WHERE vault_id = ?1 AND applied_seq > 0")?;
    let rows = stmt.query_map(params![vault], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?;
    Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
}

struct IdentityRow {
    device_id: String,
    generation: u64,
    sealed_seq: u64,
    chain_head: [u8; 32],
}

fn identity_row(conn: &Connection, vault: &str) -> Result<IdentityRow, StoreError> {
    let row = conn
        .query_row(
            "SELECT device_id, generation, sealed_seq, chain_head FROM vault_identity WHERE vault_id = ?1",
            params![vault],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?, r.get::<_, u64>(2)?, r.get::<_, Vec<u8>>(3)?)),
        )
        .optional()?;
    let (device_id, generation, sealed_seq, head) = row.ok_or(StoreError::NoIdentity)?;
    Ok(IdentityRow { device_id, generation, sealed_seq, chain_head: h32(head)? })
}

fn max_seq(conn: &Connection, vault: &str, device: &str) -> Result<u64, StoreError> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM op_dots WHERE vault_id = ?1 AND device_id = ?2",
        params![vault, device],
        |r| r.get(0),
    )?)
}

fn active_vault(conn: &Connection) -> Result<Option<String>, StoreError> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = 'active_vault_id'", [], |r| r.get(0)).optional()?)
}

/// One own op inside the caller's transaction: next own seq, exactly one dot
/// (I1), ctx = applied frontier plus the own previous seq, obs extended with
/// fork-range ops of the same entity. The caller re-projects.
fn insert_local_in(
    conn: &Connection,
    vault: &str,
    device: &str,
    mut body: OpBody,
    upload_state: UploadState,
) -> Result<(OpBody, Dot), StoreError> {
    let seq = max_seq(conn, vault, device)? + 1;
    let mut ctx = cursor_frontier(conn, vault)?;
    ctx.remove(device);
    if seq > 1 {
        ctx.insert(device.to_owned(), seq - 1);
    }
    body.origin_device = device.to_owned();
    body.ctx = ctx;
    let obs: BTreeSet<String> =
        body.obs.drain(..).chain(fork_range_obs(conn, vault, body.kind, &body.entity_id)?).collect();
    body.obs = obs.into_iter().collect();
    body.validate()?;
    insert_op_row(conn, vault, &body, hashes_of(&body)?, upload_state)?;
    let dot = Dot { device: device.to_owned(), seq };
    record_dot(conn, vault, &dot, &body.op_id)?;
    Ok((body, dot))
}

/// `meta` key holding the [`PROJECTION_VERSION`] the persisted projection
/// rows were written by.
const META_PROJECTION_VERSION: &str = "projection_version";

fn meta_get_conn(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
}

fn meta_set_conn(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Blob rows plus `vault_blob_map(vault, blob_id) = pending`, inside the
/// caller's transaction.
fn insert_blobs_in(conn: &Connection, vault: &str, blobs: &[BlobRow]) -> Result<(), StoreError> {
    for blob in blobs {
        // Blob ids are content-addressed: an existing row holds the same
        // bytes, and an existing map row keeps its upload state.
        conn.execute(
            "INSERT INTO blobs(blob_id, mime, bytes, sha256, local_path) VALUES(?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(blob_id) DO NOTHING",
            params![blob.blob_id, blob.mime, blob.bytes, blob.sha256.to_vec()],
        )?;
        conn.execute(
            "INSERT INTO vault_blob_map(vault_id, blob_id, local_blob, remote_state) VALUES(?1, ?2, ?2, 'pending')
             ON CONFLICT(vault_id, blob_id) DO NOTHING",
            params![vault, blob.blob_id],
        )?;
    }
    Ok(())
}

/// Recomputes the vault projection and persists it in full (§4.4
/// re-projection). Invariant kept by this and [`reproject_delta`]: after
/// every committed op-writing transaction, `memo_lineage` (per vault) and,
/// for the active vault, `entities` / `msg_tree` mirror the committed
/// projection.
fn reproject_conn(conn: &Connection, vault: &str) -> Result<Projection, StoreError> {
    let (set, forks) = load_set(conn, vault)?;
    let projection = project(&set, &forks)?;
    conn.execute("DELETE FROM memo_lineage WHERE vault_id = ?1", params![vault])?;
    persist_lineage_rows(conn, vault, &set, projection.memo_lineage.iter())?;
    if active_vault(conn)?.as_deref() == Some(vault) {
        persist_entities(conn, &set, &projection)?;
    }
    Ok(projection)
}

/// [`reproject_conn`] that writes only what changed. Preconditions, both
/// the caller's to keep inside one transaction: `set`/`forks` are exactly
/// what `load_set` returns for the connection's current state, and
/// `before` is the projection the persisted rows mirror right now (the
/// projection of the same connection before this transaction's inserts,
/// with the active vault unchanged since the last re-projection). Under
/// the invariant on [`reproject_conn`] the result is row-for-row what a full
/// rewrite would produce, without rewriting the rows a command leaves alone.
fn reproject_delta(
    conn: &Connection,
    vault: &str,
    set: &OpSet,
    forks: &Forks,
    before: &Projection,
) -> Result<Projection, StoreError> {
    let projection = project(set, forks)?;
    // Lineage: rows that are gone, then rows that are new or re-routed.
    let mut gone = conn.prepare_cached("DELETE FROM memo_lineage WHERE vault_id = ?1 AND op_id = ?2")?;
    for op_id in before.memo_lineage.keys() {
        if !projection.memo_lineage.contains_key(op_id) {
            gone.execute(params![vault, op_id])?;
        }
    }
    let changed = projection.memo_lineage.iter().filter(|(op_id, to)| before.memo_lineage.get(*op_id) != Some(to));
    persist_lineage_rows(conn, vault, set, changed)?;
    if active_vault(conn)?.as_deref() == Some(vault) {
        persist_entities_delta(conn, set, before, &projection)?;
    }
    Ok(projection)
}

/// Upserts the given lineage rows (the caller deleted whatever must go).
fn persist_lineage_rows<'a>(
    conn: &Connection,
    vault: &str,
    set: &OpSet,
    rows: impl Iterator<Item = (&'a String, &'a String)>,
) -> Result<(), StoreError> {
    let mut insert = conn.prepare_cached(
        "INSERT INTO memo_lineage(vault_id, memo_id, op_id, base_op_id, routed_to) VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(vault_id, op_id) DO UPDATE SET
             memo_id = excluded.memo_id, base_op_id = excluded.base_op_id, routed_to = excluded.routed_to",
    )?;
    for (op_id, routed_to) in rows {
        let Some(entry) = set.get(op_id) else { continue };
        let base = refs(&entry.body).first().copied();
        insert.execute(params![vault, entry.body.entity_id, op_id, base, routed_to])?;
    }
    Ok(())
}

const MSG_TREE_INSERT: &str =
    "INSERT OR REPLACE INTO msg_tree(conv_id, msg_id, parent_id, created_hlc) VALUES(?1, ?2, ?3, ?4)";

/// `(state_json, deleted)` of an `entities` row.
fn entity_row(state: &EntityState) -> Result<(String, bool), StoreError> {
    let value = serde_json::to_value(state).map_err(corrupt)?;
    Ok((json_text(&value)?, matches!(state, EntityState::Deleted)))
}

/// The `msg_tree` rows of one conversation (the caller cleared the old ones).
fn insert_msg_rows(
    insert_msg: &mut rusqlite::CachedStatement<'_>,
    set: &OpSet,
    conv_id: &str,
    conv: &super::merge::ConvState,
) -> Result<(), StoreError> {
    for (msg_id, msg) in &conv.messages {
        let hlc = match set.get(&msg.op_id) {
            Some(entry) => Some(json_text(&serde_json::to_value(entry.body.hlc).map_err(corrupt)?)?),
            None => None,
        };
        insert_msg.execute(params![conv_id, msg_id, msg.parent_id, hlc])?;
    }
    Ok(())
}

/// One entity's rows as [`persist_entities`] writes them: the `entities`
/// row is upserted keeping `evicted_local`; a conversation key always has
/// its old `msg_tree` rows cleared first (a tombstoned conversation keeps
/// none), then the live messages re-inserted.
fn persist_entity_row(
    conn: &Connection,
    set: &OpSet,
    kind: Kind,
    id: &str,
    state: &EntityState,
) -> Result<(), StoreError> {
    let (state_json, deleted) = entity_row(state)?;
    conn.prepare_cached(
        "INSERT INTO entities(kind, id, state_json, deleted, evicted_local) VALUES(?1, ?2, ?3, ?4, 0)
         ON CONFLICT(kind, id) DO UPDATE SET state_json = excluded.state_json, deleted = excluded.deleted",
    )?
    .execute(params![kind.as_str(), id, state_json, deleted])?;
    if kind == Kind::Conv {
        conn.prepare_cached("DELETE FROM msg_tree WHERE conv_id = ?1")?.execute(params![id])?;
        if let EntityState::Conv(conv) = state {
            insert_msg_rows(&mut conn.prepare_cached(MSG_TREE_INSERT)?, set, id, conv)?;
        }
    }
    Ok(())
}

/// [`persist_entities`] for the entities whose state differs between
/// `before` and `after`; everything else is already on disk.
fn persist_entities_delta(
    conn: &Connection,
    set: &OpSet,
    before: &Projection,
    after: &Projection,
) -> Result<(), StoreError> {
    for (key, state) in &after.entities {
        if before.entities.get(key) != Some(state) {
            persist_entity_row(conn, set, key.0, &key.1, state)?;
        }
    }
    let mut drop_entity = conn.prepare_cached("DELETE FROM entities WHERE kind = ?1 AND id = ?2")?;
    let mut drop_msgs = conn.prepare_cached("DELETE FROM msg_tree WHERE conv_id = ?1")?;
    for key in before.entities.keys() {
        if !after.entities.contains_key(key) {
            drop_entity.execute(params![key.0.as_str(), key.1])?;
            if key.0 == Kind::Conv {
                drop_msgs.execute(params![key.1])?;
            }
        }
    }
    Ok(())
}

/// Rewrites `entities` and `msg_tree` from `projection` (every applied
/// segment or snapshot, checked/queue batches, vault switch, rotation).
/// Cached statements and one serialisation per row: this is on the path of
/// every remote apply, so its cost is sync latency.
fn persist_entities(conn: &Connection, set: &OpSet, projection: &Projection) -> Result<(), StoreError> {
    let evicted: BTreeSet<(String, String)> = {
        let mut stmt = conn.prepare_cached("SELECT kind, id FROM entities WHERE evicted_local = 1")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    conn.execute("DELETE FROM entities", [])?;
    conn.execute("DELETE FROM msg_tree", [])?;
    let mut insert_entity = conn
        .prepare_cached("INSERT INTO entities(kind, id, state_json, deleted, evicted_local) VALUES(?1, ?2, ?3, ?4, ?5)")?;
    let mut insert_msg = conn.prepare_cached(MSG_TREE_INSERT)?;
    for ((kind, id), state) in &projection.entities {
        let (state_json, deleted) = entity_row(state)?;
        let is_evicted = evicted.contains(&(kind.as_str().to_owned(), id.clone()));
        insert_entity.execute(params![kind.as_str(), id, state_json, deleted, is_evicted])?;
        if let EntityState::Conv(conv) = state {
            insert_msg_rows(&mut insert_msg, set, id, conv)?;
        }
    }
    Ok(())
}

/// Ops of fork ranges (`seq >= F[d]`) touching the same entity, directly or
/// through memo lineage routing (§4.4 obs rule (ii)). A vault with no
/// recorded fork (the usual case) has no fork range, so nothing to scan.
fn fork_range_obs(conn: &Connection, vault: &str, kind: Kind, entity_id: &str) -> Result<Vec<String>, StoreError> {
    let forked: bool =
        conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM forks WHERE vault_id = ?1)")?.query_row(params![vault], |r| r.get(0))?;
    if !forked {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT d.op_id FROM op_dots d
         JOIN forks f ON f.vault_id = d.vault_id AND f.device_id = d.device_id AND d.seq >= f.fork_start
         JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
         LEFT JOIN memo_lineage l ON l.vault_id = d.vault_id AND l.op_id = d.op_id
         WHERE d.vault_id = ?1 AND o.entity_kind = ?2 AND (o.entity_id = ?3 OR l.routed_to = ?3)",
    )?;
    let rows = stmt.query_map(params![vault, kind.as_str(), entity_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Builds an [`OpBody`] from a planner op: the payload already carries the
/// original refs; any ref the planner repointed is remapped into it.
fn own_op_body(op: &OwnOp) -> Result<OpBody, StoreError> {
    let mut body = OpBody {
        v: FORMAT_V,
        op_id: op.op_id.clone(),
        origin_device: op.origin_device.clone(),
        hlc: Hlc { wall_ms: op.hlc_wall, ctr: op.hlc_ctr },
        ctx: op.ctx.clone(),
        obs: op.obs.iter().cloned().collect(),
        kind: parse_kind(&op.entity_kind)?,
        entity_id: op.entity_id.clone(),
        op_type: op.op_type.clone(),
        payload: serde_json::from_slice(&op.payload).map_err(corrupt)?,
    };
    let original: Vec<String> = refs(&body).iter().map(|s| (*s).to_owned()).collect();
    if original.len() != op.refs.len() {
        return Err(StoreError::Invalid(format!("refs of {} do not match its payload", op.op_id)));
    }
    for (old, new) in original.iter().zip(&op.refs) {
        if old != new {
            remap_ref(&mut body, old, new);
        }
    }
    Ok(body)
}

pub(crate) struct Store {
    conn: Connection,
    /// `(vault, own device) → limit` of the last `offer_deletable`: the
    /// highest seq compaction may delete remotely. Memory only: a restart
    /// forgets it, so the engine must offer again before any trim.
    offers: RefCell<BTreeMap<(String, String), u64>>,
}

impl Store {
    /// Opens (or creates) the store file in WAL mode and migrates it.
    pub(crate) fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::Invalid(format!("journal_mode stayed {mode}")));
        }
        Store::init(conn)
    }

    /// One `meta` value of an existing store file, read without creating,
    /// migrating, or writing anything (G8 E1: the flag-off native check).
    /// `None` when the file, the table, or the key is missing. A stat that
    /// fails for any reason other than `NotFound`, or a path that is not a
    /// regular file, is an error (E1a), never "missing".
    pub(crate) fn peek_meta(path: &Path, key: &str) -> Result<Option<String>, StoreError> {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(StoreError::Invalid(format!("{} is not a regular file", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e)),
        }
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let has_meta: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |r| r.get(0),
        )?;
        if !has_meta {
            return Ok(None);
        }
        Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn open_in_memory() -> Result<Store, StoreError> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store, StoreError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut store = Store { conn, offers: RefCell::new(BTreeMap::new()) };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let version: i64 = self.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let version = usize::try_from(version).map_err(|_| corrupt("negative user_version"))?;
        if version > MIGRATIONS.len() {
            return Err(StoreError::Invalid(format!("database version {version} is newer than this build")));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        // The persisted projection was written by whatever `project()` the
        // last build had. If that differs from ours, rewrite it once so the
        // incremental path (`reproject_delta`) diffs against rows this code
        // produced.
        let stamp = PROJECTION_VERSION.to_string();
        if meta_get_conn(&self.conn, META_PROJECTION_VERSION)?.as_deref() != Some(stamp.as_str()) {
            let tx = self.conn.transaction()?;
            if let Some(vault) = active_vault(&tx)? {
                reproject_conn(&tx, &vault)?;
            }
            meta_set_conn(&tx, META_PROJECTION_VERSION, &stamp)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn journal_mode(&self) -> Result<String, StoreError> {
        Ok(self.conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?)
    }

    pub(crate) fn active_vault(&self) -> Result<Option<String>, StoreError> {
        active_vault(&self.conn)
    }

    /// Test seam for the active vault (production switches through
    /// [`Store::commit_vault_switch`]). Re-projects in the same transaction
    /// so `entities` / `msg_tree` mirror the newly active vault, the
    /// invariant [`reproject_delta`] relies on.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn set_active_vault(&mut self, vault: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('active_vault_id', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![vault],
        )?;
        reproject_conn(&tx, vault)?;
        tx.commit()?;
        Ok(())
    }

    /// Writes this device's first identity in a vault (seq 1 on the genesis
    /// chain). An existing identity for the vault is an error.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn init_identity(&mut self, identity: &DeviceIdentity) -> Result<(), StoreError> {
        let head = genesis_chain_head(&identity.vault_id, &identity.device_id);
        self.conn.execute(
            "INSERT INTO vault_identity(vault_id, device_id, generation, sealed_seq, chain_head, binding_fp)
             VALUES(?1, ?2, ?3, 0, ?4, NULL)",
            params![identity.vault_id, identity.device_id, identity.generation, head.to_vec()],
        )?;
        Ok(())
    }

    pub(crate) fn device_id(&self, vault: &str) -> Result<String, StoreError> {
        Ok(identity_row(&self.conn, vault)?.device_id)
    }

    /// The SQLite copy of the identity, for [`super::identity::detect`].
    pub(crate) fn local_identity(&self, vault: &str) -> Result<Option<LocalIdentity>, StoreError> {
        let row = match identity_row(&self.conn, vault) {
            Ok(row) => row,
            Err(StoreError::NoIdentity) => return Ok(None),
            Err(e) => return Err(e),
        };
        let install_fp: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'install_fp'", [], |r| r.get(0))
            .optional()?;
        Ok(Some(LocalIdentity {
            device_id: row.device_id,
            install_fp,
            generation: row.generation,
            sealed_seq: row.sealed_seq,
            chain_head: row.chain_head,
        }))
    }

    /// Stores a new own op: next own seq, exactly one dot (I1), ctx = applied
    /// frontier plus the own previous seq, obs extended with fork-range ops of
    /// the same entity, projection updated in the same transaction. The
    /// caller's `origin_device` and `ctx` are replaced.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn insert_local_op(
        &mut self,
        vault: &str,
        body: OpBody,
        upload_state: UploadState,
    ) -> Result<LocalOp, StoreError> {
        let tx = self.conn.transaction()?;
        let device = identity_row(&tx, vault)?.device_id;
        let (body, dot) = insert_local_in(&tx, vault, &device, body, upload_state)?;
        let projection = reproject_conn(&tx, vault)?;
        tx.commit()?;
        Ok(LocalOp { body, dot, projection_hash: projection.hash })
    }

    /// Several own ops, blobs, and meta rows in one transaction (G4): each
    /// body gets the [`Store::insert_local_op`] semantics (upload state
    /// `never`), blobs go to `blobs` plus `vault_blob_map` as `pending`, meta
    /// rows are upserted, and the projection is recomputed once at the end.
    /// All or nothing: any invalid body stores nothing and consumes no seq.
    /// Every returned op carries the final projection hash. Production uses
    /// [`Store::insert_local_batch_checked`] (migration) and
    /// [`Store::plan_local_batch`] (memo commands).
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn insert_local_batch(
        &mut self,
        vault: &str,
        bodies: Vec<OpBody>,
        blobs: Vec<BlobRow>,
        meta: Vec<(String, String)>,
    ) -> Result<Vec<LocalOp>, StoreError> {
        let (stored, _) = self.insert_local_batch_checked(vault, bodies, blobs, meta, |_| true)?;
        Ok(stored.unwrap_or_default())
    }

    /// [`Store::insert_local_batch`] whose COMMIT is conditional (G4 finalize
    /// re-verify): `decide` sees the recomputed projection inside the
    /// transaction; `false` rolls everything back (`None`, nothing written).
    /// Also returns that projection.
    pub(crate) fn insert_local_batch_checked(
        &mut self,
        vault: &str,
        bodies: Vec<OpBody>,
        blobs: Vec<BlobRow>,
        meta: Vec<(String, String)>,
        decide: impl FnOnce(&Projection) -> bool,
    ) -> Result<(Option<Vec<LocalOp>>, Projection), StoreError> {
        self.batch_in(vault, bodies, blobs, meta, |_| Ok(()), decide)
    }

    /// [`Store::insert_local_batch`] of one G8 queue item: in the same
    /// transaction, `meta` (the flushed watermark) and the item's op hash
    /// under `qseq` in `queue_op_hashes`, keeping only the newest
    /// [`QUEUE_HASH_WINDOW`] qseqs (E3). All or nothing.
    pub(crate) fn insert_queue_batch(
        &mut self,
        vault: &str,
        qseq: u64,
        op_hash: &[u8; 32],
        bodies: Vec<OpBody>,
        blobs: Vec<BlobRow>,
        meta: Vec<(String, String)>,
    ) -> Result<(), StoreError> {
        let qseq = i64::try_from(qseq).map_err(|_| StoreError::Invalid("qseq out of range".to_owned()))?;
        let record = |tx: &Connection| {
            tx.execute(
                "INSERT INTO queue_op_hashes(qseq, op_hash) VALUES(?1, ?2)
                 ON CONFLICT(qseq) DO UPDATE SET op_hash = excluded.op_hash",
                params![qseq, op_hash.to_vec()],
            )?;
            tx.execute(
                "DELETE FROM queue_op_hashes WHERE qseq NOT IN
                 (SELECT qseq FROM queue_op_hashes ORDER BY qseq DESC LIMIT ?1)",
                params![QUEUE_HASH_WINDOW as i64],
            )?;
            Ok(())
        };
        self.batch_in(vault, bodies, blobs, meta, record, |_| true).map(|_| ())
    }

    /// The op hash recorded for queue item `qseq`, if it is still inside the
    /// window.
    pub(crate) fn queue_op_hash(&self, qseq: u64) -> Result<Option<[u8; 32]>, StoreError> {
        let Ok(qseq) = i64::try_from(qseq) else { return Ok(None) };
        let row: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT op_hash FROM queue_op_hashes WHERE qseq = ?1", params![qseq], |r| r.get(0))
            .optional()?;
        row.map(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| corrupt("queue op hash length"))).transpose()
    }

    /// The qseqs whose op hash is recorded, ascending.
    #[cfg(test)]
    pub(crate) fn queue_hash_qseqs(&self) -> Result<Vec<u64>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT qseq FROM queue_op_hashes ORDER BY qseq")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.map(|r| u64::try_from(r?).map_err(|_| corrupt("negative qseq"))).collect()
    }

    /// Own ops planned against the current projection, in one transaction
    /// (G10 A15; the memo-command path). Contract: one SQL load and one
    /// projection serve both the plan and the re-projection; the planner
    /// sees exactly the projection its ops are inserted against; an empty
    /// plan inserts nothing and commits nothing; only the rows the ops
    /// changed are rewritten ([`reproject_delta`]). Same op semantics as
    /// [`Store::insert_local_batch`] (upload state `never`, all or nothing).
    pub(crate) fn plan_local_batch<T>(
        &mut self,
        vault: &str,
        plan: impl FnOnce(&Projection) -> (Vec<OpBody>, T),
    ) -> Result<(Vec<LocalOp>, T), StoreError> {
        let tx = self.conn.transaction()?;
        let (mut set, forks) = load_set(&tx, vault)?;
        let current = project(&set, &forks)?;
        let (bodies, out) = plan(&current);
        if bodies.is_empty() {
            return Ok((Vec::new(), out));
        }
        let device = identity_row(&tx, vault)?.device_id;
        let mut stored = Vec::with_capacity(bodies.len());
        for body in bodies {
            let (body, dot) = insert_local_in(&tx, vault, &device, body, UploadState::Never)?;
            // Keep the loaded set in step with the row just written, so the
            // re-projection below needs no second load. A local op is a new
            // op id at a fresh own seq: never an alias, never a fork.
            let inserted = set.insert(body.clone(), dot.clone()).map_err(|e| corrupt(format!("local op: {e}")))?;
            if inserted.outcome != super::causal::Insert::New || inserted.fork_started {
                return Err(corrupt(format!("local op {} is not new in its own set", body.op_id)));
            }
            stored.push((body, dot));
        }
        let projection = reproject_delta(&tx, vault, &set, &forks, &current)?;
        tx.commit()?;
        let stored =
            stored.into_iter().map(|(body, dot)| LocalOp { body, dot, projection_hash: projection.hash }).collect();
        Ok((stored, out))
    }

    fn batch_in(
        &mut self,
        vault: &str,
        bodies: Vec<OpBody>,
        blobs: Vec<BlobRow>,
        meta: Vec<(String, String)>,
        extra: impl FnOnce(&Connection) -> Result<(), StoreError>,
        decide: impl FnOnce(&Projection) -> bool,
    ) -> Result<(Option<Vec<LocalOp>>, Projection), StoreError> {
        let tx = self.conn.transaction()?;
        let device = identity_row(&tx, vault)?.device_id;
        let mut stored = Vec::with_capacity(bodies.len());
        for body in bodies {
            stored.push(insert_local_in(&tx, vault, &device, body, UploadState::Never)?);
        }
        insert_blobs_in(&tx, vault, &blobs)?;
        for (key, value) in &meta {
            meta_set_conn(&tx, key, value)?;
        }
        extra(&tx)?;
        let projection = reproject_conn(&tx, vault)?;
        if !decide(&projection) {
            tx.rollback()?;
            return Ok((None, projection));
        }
        tx.commit()?;
        let stored =
            stored.into_iter().map(|(body, dot)| LocalOp { body, dot, projection_hash: projection.hash }).collect();
        Ok((Some(stored), projection))
    }

    pub(crate) fn meta_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        meta_get_conn(&self.conn, key)
    }

    pub(crate) fn meta_set(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        meta_set_conn(&self.conn, key, value)
    }

    /// Vault switch J3–J7 (§4.2.2, G7 vault): in ONE transaction, the new
    /// device identity for `vault` (seq 1, genesis chain), the active-vault
    /// switch, the republished ops (upload state `never`, dots on the new
    /// identity), their blobs, and `meta`. The projection is recomputed and
    /// `decide` must accept it, else everything rolls back and the previously
    /// active vault stays active. An existing identity in `vault` is refused
    /// (the insert fails and the transaction rolls back).
    pub(crate) fn commit_vault_switch(
        &mut self,
        identity: &DeviceIdentity,
        bodies: Vec<OpBody>,
        blobs: Vec<BlobRow>,
        meta: Vec<(String, String)>,
        decide: impl FnOnce(&Projection) -> bool,
    ) -> Result<Option<Vec<LocalOp>>, StoreError> {
        let vault = identity.vault_id.as_str();
        let tx = self.conn.transaction()?;
        let head = genesis_chain_head(vault, &identity.device_id);
        tx.execute(
            "INSERT INTO vault_identity(vault_id, device_id, generation, sealed_seq, chain_head, binding_fp)
             VALUES(?1, ?2, ?3, 0, ?4, NULL)",
            params![vault, identity.device_id, identity.generation, head.to_vec()],
        )?;
        meta_set_conn(&tx, "active_vault_id", vault)?;
        let mut stored = Vec::with_capacity(bodies.len());
        for body in bodies {
            stored.push(insert_local_in(&tx, vault, &identity.device_id, body, UploadState::Never)?);
        }
        insert_blobs_in(&tx, vault, &blobs)?;
        for (key, value) in &meta {
            meta_set_conn(&tx, key, value)?;
        }
        let projection = reproject_conn(&tx, vault)?;
        if !decide(&projection) {
            tx.rollback()?;
            return Ok(None);
        }
        tx.commit()?;
        Ok(Some(stored.into_iter().map(|(body, dot)| LocalOp { body, dot, projection_hash: projection.hash }).collect()))
    }

    /// `(mime, bytes)` of a local blob, or `None` when it is not stored here.
    pub(crate) fn blob_bytes(&self, blob_id: &str) -> Result<Option<(String, Vec<u8>)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(mime, ''), bytes FROM blobs WHERE blob_id = ?1 AND bytes IS NOT NULL",
                params![blob_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?)
    }

    /// `(kind, id)` of every entity of the active vault whose content was
    /// evicted locally (`entities.evicted_local`).
    pub(crate) fn evicted_entities(&self) -> Result<BTreeSet<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT kind, id FROM entities WHERE evicted_local = 1")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// §4.5: applies one decrypted segment of `address_device` in one
    /// transaction. See the module doc for the failure taxonomy.
    pub(crate) fn apply_segment(
        &mut self,
        vault: &str,
        address_device: &str,
        first: u64,
        last: u64,
        segment: &Segment,
    ) -> Result<ApplyOutcome, StoreError> {
        self.apply_keyed(vault, &seg_key(address_device, first, last), address_device, first, last, segment)
    }

    /// Opens a sealed segment fetched at `address` and applies it.
    pub(crate) fn apply_sealed(
        &mut self,
        vault: &str,
        key: &SecretKey,
        address: &ObjectAddress,
        envelope: &[u8],
    ) -> Result<ApplyOutcome, StoreError> {
        let qkey = address.remote_key().unwrap_or_else(|| format!("seg/{}/{}", address.device_id, address.object_ref));
        if address.vault_id != vault {
            return self.quarantine_only(vault, &qkey, "address names another vault");
        }
        match open_segment(key, address, envelope) {
            Ok(segment) => {
                let (first, last) = (segment.first_seq, segment.last_seq);
                self.apply_keyed(vault, &qkey, &address.device_id, first, last, &segment)
            }
            Err(SealedError::Corrupt(why)) => self.quarantine_only(vault, &qkey, &why),
            Err(SealedError::FutureVersion(v)) => self.record_failure(vault, &address.device_id, &qkey, Step::Future(v)),
        }
    }

    fn quarantine_only(&mut self, vault: &str, key: &str, why: &str) -> Result<ApplyOutcome, StoreError> {
        put_quarantine(&self.conn, vault, key, why)?;
        Ok(ApplyOutcome::Quarantined { reason: why.to_owned() })
    }

    fn apply_keyed(
        &mut self,
        vault: &str,
        key: &str,
        device: &str,
        first: u64,
        last: u64,
        segment: &Segment,
    ) -> Result<ApplyOutcome, StoreError> {
        if let Err(e) = check_plaintext(segment, device, first, last) {
            return self.quarantine_only(vault, key, &e.to_string());
        }
        let tx = self.conn.transaction()?;
        let step = apply_in(&tx, vault, device, segment)?;
        match step {
            Step::Done(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            failed => {
                tx.rollback()?;
                self.record_failure(vault, device, key, failed)
            }
        }
    }

    /// After a ROLLBACK: the quarantine row and `held` cursor go in their own
    /// transaction, so no op of the failed segment is ever applied.
    fn record_failure(&mut self, vault: &str, device: &str, key: &str, step: Step) -> Result<ApplyOutcome, StoreError> {
        let tx = self.conn.transaction()?;
        let mut cursor = read_cursor(&tx, vault, device)?.unwrap_or_else(|| Cursor::genesis(vault, device));
        cursor.state = CursorState::Held;
        put_cursor(&tx, vault, device, &cursor)?;
        let outcome = match step {
            Step::Done(outcome) => outcome,
            Step::Future(v) => ApplyOutcome::FutureVersion(v),
            Step::Hold { reason, quarantine } => {
                if quarantine {
                    put_quarantine(&tx, vault, key, &reason)?;
                }
                ApplyOutcome::Held { reason }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// §4.2.3 (a): two or more variants of one range, each already opened and
    /// cross-checked. Records `forks(d, min)`, applies every variant that
    /// attaches to a known head (or is already held) and defers the rest, and
    /// marks the stream `forked`; other streams are unaffected.
    pub(crate) fn apply_fork_variant(
        &mut self,
        vault: &str,
        address_device: &str,
        first: u64,
        last: u64,
        variants: &[Segment],
    ) -> Result<ApplyOutcome, StoreError> {
        let key = seg_key(address_device, first, last);
        for variant in variants {
            if let Err(e) = check_plaintext(variant, address_device, first, last) {
                return self.quarantine_only(vault, &key, &e.to_string());
            }
        }
        let mut distinct: BTreeMap<[u8; 32], &Segment> = BTreeMap::new();
        for variant in variants {
            match segment_hash(variant) {
                Ok(hash) => distinct.insert(hash, variant),
                Err(e) => return self.quarantine_only(vault, &key, &e.to_string()),
            };
        }
        let distinct: Vec<&Segment> = distinct.into_values().collect();
        match distinct.as_slice() {
            [] => return Err(StoreError::Invalid("no variants".to_owned())),
            [one] => return self.apply_keyed(vault, &key, address_device, first, last, one),
            _ => {}
        }
        let tx = self.conn.transaction()?;
        let step = fork_in(&tx, vault, address_device, first, last, &distinct)?;
        match step {
            Step::Done(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            failed => {
                tx.rollback()?;
                self.record_failure(vault, address_device, &key, failed)
            }
        }
    }

    /// Recomputes and persists the projection; returns its hash.
    pub(crate) fn reproject(&mut self, vault: &str) -> Result<[u8; 32], StoreError> {
        let tx = self.conn.transaction()?;
        let projection = reproject_conn(&tx, vault)?;
        tx.commit()?;
        Ok(projection.hash)
    }

    /// The projection of the stored op set, without writing anything.
    pub(crate) fn projection(&self, vault: &str) -> Result<Projection, StoreError> {
        let (set, forks) = load_set(&self.conn, vault)?;
        Ok(project(&set, &forks)?)
    }

    pub(crate) fn load_set(&self, vault: &str) -> Result<(OpSet, Forks), StoreError> {
        load_set(&self.conn, vault)
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn forks(&self, vault: &str) -> Result<Forks, StoreError> {
        load_forks(&self.conn, vault)
    }

    pub(crate) fn cursor(&self, vault: &str, device: &str) -> Result<Option<Cursor>, StoreError> {
        read_cursor(&self.conn, vault, device)
    }

    /// Cursor frontier plus this device's own highest seq.
    pub(crate) fn frontier(&self, vault: &str) -> Result<BTreeMap<String, u64>, StoreError> {
        let mut frontier = cursor_frontier(&self.conn, vault)?;
        let device = identity_row(&self.conn, vault)?.device_id;
        let own = max_seq(&self.conn, vault, &device)?;
        if own > 0 {
            frontier.insert(device, own);
        }
        Ok(frontier)
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn quarantine_rows(&self, vault: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT key, reason FROM quarantine WHERE vault_id = ?1 ORDER BY key")?;
        let rows = stmt.query_map(params![vault], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every dot an op is stored at.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn dots_of(&self, vault: &str, op_id: &str) -> Result<Vec<Dot>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT device_id, seq FROM op_dots WHERE vault_id = ?1 AND op_id = ?2 ORDER BY device_id, seq",
        )?;
        let rows = stmt.query_map(params![vault, op_id], |r| Ok(Dot { device: r.get(0)?, seq: r.get(1)? }))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// (content_hash, body_hash, upload_state) of a stored op.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn op_state(
        &self,
        vault: &str,
        op_id: &str,
    ) -> Result<Option<OpState>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT content_hash, body_hash, upload_state FROM ops WHERE vault_id = ?1 AND op_id = ?2",
                params![vault, op_id],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?;
        match row {
            Some((c, b, s)) => Ok(Some((h32(c)?, h32(b)?, parse_upload_state(&s)?))),
            None => Ok(None),
        }
    }

    /// Seals every own op past `sealed_seq` into the next segment (no size
    /// split; the engine uses [`Store::seal_pending_capped`]) and persists
    /// its bytes before any put. `seal` encrypts the canonical plaintext.
    /// Returns `None` when nothing is unsealed.
    #[cfg_attr(not(test), allow(dead_code))] // tests seal one segment directly
    pub(crate) fn seal_pending(
        &mut self,
        vault: &str,
        seal: impl FnOnce(&Segment, &[u8]) -> Result<Vec<u8>, CryptoError>,
    ) -> Result<Option<PendingSegment>, StoreError> {
        let mut seal = Some(seal);
        let mut sealed = self.seal_pending_capped(vault, usize::MAX, now_ms(), |segment, plain| match seal.take() {
            Some(seal) => seal(segment, plain),
            None => unreachable!("an uncapped seal makes one segment"),
        })?;
        Ok(sealed.pop())
    }

    /// Sealed bytes of an own segment; a resend uses exactly these (R3).
    pub(crate) fn sealed_bytes(
        &self,
        vault: &str,
        device: &str,
        first: u64,
    ) -> Result<Option<PendingSegment>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_seq, bytes FROM pending_segments WHERE vault_id = ?1 AND device_id = ?2 AND first_seq = ?3",
                params![vault, device, first],
                |r| {
                    Ok(PendingSegment {
                        device_id: device.to_owned(),
                        first_seq: first,
                        last_seq: r.get(0)?,
                        bytes: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub(crate) fn pending_segments(&self, vault: &str, device: &str) -> Result<Vec<(u64, u64, u64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT first_seq, last_seq, created_at FROM pending_segments
             WHERE vault_id = ?1 AND device_id = ?2 ORDER BY first_seq",
        )?;
        let rows = stmt.query_map(params![vault, device], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The one deletable predicate (architect B2, red-team p2_p2): own sealed
    /// ranges `(first, last)` with `last <= limit`, on the current own device
    /// id whose stream is neither forked nor retired, whose every seq holds a
    /// `published` own op: the published dot count must be `last - first + 1`,
    /// so a range with missing dots is never vacuously all published. An
    /// `attempted` range still needs its exact bytes (R3); a `rejected`
    /// range's remote name belongs to another writer.
    fn deletable(&self, vault: &str, device: &str, limit: u64) -> Result<Vec<(u64, u64)>, StoreError> {
        let conn = &self.conn;
        if identity_row(conn, vault)?.device_id != device || load_forks(conn, vault)?.contains_key(device) {
            return Ok(Vec::new());
        }
        if read_cursor(conn, vault, device)?
            .is_some_and(|c| matches!(c.state, CursorState::Retired | CursorState::Forked))
        {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for (first, last, _) in self.pending_segments(vault, device)? {
            if last > limit || last < first {
                continue;
            }
            let (published, unpublished): (u64, u64) = conn.query_row(
                "SELECT COUNT(DISTINCT CASE WHEN o.upload_state = 'published' THEN d.seq END),
                        COUNT(CASE WHEN o.upload_state IS NOT 'published' THEN 1 END)
                 FROM op_dots d LEFT JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
                 WHERE d.vault_id = ?1 AND d.device_id = ?2 AND d.seq BETWEEN ?3 AND ?4",
                params![vault, device, first, last],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if unpublished == 0 && Some(published) == (last - first).checked_add(1) {
                out.push((first, last));
            }
        }
        Ok(out)
    }

    /// Compaction offers the ranges the predicate allows up to `limit` (the
    /// verified snapshot frontier and every active cursor, 0 when nothing
    /// verified) and remembers that limit for `forget_segments`. A new offer
    /// replaces the last one, so an unverified pass revokes it.
    pub(crate) fn offer_deletable(&self, vault: &str, device: &str, limit: u64) -> Result<Vec<(u64, u64)>, StoreError> {
        let offered = self.deletable(vault, device, limit)?;
        self.offers.borrow_mut().insert((vault.to_owned(), device.to_owned()), limit);
        Ok(offered)
    }

    /// Compaction's last step (§4.5): after the provider confirmed the remote
    /// delete of these own segments, forget their sealed bytes. Accepts
    /// exactly what `offer_deletable` offers: the same predicate is evaluated
    /// again on the current state under the last offered limit, so a range
    /// never offered, or no longer deletable (forked, retired, a newer offer
    /// with a lower limit), is refused. All ranges go in one transaction; a
    /// refusal changes nothing. The ops and dots stay: local compaction never
    /// deletes op rows.
    pub(crate) fn forget_segments(
        &mut self,
        vault: &str,
        device: &str,
        ranges: &[(u64, u64)],
    ) -> Result<usize, StoreError> {
        let limit = self.offers.borrow().get(&(vault.to_owned(), device.to_owned())).copied().unwrap_or(0);
        let allowed: BTreeSet<(u64, u64)> = self.deletable(vault, device, limit)?.into_iter().collect();
        let mut seen = BTreeSet::new();
        for &(first, last) in ranges {
            if !allowed.contains(&(first, last)) || !seen.insert((first, last)) {
                return Err(StoreError::Invalid(format!("segment {device}/{first}-{last} was not offered for deletion")));
            }
        }
        let tx = self.conn.transaction()?;
        for &(first, last) in ranges {
            tx.execute(
                "DELETE FROM pending_segments WHERE vault_id = ?1 AND device_id = ?2 AND first_seq = ?3 AND last_seq = ?4",
                params![vault, device, first, last],
            )?;
        }
        tx.commit()?;
        Ok(ranges.len())
    }

    /// Moves the own ops of a sealed range `from` → `to`. Returns the count.
    fn transition(
        &mut self,
        vault: &str,
        device: &str,
        first: u64,
        from: UploadState,
        to: UploadState,
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let last: Option<u64> = tx
            .query_row(
                "SELECT last_seq FROM pending_segments WHERE vault_id = ?1 AND device_id = ?2 AND first_seq = ?3",
                params![vault, device, first],
                |r| r.get(0),
            )
            .optional()?;
        let last = last.ok_or_else(|| StoreError::Invalid(format!("segment {device}/{first} was never sealed")))?;
        let changed = tx.execute(
            "UPDATE ops SET upload_state = ?5 WHERE vault_id = ?1 AND upload_state = ?6 AND op_id IN
                 (SELECT op_id FROM op_dots WHERE vault_id = ?1 AND device_id = ?2 AND seq BETWEEN ?3 AND ?4)",
            params![vault, device, first, last, upload_state_text(to), upload_state_text(from)],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Commit `attempted` before sending (§4.2.3). Requires sealed bytes.
    pub(crate) fn mark_attempted(&mut self, vault: &str, device: &str, first: u64) -> Result<usize, StoreError> {
        self.transition(vault, device, first, UploadState::Never, UploadState::Attempted)
    }

    pub(crate) fn mark_published(&mut self, vault: &str, device: &str, first: u64) -> Result<usize, StoreError> {
        self.transition(vault, device, first, UploadState::Attempted, UploadState::Published)
    }

    /// Only after the provider confirmed the write did not happen (R3).
    pub(crate) fn mark_rejected(&mut self, vault: &str, device: &str, first: u64) -> Result<usize, StoreError> {
        self.transition(vault, device, first, UploadState::Attempted, UploadState::Rejected)
    }

    /// R2 inputs: every own op still held under the current device id, with
    /// the caller's verified remote body hashes per seq.
    pub(crate) fn rotation_input(
        &self,
        vault: &str,
        remote: BTreeMap<u64, Vec<[u8; 32]>>,
    ) -> Result<RotationSource, StoreError> {
        let device = identity_row(&self.conn, vault)?.device_id;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT d.seq, {OP_COLS}, o.upload_state, o.content_hash, o.body_hash FROM op_dots d
             JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
             WHERE d.vault_id = ?1 AND d.device_id = ?2 ORDER BY d.seq"
        ))?;
        let rows = stmt
            .query_map(params![vault, device], |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    RawOp::read(r, 1)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, Vec<u8>>(12)?,
                    r.get::<_, Vec<u8>>(13)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut ops = Vec::with_capacity(rows.len());
        for (seq, raw, state, ch, bh) in rows {
            let body = raw.body()?;
            ops.push(OwnOp {
                op_id: body.op_id.clone(),
                origin_device: device.clone(),
                seq,
                entity_kind: body.kind.as_str().to_owned(),
                entity_id: body.entity_id.clone(),
                op_type: body.op_type.clone(),
                payload: canonical_json(&body.payload)?,
                refs: refs(&body).iter().map(|s| (*s).to_owned()).collect(),
                hlc_wall: body.hlc.wall_ms,
                hlc_ctr: body.hlc.ctr,
                ctx: body.ctx.clone(),
                obs: body.obs.iter().cloned().collect(),
                upload_state: parse_upload_state(&state)?,
                content_hash: h32(ch)?,
                body_hash: h32(bh)?,
            });
        }
        Ok(RotationSource { old_device: device, ops, remote, local_compacted: BTreeMap::new() })
    }

    /// Runs the pure planner with the store's §4.3 hashing.
    pub(crate) fn plan_rotation(
        source: &RotationSource,
        new_device: &str,
        new_op_id: impl FnMut() -> String,
    ) -> Result<RotationPlan, StoreError> {
        let failure: RefCell<Option<StoreError>> = RefCell::new(None);
        let plan = rotation::plan_rotation(&source.input(new_device), new_op_id, |op| {
            match own_op_body(op).and_then(|body| hashes_of(&body)) {
                Ok(hashes) => hashes,
                Err(e) => {
                    failure.borrow_mut().get_or_insert(e);
                    OpHashes { content: [0; 32], body: [0; 32] }
                }
            }
        })
        .map_err(StoreError::Rotation)?;
        match failure.into_inner() {
            Some(e) => Err(e),
            None => Ok(plan),
        }
    }

    /// R5–R7 in one transaction (AC 16b/16c, 17). The keychain must already
    /// name    /// name `new_identity` (R4); a crash before COMMIT leaves nothing applied
    /// and the next cycle rotates again (I5).
    pub(crate) fn apply_rotation(
        &mut self,
        vault: &str,
        plan: &RotationPlan,
        remote: &BTreeMap<u64, Vec<[u8; 32]>>,
        new_identity: &DeviceIdentity,
    ) -> Result<[u8; 32], StoreError> {
        let tx = self.conn.transaction()?;
        let hash = rotation_in(&tx, vault, plan, remote, new_identity)?;
        tx.commit()?;
        Ok(hash)
    }

    /// Crash simulation: every R5–R7 write runs, then the transaction is
    /// dropped before COMMIT.
    #[cfg(test)]
    pub(crate) fn apply_rotation_then_crash(
        &mut self,
        vault: &str,
        plan: &RotationPlan,
        remote: &BTreeMap<u64, Vec<[u8; 32]>>,
        new_identity: &DeviceIdentity,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        rotation_in(&tx, vault, plan, remote, new_identity)?;
        drop(tx);
        Ok(())
    }

    // G7 engine helpers (additive; the sync engine's needs).

    /// Commits the generation the keychain already holds (§4.2.1: keychain
    /// first, then SQLite), after a seal or when `detect` adopts it.
    pub(crate) fn set_generation(&mut self, vault: &str, generation: u64) -> Result<(), StoreError> {
        let changed = self.conn.execute(
            "UPDATE vault_identity SET generation = ?2 WHERE vault_id = ?1",
            params![vault, generation],
        )?;
        if changed == 0 {
            return Err(StoreError::NoIdentity);
        }
        Ok(())
    }

    /// A quarantine row for a remote object the engine judged unreadable.
    pub(crate) fn quarantine_key(&mut self, vault: &str, key: &str, reason: &str) -> Result<(), StoreError> {
        put_quarantine(&self.conn, vault, key, reason)
    }

    /// Persists what the provider answered for a created object (the Drive
    /// file id lives in `provider_ref`), so a later resolve can name it.
    pub(crate) fn record_remote_object(
        &mut self,
        vault: &str,
        key: &str,
        provider_ref: &str,
        size: Option<u64>,
        etag: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO remote_index(vault_id, key, provider_ref, size, etag, content_hash, seen_at)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL, ?6)
             ON CONFLICT(vault_id, key) DO UPDATE SET provider_ref = excluded.provider_ref,
                 size = excluded.size, etag = excluded.etag, seen_at = excluded.seen_at",
            params![vault, key, provider_ref, size, etag, now_ms()],
        )?;
        Ok(())
    }

    /// The `provider_ref` recorded for `key`, if any.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn remote_object_ref(&self, vault: &str, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT provider_ref FROM remote_index WHERE vault_id = ?1 AND key = ?2",
                params![vault, key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Own sealed segments of `device` with the upload state of their ops
    /// (one transition moves a whole range, so the first dot speaks for it).
    pub(crate) fn segment_states(&self, vault: &str, device: &str) -> Result<Vec<(u64, u64, UploadState)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT p.first_seq, p.last_seq, o.upload_state FROM pending_segments p
             JOIN op_dots d ON d.vault_id = p.vault_id AND d.device_id = p.device_id AND d.seq = p.first_seq
             JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
             WHERE p.vault_id = ?1 AND p.device_id = ?2 ORDER BY p.first_seq",
        )?;
        let rows = stmt.query_map(params![vault, device], |r| {
            Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (first, last, state) = row?;
            out.push((first, last, parse_upload_state(&state)?));
        }
        Ok(out)
    }

    /// Our own upload landed next to another writer's valid segment under
    /// the same name (Drive, `PublishedWithFork`): record `forks(device,
    /// min(.., seq))` and re-project in one transaction. Returns whether the
    /// fork is new or moved earlier.
    pub(crate) fn record_own_fork(&mut self, vault: &str, device: &str, seq: u64) -> Result<bool, StoreError> {
        let tx = self.conn.transaction()?;
        let moved = upsert_fork(&tx, vault, device, seq)?;
        reproject_conn(&tx, vault)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Snapshot catch-up (§4.5): the reader's cursor is behind a compacted
    /// stream. One transaction: every op at every dot (alias / quarantine /
    /// fork exactly as for segments), the snapshot's forks, then each other
    /// device's cursor moves up to the snapshot frontier. An `ok` cursor
    /// moves and keeps its state; a `retired` cursor (our own retired id,
    /// G10: the clone compacted the shared stream before we rotated) moves
    /// and stays retired. Streams that are held, forked, or forked in the
    /// snapshot are never moved (they are relisted instead), and our own
    /// current id is never a cursor. A moved cursor has no chain head: the
    /// snapshot carries none, so the next segment of that stream is accepted
    /// on seq continuity alone.
    /// An invalid op quarantines the snapshot (`snap_key`) and writes nothing.
    pub(crate) fn apply_snapshot(
        &mut self,
        vault: &str,
        snapshot: &super::compaction::Snapshot,
        snap_key: &str,
    ) -> Result<ApplyOutcome, StoreError> {
        if snapshot.vault_id != vault {
            return self.quarantine_only(vault, snap_key, "snapshot names another vault");
        }
        for op in &snapshot.ops {
            let bad_dot = op.dots.is_empty() || op.dots.iter().any(|d| d.seq == 0 || d.seq > MAX_STORED);
            if let Err(e) = op.body.validate() {
                return self.quarantine_only(vault, snap_key, &format!("snapshot op: {e}"));
            }
            if bad_dot {
                return self.quarantine_only(vault, snap_key, "snapshot dot out of range");
            }
        }
        if snapshot.frontier.values().chain(snapshot.forks.values()).any(|s| *s > MAX_STORED) {
            return self.quarantine_only(vault, snap_key, "snapshot seq out of range");
        }
        let tx = self.conn.transaction()?;
        // A J2 joiner has no identity in the target yet (D4): no own stream.
        let own = match identity_row(&tx, vault) {
            Ok(row) => Some(row.device_id),
            Err(StoreError::NoIdentity) => None,
            Err(e) => return Err(e),
        };
        let mut conflicts = 0;
        let mut fork_started = false;
        for op in &snapshot.ops {
            for dot in &op.dots {
                let (conflict, fork) = store_remote_op(&tx, vault, &op.body, dot)?;
                if conflict {
                    conflicts += 1;
                    put_quarantine(&tx, vault, &format!("op/{}", op.body.op_id), "same op id, different content")?;
                }
                fork_started |= fork;
            }
        }
        for (device, start) in &snapshot.forks {
            fork_started |= upsert_fork(&tx, vault, device, *start)?;
        }
        for (device, seq) in &snapshot.frontier {
            if own.as_deref() == Some(device.as_str()) || snapshot.forks.contains_key(device) {
                continue;
            }
            let advance = match read_cursor(&tx, vault, device)? {
                None => Some(CursorState::Ok),
                Some(c) if c.applied_seq >= *seq => None,
                Some(c) if matches!(c.state, CursorState::Ok | CursorState::Retired) => Some(c.state),
                Some(_) => None,
            };
            if let Some(state) = advance {
                let next = Cursor { applied_seq: *seq, chain_head: None, state };
                put_cursor(&tx, vault, device, &next)?;
            }
        }
        let projection = reproject_conn(&tx, vault)?;
        tx.commit()?;
        Ok(ApplyOutcome::Applied { hash: projection.hash, fork_started, conflicts })
    }

    /// Every durable row except quarantine and wall-clock columns, in a
    /// stable order, for byte-for-byte state comparisons.
    #[cfg(test)]
    pub(crate) fn dump_state(&self) -> Result<String, StoreError> {
        const QUERIES: &[&str] = &[
            "SELECT * FROM meta ORDER BY 1",
            "SELECT * FROM entities ORDER BY 1, 2",
            "SELECT * FROM msg_tree ORDER BY 2",
            "SELECT * FROM vault_identity ORDER BY 1",
            "SELECT * FROM ops ORDER BY 1, 2",
            "SELECT * FROM op_dots ORDER BY 1, 2, 3, 4",
            "SELECT vault_id, device_id, fork_start FROM forks ORDER BY 1, 2",
            "SELECT * FROM memo_lineage ORDER BY 1, 3",
            "SELECT * FROM cursors ORDER BY 1, 2",
            "SELECT * FROM chain_nodes ORDER BY 1, 2, 3",
            "SELECT vault_id, device_id, reason, verified_prefix_S FROM retired_devices ORDER BY 1, 2",
            "SELECT vault_id, device_id, first_seq, last_seq FROM pending_segments ORDER BY 1, 2, 3",
            "SELECT * FROM sync_stats ORDER BY 1",
        ];
        let mut out = String::new();
        for sql in QUERIES {
            out.push_str(sql);
            out.push('\n');
            let mut stmt = self.conn.prepare(sql)?;
            let n = stmt.column_count();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                for i in 0..n {
                    out.push_str(&format!("{:?}|", row.get_ref(i)?));
                }
                out.push('\n');
            }
        }
        Ok(out)
    }
}

/// Largest canonical plaintext one sealed own segment carries (A4, D10).
/// The envelope adds 41 bytes, so a sealed segment stays far below
/// `provider::MAX_OBJECT_BYTES`. A single op larger than this still gets a
/// segment of its own (the engine refuses it as `SegmentTooLarge`).
pub(crate) const SEGMENT_PLAIN_CAP: usize = 8 << 20;

/// Remote state of a `vault_blob_map` row (§4.2.3 blob rule, D5).
pub(crate) const BLOB_PENDING: &str = "pending";
pub(crate) const BLOB_PUBLISHED: &str = "published";
/// Missing, over the cap, or unverifiable on download: the attachment stays
/// evicted (no bytes) and is never fetched again.
pub(crate) const BLOB_EVICTED: &str = "evicted";
/// An own attachment too large to upload as one object (D5b): never sent;
/// readers see it evicted; it blocks no segment.
pub(crate) const BLOB_SKIPPED_OVERSIZE: &str = "skipped_oversize";

/// Blob ids a `msg.add` payload references (`attachments[].blob_id`).
fn payload_blob_ids(payload: &[u8], out: &mut BTreeSet<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(payload) else { return };
    let Some(attachments) = value.get("attachments").and_then(Value::as_array) else { return };
    for attachment in attachments {
        if let Some(id) = attachment.get("blob_id").and_then(Value::as_str) {
            out.insert(id.to_owned());
        }
    }
}

impl Store {
    /// [`Store::seal_pending`] split so that no segment's canonical plaintext
    /// exceeds `cap` (A4): consecutive segments, each chained to the one
    /// before, all persisted in one transaction before any put. `seal` is
    /// called once per segment. `now` stamps `created_at`, the 30-day
    /// compaction trigger: the engine passes its cycle clock so the trigger
    /// and the stamp agree. Returns the sealed segments (empty when nothing
    /// is unsealed).
    pub(crate) fn seal_pending_capped(
        &mut self,
        vault: &str,
        cap: usize,
        now: u64,
        mut seal: impl FnMut(&Segment, &[u8]) -> Result<Vec<u8>, CryptoError>,
    ) -> Result<Vec<PendingSegment>, StoreError> {
        let tx = self.conn.transaction()?;
        let id = identity_row(&tx, vault)?;
        let rows = {
            let mut stmt = tx.prepare(&format!(
                "SELECT d.seq, {OP_COLS} FROM op_dots d JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
                 WHERE d.vault_id = ?1 AND d.device_id = ?2 AND d.seq > ?3 ORDER BY d.seq"
            ))?;
            let rows = stmt.query_map(params![vault, id.device_id, id.sealed_seq], |r| {
                Ok((r.get::<_, u64>(0)?, RawOp::read(r, 1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut ops = Vec::with_capacity(rows.len());
        for ((seq, raw), want) in rows.into_iter().zip(id.sealed_seq + 1..) {
            if seq != want {
                return Err(StoreError::Invalid(format!("own seq {want} is missing")));
            }
            let mut body = raw.body()?;
            body.origin_device = id.device_id.clone();
            let bh = body_hash(&body)?;
            tx.execute(
                "UPDATE ops SET body_hash = ?3 WHERE vault_id = ?1 AND op_id = ?2",
                params![vault, body.op_id, bh.to_vec()],
            )?;
            // The op's share of the segment plaintext (its canonical JSON
            // plus a separator); the envelope of the segment is added below.
            let op = SegOp { seq, body };
            let size = canonical_json(&serde_json::to_value(&op).map_err(corrupt)?)?.len() + 1;
            ops.push((op, size));
        }
        let mut out = Vec::new();
        let mut prev = id.chain_head;
        let mut rest = ops.into_iter().peekable();
        while rest.peek().is_some() {
            // Header overhead (device id, seqs, prev hash) is well under 1 KiB.
            let header = 512 + id.device_id.len();
            let mut chunk = Vec::new();
            let mut used = header;
            while let Some((_, size)) = rest.peek() {
                if !chunk.is_empty() && used.saturating_add(*size) > cap {
                    break;
                }
                let (op, size) = rest.next().expect("peeked");
                used = used.saturating_add(size);
                chunk.push(op);
            }
            let first = chunk[0].seq;
            let last = chunk[chunk.len() - 1].seq;
            let segment = Segment {
                v: FORMAT_V,
                device_id: id.device_id.clone(),
                first_seq: first,
                last_seq: last,
                prev_seg_hash: hex(&prev),
                ops: chunk,
            };
            let plain = segment.encode()?;
            let bytes = seal(&segment, &plain).map_err(|e| StoreError::Invalid(format!("seal failed: {e:?}")))?;
            tx.execute(
                "INSERT INTO pending_segments(vault_id, device_id, first_seq, last_seq, bytes, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![vault, id.device_id, first, last, bytes, now],
            )?;
            prev = segment_hash(&segment)?;
            tx.execute(
                "UPDATE vault_identity SET sealed_seq = ?2, chain_head = ?3 WHERE vault_id = ?1",
                params![vault, last, prev.to_vec()],
            )?;
            out.push(PendingSegment { device_id: id.device_id.clone(), first_seq: first, last_seq: last, bytes });
        }
        tx.commit()?;
        Ok(out)
    }

    /// `(blob_id, bytes)` of every blob of `vault` still to upload, oldest
    /// id order. Rows whose bytes are not stored here are skipped.
    pub(crate) fn pending_blobs(&self, vault: &str) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT m.blob_id, b.bytes FROM vault_blob_map m JOIN blobs b ON b.blob_id = COALESCE(m.local_blob, m.blob_id)
             WHERE m.vault_id = ?1 AND m.remote_state = ?2 AND b.bytes IS NOT NULL ORDER BY m.blob_id",
        )?;
        let rows = stmt.query_map(params![vault, BLOB_PENDING], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Ids of the blobs [`Store::pending_blobs`] lists (no bytes loaded):
    /// what still has to go up before a segment referencing it (D5a).
    pub(crate) fn pending_blob_ids(&self, vault: &str) -> Result<BTreeSet<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT m.blob_id FROM vault_blob_map m JOIN blobs b ON b.blob_id = COALESCE(m.local_blob, m.blob_id)
             WHERE m.vault_id = ?1 AND m.remote_state = ?2 AND b.bytes IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![vault, BLOB_PENDING], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<BTreeSet<_>, _>>()?)
    }

    /// Blob ids the `msg.add` ops at seqs `first..=last` of `device`
    /// reference: what the sealed segment carrying them depends on (D5a).
    pub(crate) fn segment_blob_ids(
        &self,
        vault: &str,
        device: &str,
        first: u64,
        last: u64,
    ) -> Result<BTreeSet<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT o.payload FROM op_dots d JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
             WHERE d.vault_id = ?1 AND d.device_id = ?2 AND d.seq BETWEEN ?3 AND ?4 AND o.op_type = 'msg.add'",
        )?;
        let payloads = stmt
            .query_map(params![vault, device, first, last], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut ids = BTreeSet::new();
        for payload in payloads {
            payload_blob_ids(&payload, &mut ids);
        }
        Ok(ids)
    }

    /// Sets the remote state of a blob of `vault` (inserting the map row).
    pub(crate) fn set_blob_state(&mut self, vault: &str, blob_id: &str, state: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO vault_blob_map(vault_id, blob_id, local_blob, remote_state) VALUES(?1, ?2, NULL, ?3)
             ON CONFLICT(vault_id, blob_id) DO UPDATE SET remote_state = excluded.remote_state",
            params![vault, blob_id, state],
        )?;
        Ok(())
    }

    /// A blob downloaded and verified (HMAC == id): stored and marked
    /// published for `vault`.
    pub(crate) fn store_remote_blob(&mut self, vault: &str, blob_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let sha: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(bytes).into();
        tx.execute(
            "INSERT INTO blobs(blob_id, mime, bytes, sha256, local_path) VALUES(?1, NULL, ?2, ?3, NULL)
             ON CONFLICT(blob_id) DO UPDATE SET bytes = excluded.bytes, sha256 = excluded.sha256
             WHERE blobs.bytes IS NULL",
            params![blob_id, bytes, sha.to_vec()],
        )?;
        tx.execute(
            "INSERT INTO vault_blob_map(vault_id, blob_id, local_blob, remote_state) VALUES(?1, ?2, ?2, ?3)
             ON CONFLICT(vault_id, blob_id) DO UPDATE SET local_blob = excluded.local_blob, remote_state = excluded.remote_state",
            params![vault, blob_id, BLOB_PUBLISHED],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Blob ids that `msg.add` ops of `vault` reference but that are neither
    /// stored locally nor already settled (`published` / `evicted` /
    /// `skipped_oversize`) for the vault: what catch-up should download.
    pub(crate) fn missing_blobs(&self, vault: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT payload FROM ops WHERE vault_id = ?1 AND op_type = 'msg.add'")?;
        let payloads = stmt.query_map(params![vault], |r| r.get::<_, Vec<u8>>(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut ids = BTreeSet::new();
        for payload in payloads {
            payload_blob_ids(&payload, &mut ids);
        }
        let mut out = Vec::new();
        for id in ids {
            let stored: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE blob_id = ?1 AND bytes IS NOT NULL)",
                params![id],
                |r| r.get(0),
            )?;
            let state: Option<String> = self
                .conn
                .query_row(
                    "SELECT remote_state FROM vault_blob_map WHERE vault_id = ?1 AND blob_id = ?2",
                    params![vault, id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let settled = matches!(
                state.as_deref(),
                Some(BLOB_EVICTED) | Some(BLOB_PUBLISHED) | Some(BLOB_SKIPPED_OVERSIZE)
            );
            if !stored && !settled {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Remote state of a blob of `vault`.
    #[cfg_attr(not(test), allow(dead_code))] // tests read it; the engine writes it
    pub(crate) fn blob_state(&self, vault: &str, blob_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT remote_state FROM vault_blob_map WHERE vault_id = ?1 AND blob_id = ?2",
                params![vault, blob_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }
}

/// Steps 2–9 of §4.5 for one segment, inside the caller's transaction.
fn apply_in(conn: &Connection, vault: &str, device: &str, segment: &Segment) -> Result<Step, StoreError> {
    let cursor = read_cursor(conn, vault, device)?.unwrap_or_else(|| Cursor::genesis(vault, device));
    match cursor.state {
        CursorState::Held => return Ok(Step::Done(ApplyOutcome::Skipped(cursor.state))),
        CursorState::Forked => return forked_in(conn, vault, device, segment, &cursor),
        CursorState::Ok | CursorState::Retired => {}
    }
    let (first, last) = (segment.first_seq, segment.last_seq);
    let retired = cursor.state == CursorState::Retired;
    if last <= cursor.applied_seq && !retired {
        return Ok(Step::Done(ApplyOutcome::Duplicate));
    }
    // A retired stream is relisted in full (engine `segment_wanted`), so a
    // range it already covers comes back every cycle. It is a no-op only
    // when every op is already stored at exactly that dot and applying it
    // again would change nothing; any other content at the range still
    // goes through the chain check below.
    if retired && last <= cursor.applied_seq && already_applied(conn, vault, device, segment)? {
        return Ok(Step::Done(ApplyOutcome::Duplicate));
    }
    if first > cursor.applied_seq.saturating_add(1) || (!retired && first != cursor.applied_seq + 1) {
        let reason = format!("gap: expected seq {}, got {first}", cursor.applied_seq.saturating_add(1));
        return Ok(Step::Hold { reason, quarantine: false });
    }
    if let Some(head) = cursor.chain_head {
        if segment.prev_seg_hash != hex(&head) {
            return Ok(Step::Hold { reason: "chain: prev_seg_hash mismatch".to_owned(), quarantine: true });
        }
    }
    if let Some(step) = schema_step(segment, device) {
        return Ok(step);
    }
    let hash = match incoming_hash(segment) {
        Ok(hash) => hash,
        Err(step) => return Ok(step),
    };

    let (conflicts, fork_started) = store_ops(conn, vault, device, &[segment])?;
    let projection = reproject_conn(conn, vault)?;
    if last >= cursor.applied_seq {
        let next = Cursor { applied_seq: last, chain_head: Some(hash), state: cursor.state };
        put_cursor(conn, vault, device, &next)?;
    }
    Ok(Step::Done(ApplyOutcome::Applied { hash: projection.hash, fork_started, conflicts }))
}

/// A segment of a forked stream (§4.2.3 (a), §4.4 projection = f(op set,
/// forks)). Writes made before the fork was noticed continue one of the
/// variant chains: a segment whose `prev_seg_hash` is any known head applies,
/// so every reader converges whichever variant it saw first. A segment that
/// continues no known head is list lag (its parent on some variant chain is
/// not listed yet), not a break: it is deferred with nothing written and is
/// retried when the engine relists the forked stream next cycle.
fn forked_in(conn: &Connection, vault: &str, device: &str, segment: &Segment, cursor: &Cursor) -> Result<Step, StoreError> {
    let hash = match incoming_hash(segment) {
        Ok(hash) => hash,
        Err(step) => return Ok(step),
    };
    if let Some(step) = unstorable_seq(segment) {
        return Ok(step);
    }
    let heads = known_heads(conn, vault, device, cursor)?;
    if heads.contains_key(&hash) || all_dots_held(conn, vault, device, segment)? {
        return Ok(Step::Done(ApplyOutcome::Duplicate));
    }
    let (first, last) = (segment.first_seq, segment.last_seq);
    match heads.iter().find(|(head, _)| segment.prev_seg_hash == hex(*head)) {
        Some((_, &head_last)) if head_last.checked_add(1) == Some(first) => {}
        Some(_) => {
            return Ok(Step::Hold { reason: "chain: segment does not follow its head".to_owned(), quarantine: true });
        }
        None => {
            let reason = "chain: prev_seg_hash continues no known head yet".to_owned();
            return Ok(Step::Done(ApplyOutcome::Deferred { reason }));
        }
    }
    if let Some(step) = schema_step(segment, device) {
        return Ok(step);
    }
    let (conflicts, fork_started) = store_ops(conn, vault, device, &[segment])?;
    put_node(conn, vault, device, &hash, last)?;
    let projection = reproject_conn(conn, vault)?;
    let next = Cursor { applied_seq: cursor.applied_seq.max(last), chain_head: None, state: CursorState::Forked };
    put_cursor(conn, vault, device, &next)?;
    Ok(Step::Done(ApplyOutcome::Applied { hash: projection.hash, fork_started, conflicts }))
}

/// §4.5 steps 4–6 for every op of `segments`: alias or insert, then the dot
/// (a second op id at a dot records a fork). A same-op-id/different-content
/// op is quarantined alone. Returns (conflicts, fork_started).
fn store_ops(conn: &Connection, vault: &str, device: &str, segments: &[&Segment]) -> Result<(usize, bool), StoreError> {
    let mut conflicts = 0;
    let mut fork_started = false;
    for segment in segments {
        for op in &segment.ops {
            let dot = Dot { device: device.to_owned(), seq: op.seq };
            let (conflict, fork) = store_remote_op(conn, vault, &op.body, &dot)?;
            if conflict {
                conflicts += 1;
                put_quarantine(conn, vault, &format!("op/{}", op.body.op_id), "same op id, different content")?;
            }
            fork_started |= fork;
        }
    }
    Ok((conflicts, fork_started))
}

/// Schema check (§4.5 step 3) plus the stream-origin rule: every op in
/// `log/<d>/` was written by `d`.
fn schema_step(segment: &Segment, device: &str) -> Option<Step> {
    if let Err(e) = segment.validate() {
        return Some(match e {
            OpError::FutureVersion(v) => Step::Future(v),
            e => Step::Hold { reason: e.to_string(), quarantine: true },
        });
    }
    segment.ops.iter().find(|op| op.body.origin_device != device).map(|op| Step::Hold {
        reason: format!("op {} has origin {} in stream {device}", op.body.op_id, op.body.origin_device),
        quarantine: true,
    })
}

fn dot_holds(conn: &Connection, vault: &str, device: &str, seq: u64, op_id: &str) -> Result<bool, StoreError> {
    let n: u64 = conn.query_row(
        "SELECT COUNT(*) FROM op_dots WHERE vault_id = ?1 AND device_id = ?2 AND seq = ?3 AND op_id = ?4",
        params![vault, device, seq, op_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every op of the segment is already stored at its dot.
fn all_dots_held(conn: &Connection, vault: &str, device: &str, segment: &Segment) -> Result<bool, StoreError> {
    for op in &segment.ops {
        if !dot_holds(conn, vault, device, op.seq, &op.body.op_id)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every op of the segment is stored at its dot with the same content hash,
/// and its ctx, obs, and origin are already absorbed into the stored op (I6
/// alias union), so storing it again would change nothing. Body hashes are
/// not compared: an aliased op keeps the body hash of its first-seen copy.
fn already_applied(conn: &Connection, vault: &str, device: &str, segment: &Segment) -> Result<bool, StoreError> {
    if segment.ops.is_empty() {
        return Ok(false);
    }
    for op in &segment.ops {
        if !dot_holds(conn, vault, device, op.seq, &op.body.op_id)? {
            return Ok(false);
        }
        let Some((stored, stored_content)) = read_op(conn, vault, &op.body.op_id)? else { return Ok(false) };
        let absorbed = op.body.ctx.iter().all(|(d, s)| stored.ctx.get(d).is_some_and(|have| have >= s))
            && op.body.obs.iter().all(|o| stored.obs.contains(o))
            && op.body.origin_device >= stored.origin_device;
        if stored_content != hashes_of(&op.body)?.content || !absorbed {
            return Ok(false);
        }
    }
    Ok(true)
}

/// §4.2.3 (a) inside the caller's transaction; `variants` are distinct.
///
/// Each variant must attach to what this reader already knows of the
/// stream: it continues a known head, or it is already applied (one
/// variant arrived alone before the name showed two objects), or it shares
/// its `prev_seg_hash` with such a variant (a sibling at the same point).
/// That holds whichever variant a reader saw first, and also for a held
/// stream whose break was the other variant's chain (r02b), so a fork is
/// accepted from `held` too; every later segment is still checked. On a
/// forked stream the variants that attach apply even when a sibling does not.
fn fork_in(
    conn: &Connection,
    vault: &str,
    device: &str,
    first: u64,
    last: u64,
    variants: &[&Segment],
) -> Result<Step, StoreError> {
    if let Some(step) = variants.iter().find_map(|v| unstorable_seq(v)) {
        return Ok(step);
    }
    let cursor = read_cursor(conn, vault, device)?.unwrap_or_else(|| Cursor::genesis(vault, device));
    let heads = known_heads(conn, vault, device, &cursor)?;
    // After a rotation the retired id has no known head (module doc).
    let headless_retired = cursor.state == CursorState::Retired && heads.is_empty();
    let mut anchors: BTreeSet<&str> = BTreeSet::new();
    let mut applied: Vec<bool> = Vec::with_capacity(variants.len());
    for v in variants {
        let continues = heads
            .iter()
            .any(|(head, &head_last)| v.prev_seg_hash == hex(head) && head_last.checked_add(1) == Some(first))
            || (headless_retired && cursor.applied_seq.checked_add(1) == Some(first));
        let held = all_dots_held(conn, vault, device, v)?;
        if continues || held {
            anchors.insert(v.prev_seg_hash.as_str());
        }
        applied.push(held);
    }
    let attached: Vec<(&Segment, bool)> =
        variants.iter().copied().zip(applied).filter(|(v, _)| anchors.contains(v.prev_seg_hash.as_str())).collect();
    let attached_only: Vec<&Segment>;
    let variants: &[&Segment] = if attached.len() == variants.len() {
        variants
    } else if cursor.state == CursorState::Forked {
        // On a forked stream a variant whose parent is not listed yet is lag
        // (architect B1): it alone is deferred, never held, so the relist can
        // attach it. Every sibling that attaches applies now (red-team f3), so
        // a variant that never connects cannot block a chain that does. With
        // nothing new to apply the name stays deferred and writes nothing.
        if attached.iter().all(|&(_, held)| held) {
            let reason = "fork variants continue no known head yet".to_owned();
            return Ok(Step::Done(ApplyOutcome::Deferred { reason }));
        }
        attached_only = attached.into_iter().map(|(v, _)| v).collect();
        &attached_only
    } else {
        if first > cursor.applied_seq.saturating_add(1) {
            let reason = format!("gap: expected seq {}, got {first}", cursor.applied_seq.saturating_add(1));
            return Ok(Step::Hold { reason, quarantine: false });
        }
        return Ok(Step::Hold { reason: "fork variants miss the known chain".to_owned(), quarantine: true });
    };
    for v in variants {
        if let Some(step) = schema_step(v, device) {
            return Ok(step);
        }
    }

    let (conflicts, mut fork_started) = store_ops(conn, vault, device, variants)?;
    let start = (first..=last).zip(0..).find(|(_, i)| {
        variants.iter().map(|v| v.ops[*i].body.op_id.as_str()).collect::<BTreeSet<_>>().len() > 1
    });
    if let Some((seq, _)) = start {
        fork_started |= upsert_fork(conn, vault, device, seq)?;
    }
    // Every head stays known so later segments of any variant chain apply.
    if let Some(head) = cursor.chain_head {
        put_node(conn, vault, device, &head, cursor.applied_seq)?;
    }
    for v in variants {
        put_node(conn, vault, device, &segment_hash(v)?, last)?;
    }
    let projection = reproject_conn(conn, vault)?;
    let next = Cursor { applied_seq: cursor.applied_seq.max(last), chain_head: None, state: CursorState::Forked };
    put_cursor(conn, vault, device, &next)?;
    Ok(Step::Done(ApplyOutcome::Applied { hash: projection.hash, fork_started, conflicts }))
}

/// R5–R7 (with amendment A5) inside the caller's transaction.
fn rotation_in(
    conn: &Connection,
    vault: &str,
    plan: &RotationPlan,
    remote: &BTreeMap<u64, Vec<[u8; 32]>>,
    new_identity: &DeviceIdentity,
) -> Result<[u8; 32], StoreError> {
    let old = plan.retired.device_id.as_str();
    let new = new_identity.device_id.as_str();
    if identity_row(conn, vault)?.device_id != old {
        return Err(StoreError::Invalid("plan does not retire the current device id".to_owned()));
    }
    if new_identity.vault_id != vault || new == old {
        return Err(StoreError::Invalid("new identity must be another id in this vault".to_owned()));
    }
    let s = plan.verified_prefix;
    let source_of: BTreeMap<&str, &str> = plan.reissued.iter().map(|(o, n)| (n.as_str(), o.as_str())).collect();

    // R5 / R6: old dot out, rewritten op in under (D′, k).
    for rewrite in &plan.rewritten {
        let op = &rewrite.op;
        let reissued = source_of.get(op.op_id.as_str()).copied();
        let source_id = reissued.unwrap_or(op.op_id.as_str());
        let removed = conn.execute(
            "DELETE FROM op_dots WHERE vault_id = ?1 AND device_id = ?2 AND seq = ?3 AND op_id = ?4",
            params![vault, old, rewrite.source_seq, source_id],
        )?;
        if removed != 1 || op.origin_device != new {
            return Err(StoreError::Invalid(format!("rewrite of {old}/{} does not match the store", rewrite.source_seq)));
        }
        let body = own_op_body(op)?;
        body.validate()?;
        let hashes = hashes_of(&body)?;
        if hashes != (OpHashes { content: op.content_hash, body: op.body_hash }) {
            return Err(StoreError::Invalid(format!("plan hashes of {} are stale", op.op_id)));
        }
        if reissued.is_some() {
            insert_op_row(conn, vault, &body, hashes, UploadState::Never)?;
        } else {
            conn.execute(
                "UPDATE ops SET origin_device = ?3, ctx_frontier = ?4, obs = ?5, payload = ?6, content_hash = ?7,
                     body_hash = ?8, upload_state = 'never'
                 WHERE vault_id = ?1 AND op_id = ?2",
                params![
                    vault,
                    body.op_id,
                    new,
                    ctx_text(&body.ctx)?,
                    obs_text(&body.obs)?,
                    canonical_json(&body.payload)?,
                    hashes.content.to_vec(),
                    hashes.body.to_vec(),
                ],
            )?;
        }
        record_dot(conn, vault, &Dot { device: new.to_owned(), seq: op.seq }, &body.op_id)?;
    }
    for x in &plan.quarantined {
        let fresh = plan.reissued.get(&x.op_id).map(String::as_str).unwrap_or_default();
        let reason = format!("rejected at {old}/{}; reissued as {fresh}", x.seq);
        put_quarantine(conn, vault, &format!("op/{}", x.op_id), &reason)?;
    }
    for op_id in &plan.promoted {
        conn.execute(
            "UPDATE ops SET upload_state = 'published' WHERE vault_id = ?1 AND op_id = ?2",
            params![vault, op_id],
        )?;
    }

    // R7: retire D and read it like another device from S + 1.
    conn.execute(
        "INSERT OR REPLACE INTO retired_devices(vault_id, device_id, retired_at, reason, verified_prefix_S)
         VALUES(?1, ?2, ?3, 'rotation', ?4)",
        params![vault, old, now_ms(), s],
    )?;
    let kept: BTreeMap<u64, [u8; 32]> = {
        let mut stmt = conn.prepare(
            "SELECT d.seq, o.body_hash FROM op_dots d JOIN ops o ON o.vault_id = d.vault_id AND o.op_id = d.op_id
             WHERE d.vault_id = ?1 AND d.device_id = ?2 AND d.seq > ?3 AND o.upload_state = 'published'",
        )?;
        let rows = stmt.query_map(params![vault, old, s], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|(seq, h)| Ok((seq, h32(h)?))).collect::<Result<_, StoreError>>()?
    };
    // Amendment A5: a seq > S whose remote set is not exactly ours.
    let mut applied = plan.retired.applied_seq;
    let seqs: BTreeSet<u64> = kept.keys().copied().chain(remote.range(s + 1..).map(|(seq, _)| *seq)).collect();
    for seq in seqs {
        let theirs: BTreeSet<[u8; 32]> = remote.get(&seq).into_iter().flatten().copied().collect();
        let other_content = match kept.get(&seq) {
            Some(ours) if theirs.len() == 1 && theirs.contains(ours) => continue,
            Some(ours) => {
                applied = applied.min(seq - 1);
                theirs.iter().any(|h| h != ours)
            }
            None => theirs.len() > 1,
        };
        if other_content {
            upsert_fork(conn, vault, old, seq)?;
            applied = applied.min(seq - 1);
        }
    }
    put_cursor(conn, vault, old, &Cursor { applied_seq: applied, chain_head: None, state: CursorState::Retired })?;
    conn.execute(
        "UPDATE vault_identity SET device_id = ?2, generation = ?3, sealed_seq = 0, chain_head = ?4 WHERE vault_id = ?1",
        params![vault, new, new_identity.generation, genesis_chain_head(vault, new).to_vec()],
    )?;
    conn.execute(
        "INSERT INTO sync_stats(name, value) VALUES('rotation', 1)
         ON CONFLICT(name) DO UPDATE SET value = value + 1",
        [],
    )?;
    Ok(reproject_conn(conn, vault)?.hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::envelope::seal;
    use crate::sync::sim_tests::{
        create, device, edit, identity, local, oid, publish, remote_key, seal_next, seg_key, sync_stream, Remote,
        TempDir, EPOCH, K, M, VAULT,
    };

    #[test]
    fn open_sets_wal_and_foreign_keys_and_creates_every_table() {
        let dir = TempDir::new("wal");
        let store = Store::open(&dir.0.join("sync").join("sync.db")).unwrap();
        assert_eq!(store.journal_mode().unwrap(), "wal");
        let fk: i64 = store.conn().pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
        let version: i64 = store.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        for table in [
            "meta", "entities", "msg_tree", "blobs", "migration_queue", "checkpoints", "sync_stats",
            "vault_identity", "ops", "op_dots", "forks", "memo_lineage", "vault_blob_map", "cursors",
            "remote_index", "retired_devices", "quarantine", "pending_segments", "chain_nodes", "queue_op_hashes",
        ] {
            let n: i64 = store
                .conn()
                .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [table], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(n, 1, "{table}");
        }
        drop(store);
        // Reopening is a no-op migration.
        let store = Store::open(&dir.0.join("sync").join("sync.db")).unwrap();
        assert_eq!(store.journal_mode().unwrap(), "wal");
    }

    #[test]
    fn peek_meta_reads_without_creating_or_migrating() {
        let dir = TempDir::new("peek");
        let path = dir.0.join("sync").join("sync.db");
        assert_eq!(Store::peek_meta(&path, "migration_state").unwrap(), None);
        assert!(!dir.0.join("sync").exists(), "a missing store is not created");
        // An older schema (no queue_op_hashes yet) is read but not migrated.
        std::fs::create_dir_all(dir.0.join("sync")).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            meta_set_conn(&conn, "migration_state", "native").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert_eq!(Store::peek_meta(&path, "migration_state").unwrap().as_deref(), Some("native"));
        assert_eq!(Store::peek_meta(&path, "absent").unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), before, "the file is unchanged");
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 1, "not migrated");
        // A file without the meta table reads as nothing.
        let empty = dir.0.join("sync").join("empty.db");
        Connection::open(&empty).unwrap().execute_batch("CREATE TABLE other(x);").unwrap();
        assert_eq!(Store::peek_meta(&empty, "migration_state").unwrap(), None);
    }

    /// G10 A1: rows an older `project()` wrote are rewritten once on open
    /// (the incremental path diffs against them), and a matching version
    /// leaves the file alone.
    #[test]
    fn open_rewrites_the_projection_once_when_the_projection_version_changes() {
        let dir = TempDir::new("projection-version");
        let path = dir.0.join("sync").join("sync.db");
        {
            let mut store = Store::open(&path).unwrap();
            store.init_identity(&identity("dev-a")).unwrap();
            store.set_active_vault(VAULT).unwrap();
            local(&mut store, create(1, 1_000, M, "x"));
            assert_eq!(store.meta_get(META_PROJECTION_VERSION).unwrap().as_deref(), Some("1"));
            // Simulate rows an older build left behind: a stale entity row
            // and an older version stamp.
            store
                .conn()
                .execute("INSERT INTO entities(kind, id, state_json, deleted, evicted_local) VALUES('memo', 'stale', '{}', 0, 0)", [])
                .unwrap();
            store.conn().execute("UPDATE meta SET value = '0' WHERE key = ?1", [META_PROJECTION_VERSION]).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.meta_get(META_PROJECTION_VERSION).unwrap().as_deref(), Some("1"));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM entities WHERE id = 'stale'"), 0, "rewritten from the ops");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM entities"), 1);
        let after_heal = store.dump_state().unwrap();
        drop(store);
        // Same version: open leaves the rows exactly as they are.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.dump_state().unwrap(), after_heal);
    }

    #[test]
    fn insert_queue_batch_records_the_hash_with_the_meta_and_evicts_the_oldest() {
        let mut store = Store::open_in_memory().unwrap();
        store.init_identity(&identity("dev-a")).unwrap();
        let hash = |n: u8| [n; 32];
        let meta = |q: u64| vec![("wm".to_owned(), q.to_string())];
        for qseq in 1..=(QUEUE_HASH_WINDOW as u64 + 2) {
            store.insert_queue_batch(VAULT, qseq, &hash(qseq as u8), Vec::new(), Vec::new(), meta(qseq)).unwrap();
        }
        assert_eq!(store.meta_get("wm").unwrap().as_deref(), Some((QUEUE_HASH_WINDOW + 2).to_string().as_str()));
        assert_eq!(store.queue_op_hash(1).unwrap(), None);
        assert_eq!(store.queue_op_hash(2).unwrap(), None);
        assert_eq!(store.queue_op_hash(3).unwrap(), Some(hash(3)));
        assert_eq!(store.queue_hash_qseqs().unwrap().len(), QUEUE_HASH_WINDOW);
        // Out-of-range qseqs never match and are refused.
        assert_eq!(store.queue_op_hash(u64::MAX).unwrap(), None);
        assert!(store.insert_queue_batch(VAULT, u64::MAX, &hash(1), Vec::new(), Vec::new(), meta(1)).is_err());
        assert_eq!(store.meta_get("wm").unwrap().as_deref(), Some((QUEUE_HASH_WINDOW + 2).to_string().as_str()));
        // Without an identity nothing is written.
        assert!(store.insert_queue_batch("other-vault", 9_999, &hash(7), Vec::new(), Vec::new(), meta(9_999)).is_err());
        assert_eq!(store.queue_op_hash(9_999).unwrap(), None);
    }

    #[test]
    fn vaults_are_isolated() {
        let mut store = device("a");
        let other = DeviceIdentity { vault_id: "vault-2".to_owned(), device_id: "a-v2".to_owned(), ..identity("a") };
        store.init_identity(&other).unwrap();
        local(&mut store, create(1, 1_000, M, "in vault 1"));
        store.insert_local_op("vault-2", create(2, 2_000, M, "in vault 2"), UploadState::Never).unwrap();

        let p1 = store.projection(VAULT).unwrap();
        let p2 = store.projection("vault-2").unwrap();
        assert_ne!(p1.hash, p2.hash);
        assert_eq!(p1.entities.len(), 1);
        assert_eq!(store.dots_of(VAULT, &oid(2)).unwrap(), vec![]);
        assert_eq!(store.dots_of("vault-2", &oid(2)).unwrap(), vec![Dot { device: "a-v2".to_owned(), seq: 1 }]);
        assert_eq!(store.op_state(VAULT, &oid(2)).unwrap(), None);
        // The global entity table mirrors only the active vault.
        let text: String = store.conn().query_row("SELECT state_json FROM entities", [], |r| r.get(0)).unwrap();
        assert!(text.contains("in vault 1"), "{text}");
        store.reproject("vault-2").unwrap();
        let text: String = store.conn().query_row("SELECT state_json FROM entities", [], |r| r.get(0)).unwrap();
        assert!(text.contains("in vault 1"), "{text}");
    }

    #[test]
    fn local_ops_get_exactly_one_dot_and_the_previous_own_seq_as_ctx() {
        let mut store = device("a");
        let first = store.insert_local_op(VAULT, create(1, 1_000, M, "x"), UploadState::Never).unwrap();
        let second = store.insert_local_op(VAULT, edit(2, 2_000, M, 1, "y"), UploadState::Never).unwrap();
        assert_eq!(first.dot, Dot { device: "a".to_owned(), seq: 1 });
        assert!(first.body.ctx.is_empty());
        assert_eq!(second.dot.seq, 2);
        assert_eq!(second.body.ctx, BTreeMap::from([("a".to_owned(), 1)]));
        assert_eq!(second.body.origin_device, "a");
        for n in [1, 2] {
            assert_eq!(store.dots_of(VAULT, &oid(n)).unwrap().len(), 1);
            let (ch, bh, state) = store.op_state(VAULT, &oid(n)).unwrap().unwrap();
            let body = &store.load_set(VAULT).unwrap().0.get(&oid(n)).unwrap().body.clone();
            assert_eq!((ch, bh, state), (content_hash(body).unwrap(), body_hash(body).unwrap(), UploadState::Never));
        }
        assert_eq!(second.projection_hash, store.projection(VAULT).unwrap().hash);
        let entity: String = store.conn().query_row("SELECT state_json FROM entities", [], |r| r.get(0)).unwrap();
        assert!(entity.contains("\"text\":\"y\""));
        // A malformed op is refused and leaves nothing behind.
        let mut bad = create(3, 3_000, M, "z");
        bad.payload = serde_json::json!({"text": 1});
        assert!(store.insert_local_op(VAULT, bad, UploadState::Never).is_err());
        assert!(store.dots_of(VAULT, &oid(3)).unwrap().is_empty());
    }

    #[test]
    fn sealed_bytes_are_persisted_before_any_put_and_never_resealed() {
        let mut store = device("a");
        local(&mut store, create(1, 1_000, M, "x"));
        assert!(store.mark_attempted(VAULT, "a", 1).is_err(), "nothing sealed, nothing to attempt");
        let sealed = seal_next(&mut store).unwrap();
        assert_eq!((sealed.first_seq, sealed.last_seq), (1, 1));
        assert_eq!(store.mark_attempted(VAULT, "a", 1).unwrap(), 1);
        assert!(seal_next(&mut store).is_none(), "an attempted segment is never resealed");
        assert_eq!(store.sealed_bytes(VAULT, "a", 1).unwrap().unwrap().bytes, sealed.bytes);
        local(&mut store, edit(2, 2_000, M, 1, "y"));
        let next = seal_next(&mut store).unwrap();
        assert_eq!((next.first_seq, next.last_seq), (2, 2));
        assert_eq!(store.sealed_bytes(VAULT, "a", 1).unwrap().unwrap().bytes, sealed.bytes);
        assert_eq!(store.mark_published(VAULT, "a", 1).unwrap(), 1);
        assert_eq!(store.mark_published(VAULT, "a", 2).unwrap(), 0, "published only after attempted");
        let ids = store.local_identity(VAULT).unwrap().unwrap();
        assert_eq!(ids.sealed_seq, 2);
        assert_eq!(store.op_state(VAULT, &oid(1)).unwrap().unwrap().2, UploadState::Published);
        assert_eq!(store.op_state(VAULT, &oid(2)).unwrap().unwrap().2, UploadState::Never);
    }

    fn addr(device: &str, first: u64, last: u64) -> ObjectAddress {
        ObjectAddress::segment(VAULT, EPOCH, device, first, last)
    }

    fn genesis(device: &str) -> String {
        hex(&genesis_chain_head(VAULT, device))
    }

    /// A remote segment of `device` holding one memo create per seq.
    fn seg(device: &str, first: u64, prev: String, ns: &[u64]) -> Segment {
        let ops = ns
            .iter()
            .zip(first..)
            .map(|(&n, seq)| {
                let mut body = create(n, n * 1_000, &oid(500 + n), &format!("m{n}"));
                body.origin_device = device.to_owned();
                SegOp { seq, body }
            })
            .collect::<Vec<_>>();
        let last = first + ops.len() as u64 - 1;
        Segment { v: FORMAT_V, device_id: device.to_owned(), first_seq: first, last_seq: last, prev_seg_hash: prev, ops }
    }

    fn sealed(segment: &Segment) -> Vec<u8> {
        let address = addr(&segment.device_id, segment.first_seq, segment.last_seq);
        seal(&seg_key(), &address, &segment.encode().unwrap()).unwrap()
    }

    fn head(segment: &Segment) -> String {
        hex(&segment_hash(segment).unwrap())
    }

    fn state(store: &Store, device: &str) -> (CursorState, u64) {
        let c = store.cursor(VAULT, device).unwrap().unwrap();
        (c.state, c.applied_seq)
    }

    /// `dump_state` without the cursors section.
    fn without_cursors(store: &Store) -> String {
        let dump = store.dump_state().unwrap();
        let mut skipping = false;
        let mut out = String::new();
        for line in dump.lines() {
            if line.starts_with("SELECT ") {
                skipping = line.contains("FROM cursors");
            }
            if !skipping {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    fn has(store: &Store, n: u64) -> bool {
        !store.dots_of(VAULT, &oid(n)).unwrap().is_empty()
    }

    /// Architect B2: forget_segments drops the sealed bytes once the remote
    /// delete is confirmed, so compaction_due clears; it refuses ranges that
    /// were never sealed or are not fully published, all in one transaction.
    #[test]
    fn forgetting_compacted_segments_clears_the_compaction_trigger() {
        use crate::sync::compaction::{
            build_snapshot, compaction_due, segments_deletable, verify_snapshot, MAX_OWN_SEGMENTS,
        };
        let mut remote = Remote::new(true);
        let mut a = device("a");
        let n = MAX_OWN_SEGMENTS as u64 + 1;
        for i in 1..=n {
            local(&mut a, create(i, i, &oid(1_000 + i), "x"));
            assert!(publish(&mut a, &mut remote).unwrap().1);
        }
        assert!(compaction_due(&a, VAULT, now_ms()).unwrap(), "201 live segments trigger");
        let snapshot = build_snapshot(&a, VAULT).unwrap();
        assert!(verify_snapshot(&a, VAULT, &snapshot).unwrap());
        let deletable = segments_deletable(&a, VAULT, "a", &[n - 5], true, &snapshot).unwrap();
        assert_eq!(deletable.len() as u64, n - 5);

        // Not yet published, never sealed: refused, nothing forgotten.
        local(&mut a, create(9_000, 9_000, K, "unsent"));
        let unsent = seal_next(&mut a).unwrap();
        let before = a.dump_state().unwrap();
        let mixed = [deletable[0], (unsent.first_seq, unsent.last_seq)];
        assert!(a.forget_segments(VAULT, "a", &mixed).is_err(), "an unpublished range keeps its bytes");
        assert!(a.forget_segments(VAULT, "a", &[(1, 2)]).is_err(), "not a sealed range");
        assert_eq!(a.dump_state().unwrap(), before, "a refused call forgets nothing");

        assert_eq!(a.forget_segments(VAULT, "a", &deletable).unwrap(), deletable.len());
        let live = a.pending_segments(VAULT, "a").unwrap();
        assert_eq!(live.len(), 6, "only the undeleted segments remain");
        assert_eq!(live[0].0, n - 4);
        assert!(!compaction_due(&a, VAULT, now_ms()).unwrap(), "the trigger clears after a compaction");
        assert!(a.sealed_bytes(VAULT, "a", 1).unwrap().is_none());
        assert_eq!(a.sealed_bytes(VAULT, "a", unsent.first_seq).unwrap().unwrap().bytes, unsent.bytes);
        assert_eq!(a.dots_of(VAULT, &oid(1)).unwrap().len(), 1, "op rows and dots stay");
        // The 30-day trigger also only looks at live segments.
        assert!(compaction_due(&a, VAULT, now_ms() + crate::sync::compaction::MAX_SEGMENT_AGE_MS).unwrap());
        let rest: Vec<(u64, u64)> = live.iter().map(|(f, l, _)| (*f, *l)).collect();
        let published: Vec<(u64, u64)> = rest.into_iter().filter(|r| r.0 != unsent.first_seq).collect();
        assert!(a.forget_segments(VAULT, "a", &published).is_err(), "past the offered reader limit");
        let offer = |a: &Store| {
            let snapshot = build_snapshot(a, VAULT).unwrap();
            assert!(verify_snapshot(a, VAULT, &snapshot).unwrap());
            segments_deletable(a, VAULT, "a", &[u64::MAX], true, &snapshot).unwrap()
        };
        assert_eq!(offer(&a), published, "the unsent range is never offered");
        a.forget_segments(VAULT, "a", &published).unwrap();
        a.mark_attempted(VAULT, "a", unsent.first_seq).unwrap();
        assert!(offer(&a).is_empty(), "an attempted range is never offered");
        a.mark_published(VAULT, "a", unsent.first_seq).unwrap();
        assert_eq!(offer(&a), vec![(unsent.first_seq, unsent.last_seq)]);
        a.forget_segments(VAULT, "a", &[(unsent.first_seq, unsent.last_seq)]).unwrap();
        assert!(!compaction_due(&a, VAULT, now_ms() + crate::sync::compaction::MAX_SEGMENT_AGE_MS).unwrap());
    }

    /// Architect B2 / red-team p2_p2: segments_deletable and forget_segments
    /// share one predicate. A rejected range (its name is another writer's
    /// object) and an attempted range are never offered and never forgotten;
    /// a published range is, and forgetting it clears compaction_due. An
    /// offer goes stale when the state changes after it: a fork of the own
    /// stream, or a later unverified pass, makes forget refuse with
    /// pending_segments and compaction_due unchanged.
    #[test]
    fn only_offered_published_ranges_are_forgotten() {
        use crate::sync::compaction::{
            build_snapshot, compaction_due, segments_deletable, verify_snapshot, MAX_SEGMENT_AGE_MS,
        };
        let aged = now_ms() + MAX_SEGMENT_AGE_MS + 60_000;
        let offer = |a: &Store, device: &str| {
            let snapshot = build_snapshot(a, VAULT).unwrap();
            assert!(verify_snapshot(a, VAULT, &snapshot).unwrap());
            segments_deletable(a, VAULT, device, &[u64::MAX], true, &snapshot).unwrap()
        };
        // a: 1-1 published, 2-2 rejected (a clone took the name), 3-3 attempted.
        let mut remote = Remote::new(true);
        let mut a = device("a");
        local(&mut a, create(1, 1_000, M, "one"));
        publish(&mut a, &mut remote).unwrap();
        local(&mut a, create(2, 2_000, K, "two"));
        let taken = seal_next(&mut a).unwrap();
        a.mark_attempted(VAULT, "a", taken.first_seq).unwrap();
        a.mark_rejected(VAULT, "a", taken.first_seq).unwrap();
        local(&mut a, create(3, 3_000, "memo-3", "three"));
        let pending = seal_next(&mut a).unwrap();
        a.mark_attempted(VAULT, "a", pending.first_seq).unwrap();
        assert!(compaction_due(&a, VAULT, aged).unwrap());

        assert_eq!(offer(&a, "a"), vec![(1, 1)], "only the published range is offered");
        let before = a.dump_state().unwrap();
        for refused in [vec![(2, 2)], vec![(3, 3)], vec![(1, 1), (2, 2)], vec![(1, 1), (1, 1)]] {
            assert!(a.forget_segments(VAULT, "a", &refused).is_err(), "{refused:?}");
            assert_eq!(a.dump_state().unwrap(), before, "{refused:?}: nothing forgotten");
        }
        assert_eq!(a.forget_segments(VAULT, "a", &[(1, 1)]).unwrap(), 1);
        assert_eq!(a.pending_segments(VAULT, "a").unwrap().len(), 2, "rejected and attempted rows stay");
        a.mark_published(VAULT, "a", pending.first_seq).unwrap();
        assert_eq!(offer(&a, "a"), vec![(3, 3)], "the rejected range is still never offered");
        a.forget_segments(VAULT, "a", &[(3, 3)]).unwrap();
        assert!(compaction_due(&a, VAULT, aged).unwrap(), "the rejected row is still live");

        // Offered, then an unverified pass revokes the offer.
        let mut b = device("b");
        local(&mut b, create(1, 1_000, M, "one"));
        publish(&mut b, &mut remote).unwrap();
        let snapshot = build_snapshot(&b, VAULT).unwrap();
        assert_eq!(offer(&b, "b"), vec![(1, 1)]);
        assert!(segments_deletable(&b, VAULT, "b", &[u64::MAX], false, &snapshot).unwrap().is_empty());
        assert!(b.forget_segments(VAULT, "b", &[(1, 1)]).is_err(), "revoked by the unverified pass");
        assert!(compaction_due(&b, VAULT, aged).unwrap());

        // Offered, then the own stream forks before the trim: refused.
        let mut f = device("f");
        local(&mut f, create(1, 1_000, M, "mine"));
        let mine = seal_next(&mut f).unwrap();
        f.mark_attempted(VAULT, "f", 1).unwrap();
        f.mark_published(VAULT, "f", 1).unwrap();
        assert_eq!(offer(&f, "f"), vec![(1, 1)]);
        let mine = open_segment(&seg_key(), &addr("f", 1, 1), &mine.bytes).unwrap();
        let theirs = seg("f", 1, genesis("f"), &[9]);
        f.apply_fork_variant(VAULT, "f", 1, 1, &[mine, theirs]).unwrap();
        let before = f.dump_state().unwrap();
        let due = compaction_due(&f, VAULT, aged).unwrap();
        assert!(f.forget_segments(VAULT, "f", &[(1, 1)]).is_err(), "the offer predates the fork");
        assert_eq!((f.dump_state().unwrap(), compaction_due(&f, VAULT, aged).unwrap()), (before, due));
    }

    /// Architect G3 cleanup a: a range qualifies only when its published own
    /// op count is `last - first + 1`. A dot missing from a published range
    /// (one seq or all of them) never makes it vacuously all published.
    #[test]
    fn a_range_with_missing_dots_is_never_deletable() {
        let mut remote = Remote::new(true);
        let mut a = device("a");
        local(&mut a, create(1, 1_000, M, "one"));
        publish(&mut a, &mut remote).unwrap();
        local(&mut a, create(2, 2_000, K, "two"));
        local(&mut a, create(3, 3_000, "memo-3", "three"));
        publish(&mut a, &mut remote).unwrap();
        local(&mut a, create(4, 4_000, "memo-4", "four"));
        publish(&mut a, &mut remote).unwrap();
        assert_eq!(a.offer_deletable(VAULT, "a", u64::MAX).unwrap(), vec![(1, 1), (2, 3), (4, 4)]);

        let drop_dot = |a: &Store, seq: u64| {
            let n = a.conn().execute("DELETE FROM op_dots WHERE vault_id = ?1 AND device_id = 'a' AND seq = ?2", params![VAULT, seq]);
            assert_eq!(n.unwrap(), 1);
        };
        drop_dot(&a, 3);
        assert_eq!(a.offer_deletable(VAULT, "a", u64::MAX).unwrap(), vec![(1, 1), (4, 4)], "one of two dots missing");
        drop_dot(&a, 4);
        assert_eq!(a.offer_deletable(VAULT, "a", u64::MAX).unwrap(), vec![(1, 1)], "every dot missing");
        let before = a.dump_state().unwrap();
        for refused in [vec![(2, 3)], vec![(4, 4)]] {
            assert!(a.forget_segments(VAULT, "a", &refused).is_err(), "{refused:?}");
            assert_eq!(a.dump_state().unwrap(), before, "{refused:?}: nothing forgotten");
        }
        assert_eq!(a.forget_segments(VAULT, "a", &[(1, 1)]).unwrap(), 1);
    }

    /// Critic B1: a future envelope version byte and a future plaintext `v`
    /// (even one that is not v1-canonical, e.g. carries a float) hold the
    /// stream: FutureVersion, cursor held, no quarantine, state unchanged.
    #[test]
    fn future_versions_hold_the_stream_without_quarantine() {
        let key = seg_key();
        let s1 = seg("a", 1, genesis("a"), &[1]);
        let s2 = seg("a", 2, head(&s1), &[2]);
        let mut float = serde_json::to_value(&s2).unwrap();
        float["v"] = serde_json::json!(2);
        float["ops"][0]["body"]["payload"]["weight"] = serde_json::json!(0.5);
        let float = seal(&key, &addr("a", 2, 2), &serde_json::to_vec(&float).unwrap()).unwrap();
        let mut op_v = serde_json::to_value(&s2).unwrap();
        op_v["ops"][0]["body"]["v"] = serde_json::json!(3);
        let op_v = seal(&key, &addr("a", 2, 2), &canonical_json(&op_v).unwrap()).unwrap();
        let mut envelope = sealed(&s2);
        envelope[0] = 0x02;
        for (name, bytes, v) in [("envelope version", envelope, 2), ("plaintext v + float", float, 2), ("op v", op_v, 3)] {
            let mut r = device("r");
            assert!(matches!(r.apply_sealed(VAULT, &key, &addr("a", 1, 1), &sealed(&s1)).unwrap(), ApplyOutcome::Applied { .. }));
            let before = without_cursors(&r);
            let head_before = r.cursor(VAULT, "a").unwrap().unwrap().chain_head;
            let out = r.apply_sealed(VAULT, &key, &addr("a", 2, 2), &bytes).unwrap();
            assert_eq!(out, ApplyOutcome::FutureVersion(v), "{name}");
            assert_eq!(state(&r, "a"), (CursorState::Held, 1), "{name}");
            assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().chain_head, head_before, "{name}");
            assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{name}: not quarantined");
            assert_eq!(without_cursors(&r), before, "{name}: only the cursor state changed");
            assert!(!has(&r, 2), "{name}");
        }
        // An older envelope version byte is still corrupt (quarantine only).
        let mut r = device("r");
        let mut old = sealed(&s1);
        old[0] = 0x00;
        assert!(matches!(r.apply_sealed(VAULT, &key, &addr("a", 1, 1), &old).unwrap(), ApplyOutcome::Quarantined { .. }));
        assert_eq!(r.cursor(VAULT, "a").unwrap(), None);
    }

    /// Cleaner 1: faults are classified by cause. An incoming segment that
    /// fails decoding or validation (an integer past i64, a segment that does
    /// not encode) is held + quarantined; a damaged local row met while
    /// re-projecting is a local fault: `Err`, no quarantine, no hold, the
    /// stream and every other row untouched.
    #[test]
    fn remote_faults_hold_by_cause_and_local_faults_stay_errors() {
        // Incoming, rejected by validation: held + quarantined, nothing stored.
        let mut r = device("r");
        let mut s1 = seg("a", 1, genesis("a"), &[1]);
        s1.ops[0].body.hlc.wall_ms = i64::MAX as u64 + 1;
        let before = without_cursors(&r);
        let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &sealed(&s1)).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { ref reason } if reason.contains("i64")), "{out:?}");
        assert_eq!(state(&r, "a"), (CursorState::Held, 0));
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1);
        assert_eq!(without_cursors(&r), before, "nothing of the segment is stored");

        // Incoming, does not encode (a float) on a forked stream, where the
        // segment is hashed before validation: held + quarantined.
        let s1 = seg("a", 1, genesis("a"), &[1]);
        let (v2, w2) = (seg("a", 2, head(&s1), &[2]), seg("a", 2, head(&s1), &[3]));
        let mut r = device("r");
        r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2]).unwrap();
        let mut float = seg("a", 3, head(&v2), &[4]);
        float.ops[0].body.payload["text"] = serde_json::json!(1.5);
        let before = without_cursors(&r);
        let out = r.apply_segment(VAULT, "a", 3, 3, &float).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { ref reason } if reason.contains("does not encode")), "{out:?}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1);
        assert_eq!(state(&r, "a"), (CursorState::Held, 2));
        assert_eq!(without_cursors(&r), before);

        // Local: a stored row damaged to a float payload. A valid remote
        // segment then fails while re-projecting: Err (Corrupt), not Held.
        let mut r = device("r");
        r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r.conn()
            .execute("UPDATE ops SET payload = CAST('{\"pinned\":false,\"text\":1.5}' AS BLOB) WHERE op_id = ?1", [oid(1)])
            .unwrap();
        let before = r.dump_state().unwrap();
        let cursor = r.cursor(VAULT, "a").unwrap();
        let s2 = seg("a", 2, head(&s1), &[2]);
        let err = r.apply_sealed(VAULT, &seg_key(), &addr("a", 2, 2), &sealed(&s2)).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "the remote object is not blamed");
        assert_eq!(r.cursor(VAULT, "a").unwrap(), cursor, "the stream is untouched");
        assert_eq!(r.dump_state().unwrap(), before, "rolled back");
        let err = r.apply_fork_variant(VAULT, "a", 2, 2, &[s2.clone(), seg("a", 2, head(&s1), &[3])]).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
        assert_eq!(r.dump_state().unwrap(), before);
    }

    /// Cleaner 4 (a)/(c): schema violation and foreign origin → held +
    /// quarantine with nothing applied; a same-op-id/different-content op
    /// alone is quarantined while the rest of its segment applies.
    #[test]
    fn schema_and_origin_failures_hold_and_an_op_conflict_quarantines_only_that_op() {
        let key = seg_key();
        let s1 = seg("a", 1, genesis("a"), &[1]);
        let mut bad = seg("a", 2, head(&s1), &[2]);
        bad.ops[0].body.entity_id = "not an id".to_owned();
        let mut foreign = seg("a", 2, head(&s1), &[2]);
        foreign.ops[0].body.origin_device = "c".to_owned();
        for (name, s2) in [("schema", bad), ("origin", foreign)] {
            let mut r = device("r");
            r.apply_sealed(VAULT, &key, &addr("a", 1, 1), &sealed(&s1)).unwrap();
            let before = without_cursors(&r);
            let out = r.apply_sealed(VAULT, &key, &addr("a", 2, 2), &sealed(&s2)).unwrap();
            assert!(matches!(out, ApplyOutcome::Held { .. }), "{name}: {out:?}");
            assert_eq!(state(&r, "a"), (CursorState::Held, 1), "{name}");
            assert_eq!(r.quarantine_rows(VAULT).unwrap(), vec![(remote_key("a", 2, 2), match &out {
                ApplyOutcome::Held { reason } => reason.clone(),
                _ => unreachable!(),
            })], "{name}");
            assert_eq!(without_cursors(&r), before, "{name}: only the cursor state changed");
        }

        let mut r = device("r");
        r.apply_sealed(VAULT, &key, &addr("a", 1, 1), &sealed(&s1)).unwrap();
        let mut s2 = seg("a", 2, head(&s1), &[1, 3]);
        s2.ops[0].body.payload = serde_json::json!({"text": "forged", "pinned": true});
        let out = r.apply_sealed(VAULT, &key, &addr("a", 2, 3), &sealed(&s2)).unwrap();
        assert!(matches!(out, ApplyOutcome::Applied { conflicts: 1, .. }), "{out:?}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), [format!("op/{}", oid(1))]);
        assert_eq!(r.dots_of(VAULT, &oid(1)).unwrap(), vec![Dot { device: "a".to_owned(), seq: 1 }], "the forged dot is skipped");
        assert!(has(&r, 3), "the rest of the segment applied");
        assert_eq!(state(&r, "a"), (CursorState::Ok, 3));
    }

    /// Cleaner 4 (d): fork_in guards. A fork whose variants attach to nothing
    /// known is held: a gap blames no object; a variant that misses the known
    /// chain is quarantined. A fork from `held` applies when its variants
    /// attach (the held break was the other variant's chain), and variants
    /// with different `prev_seg_hash` each continuing a known head apply.
    #[test]
    fn fork_variants_must_attach_to_the_known_chain() {
        let s1 = seg("a", 1, genesis("a"), &[1]);
        let v2 = seg("a", 2, head(&s1), &[2]);
        let w2 = seg("a", 2, head(&s1), &[3]);

        // Gap: the reader has nothing of a yet.
        let mut r = device("r");
        let before = without_cursors(&r);
        let out = r.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2.clone()]).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { ref reason } if reason.starts_with("gap")), "{out:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "a gap blames no object");
        assert_eq!(state(&r, "a"), (CursorState::Held, 0));
        assert_eq!(without_cursors(&r), before);

        // Misses the known chain: both variants claim a prev nobody knows.
        let mut r = device("r");
        r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        let x2 = seg("a", 2, "0".repeat(64), &[2]);
        let y2 = seg("a", 2, "1".repeat(64), &[3]);
        let out = r.apply_fork_variant(VAULT, "a", 2, 2, &[x2, y2]).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { ref reason } if reason.contains("miss the known chain")), "{out:?}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1);
        assert_eq!(state(&r, "a"), (CursorState::Held, 1));
        assert!(!has(&r, 2) && !has(&r, 3));
        // Already applied but the other variant hangs elsewhere: also a miss.
        let mut r = device("r");
        r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r.apply_segment(VAULT, "a", 2, 2, &v2).unwrap();
        let stray = seg("a", 2, "2".repeat(64), &[3]);
        let out = r.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), stray]).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
        assert!(!has(&r, 3));

        // Held by the other variant's chain, then the fork shows up: applies.
        let mut r = device("r");
        r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r.apply_segment(VAULT, "a", 2, 2, &w2).unwrap();
        let v3 = seg("a", 3, head(&v2), &[4]);
        let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 3, 3), &sealed(&v3)).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
        assert_eq!(state(&r, "a"), (CursorState::Held, 2));
        assert_eq!(r.apply_segment(VAULT, "a", 3, 3, &v3).unwrap(), ApplyOutcome::Skipped(CursorState::Held));
        let out = r.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2.clone()]).unwrap();
        assert!(matches!(out, ApplyOutcome::Applied { fork_started: true, .. }), "{out:?}");
        assert_eq!(state(&r, "a"), (CursorState::Forked, 2));
        assert!(matches!(r.apply_segment(VAULT, "a", 3, 3, &v3).unwrap(), ApplyOutcome::Applied { .. }));
        assert!(has(&r, 2) && has(&r, 3) && has(&r, 4));
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]));

        // A second fork whose variants continue different known heads.
        let w3 = seg("a", 3, head(&w2), &[5]);
        let out = r.apply_fork_variant(VAULT, "a", 3, 3, &[v3.clone(), w3]).unwrap();
        assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
        assert!(has(&r, 5));
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]), "fork_start stays the minimum");
    }

    /// Red-team r02b: a forked stream applies every segment continuing a
    /// known head (writes made before the fork was noticed), dedups relists,
    /// and holds + quarantines a seq skip after a known head. Architect B1: a
    /// child listed before its parent (list lag) is deferred, not held, and
    /// applies once the parent is in; the reader then equals an in-order one.
    #[test]
    fn a_forked_stream_applies_continuations_of_every_known_head() {
        let s1 = seg("a", 1, genesis("a"), &[1]);
        let v2 = seg("a", 2, head(&s1), &[2]);
        let w2 = seg("a", 2, head(&s1), &[3]);
        let v3 = seg("a", 3, head(&v2), &[4]);
        let w3 = seg("a", 3, head(&w2), &[5]);
        let mut r = device("r");
        r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2.clone()]).unwrap();
        for s in [&v3, &w3] {
            assert!(matches!(r.apply_segment(VAULT, "a", 3, 3, s).unwrap(), ApplyOutcome::Applied { .. }));
        }
        assert!(has(&r, 4) && has(&r, 5));
        assert_eq!(r.apply_segment(VAULT, "a", 3, 3, &v3).unwrap(), ApplyOutcome::Duplicate);
        assert_eq!(r.apply_segment(VAULT, "a", 1, 1, &s1).unwrap(), ApplyOutcome::Duplicate, "relisted prefix");
        assert_eq!(state(&r, "a"), (CursorState::Forked, 3));

        // A seq skip after a known head is the object's fault.
        let mut r2 = device("r");
        r2.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r2.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2.clone()]).unwrap();
        let skip = seg("a", 4, head(&v2), &[6]);
        let out = r2.apply_segment(VAULT, "a", 4, 4, &skip).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
        assert_eq!(r2.quarantine_rows(VAULT).unwrap().len(), 1);
        assert_eq!(r2.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held);
        // A child before its parent is lag: deferred, nothing written, then
        // parent and child apply on the relist.
        let mut r3 = device("r");
        r3.apply_segment(VAULT, "a", 1, 1, &s1).unwrap();
        r3.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2.clone()]).unwrap();
        let v4 = seg("a", 4, head(&v3), &[7]);
        let before = r3.dump_state().unwrap();
        let out = r3.apply_segment(VAULT, "a", 4, 4, &v4).unwrap();
        assert!(matches!(out, ApplyOutcome::Deferred { .. }), "{out:?}");
        assert!(r3.quarantine_rows(VAULT).unwrap().is_empty());
        assert_eq!(r3.dump_state().unwrap(), before, "a deferral writes nothing");
        assert_eq!(state(&r3, "a"), (CursorState::Forked, 2));
        // The engine relists the forked stream in full: every name again.
        assert_eq!(r3.apply_segment(VAULT, "a", 1, 1, &s1).unwrap(), ApplyOutcome::Duplicate);
        r3.apply_fork_variant(VAULT, "a", 2, 2, &[v2.clone(), w2.clone()]).unwrap();
        for s in [&v3, &w3] {
            assert!(matches!(r3.apply_segment(VAULT, "a", 3, 3, s).unwrap(), ApplyOutcome::Applied { .. }));
        }
        assert!(matches!(r3.apply_segment(VAULT, "a", 4, 4, &v4).unwrap(), ApplyOutcome::Applied { .. }));
        assert!(r3.apply_segment(VAULT, "a", 4, 4, &v4).unwrap() == ApplyOutcome::Duplicate);
        assert!(matches!(r.apply_segment(VAULT, "a", 4, 4, &v4).unwrap(), ApplyOutcome::Applied { .. }));
        assert_eq!(r3.projection(VAULT).unwrap().hash, r.projection(VAULT).unwrap().hash, "equals the in-order reader");
        assert!(r3.quarantine_rows(VAULT).unwrap().is_empty() && has(&r3, 7));
        assert_eq!(state(&r3, "a"), (CursorState::Forked, 4));
    }

    /// Cleaner 2: checkpoint() propagates a frontier failure instead of
    /// recording `{}`; only an active vault without an identity yet has an
    /// empty frontier.
    #[test]
    fn checkpoint_propagates_frontier_errors() {
        use crate::sync::compaction::checkpoint;
        let dir = TempDir::new("ckpt-frontier");
        let mut fresh = Store::open_in_memory().unwrap();
        fresh.set_active_vault(VAULT).unwrap();
        checkpoint(&mut fresh, &dir.0.join("fresh")).unwrap();
        let recorded: String = fresh.conn().query_row("SELECT frontier_json FROM checkpoints", [], |r| r.get(0)).unwrap();
        assert_eq!(recorded, "{}");

        let mut a = device("a");
        local(&mut a, create(1, 1_000, M, "x"));
        checkpoint(&mut a, &dir.0.join("a")).unwrap();
        let recorded: String =
            a.conn().query_row("SELECT frontier_json FROM checkpoints ORDER BY id DESC", [], |r| r.get(0)).unwrap();
        assert_eq!(recorded, r#"{"a":1}"#);
        a.conn().execute("UPDATE vault_identity SET chain_head = x'00'", []).unwrap();
        let rows = |s: &Store| s.conn().query_row("SELECT COUNT(*) FROM checkpoints", [], |r| r.get::<_, i64>(0)).unwrap();
        let before = rows(&a);
        assert!(matches!(checkpoint(&mut a, &dir.0.join("a")), Err(StoreError::Corrupt(_))));
        assert_eq!(rows(&a), before, "no checkpoint with a wrong rollback reference");
    }

    /// Red-team r02b through the store streams of the simulator: the edit
    /// made after the fork by the original reaches readers in every order.
    #[test]
    fn continued_writes_after_a_fork_reach_a_reader_that_saw_both_variants_first() {
        let mut remote = Remote::new(false);
        let mut a = device("a");
        local(&mut a, create(1, 100, M, "base"));
        publish(&mut a, &mut remote).unwrap();
        let dir = TempDir::new("store-fork");
        let path = dir.0.join("clone.db");
        a.conn().execute("VACUUM INTO ?1", [path.to_string_lossy().into_owned()]).unwrap();
        let mut clone = Store::open(&path).unwrap();
        local(&mut a, edit(2, 200, M, 1, "one"));
        local(&mut clone, edit(3, 210, M, 1, "two"));
        publish(&mut a, &mut remote).unwrap();
        publish(&mut clone, &mut remote).unwrap();
        local(&mut a, create(4, 300, K, "after"));
        publish(&mut a, &mut remote).unwrap();
        local(&mut clone, edit(5, 310, M, 3, "two more"));
        publish(&mut clone, &mut remote).unwrap();
        assert_eq!(remote.objects[&remote_key("a", 3, 3)].len(), 2, "both continue under the same name");
        let mut r = device("r");
        let outcomes = sync_stream(&mut r, &remote, "a", &crate::sync::sim_tests::all);
        assert!(outcomes.iter().all(|o| matches!(o, ApplyOutcome::Applied { .. })), "{outcomes:?}");
        for n in 1..=5 {
            assert!(has(&r, n), "op {n} applied");
        }
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]));
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn blob(n: u8) -> BlobRow {
        BlobRow { blob_id: format!("{n:02x}{}", "ab".repeat(31)), mime: "image/png".into(), bytes: vec![n; 4], sha256: [n; 32] }
    }

    #[test]
    fn local_batch_is_all_or_nothing() {
        let mut store = device("a");
        local(&mut store, create(1, 1_000, M, "before"));
        let before = store.dump_state().unwrap();
        let mut bad = create(3, 3_000, K, "z");
        bad.payload = serde_json::json!({"text": 1});
        let result = store.insert_local_batch(
            VAULT,
            vec![edit(2, 2_000, M, 1, "y"), bad, create(4, 4_000, K, "w")],
            vec![blob(1)],
            vec![("migration_state".into(), "imported".into())],
        );
        assert!(result.is_err());
        assert_eq!(store.dump_state().unwrap(), before, "nothing stored");
        assert_eq!(store.meta_get("migration_state").unwrap(), None);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM blobs"), 0);
        // No seq was consumed: the next op still gets seq 2.
        let next = store.insert_local_op(VAULT, edit(5, 5_000, M, 1, "next"), UploadState::Never).unwrap();
        assert_eq!(next.dot, Dot { device: "a".to_owned(), seq: 2 });
    }

    #[test]
    fn local_batch_matches_sequential_inserts_and_writes_blobs_and_meta() {
        let bodies = || vec![create(1, 1_000, M, "x"), edit(2, 2_000, M, 1, "y"), create(3, 3_000, K, "k")];
        let mut seq_store = device("a");
        let mut last = None;
        for body in bodies() {
            last = Some(seq_store.insert_local_op(VAULT, body, UploadState::Never).unwrap());
        }

        let mut store = device("a");
        store.meta_set("migration_state", "importing").unwrap();
        let ops = store
            .insert_local_batch(
                VAULT,
                bodies(),
                vec![blob(1), blob(2)],
                vec![("migration_state".into(), "imported".into()), ("other".into(), "1".into())],
            )
            .unwrap();
        assert_eq!(ops.len(), 3);
        for (i, op) in ops.iter().enumerate() {
            let seq = i as u64 + 1;
            assert_eq!(op.dot, Dot { device: "a".to_owned(), seq });
            assert_eq!(store.dots_of(VAULT, &op.body.op_id).unwrap(), vec![op.dot.clone()], "one dot each");
            let ctx = if seq > 1 { BTreeMap::from([("a".to_owned(), seq - 1)]) } else { BTreeMap::new() };
            assert_eq!(op.body.ctx, ctx);
        }
        let hash = last.unwrap().projection_hash;
        assert!(ops.iter().all(|op| op.projection_hash == hash), "same projection as sequential inserts");
        assert_eq!(store.projection(VAULT).unwrap().hash, hash);
        assert_eq!(store.meta_get("migration_state").unwrap().as_deref(), Some("imported"), "upserted");
        assert_eq!(store.meta_get("other").unwrap().as_deref(), Some("1"));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM blobs"), 2);
        let (mime, bytes, sha): (String, Vec<u8>, Vec<u8>) = store
            .conn()
            .query_row("SELECT mime, bytes, sha256 FROM blobs WHERE blob_id = ?1", [&blob(2).blob_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((mime.as_str(), bytes, sha), ("image/png", vec![2; 4], vec![2; 32]));
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM vault_blob_map WHERE vault_id = 'vault-1' AND local_blob = blob_id AND remote_state = 'pending'"
            ),
            2
        );
        // Re-inserting a known blob keeps its map row's state.
        store.conn().execute("UPDATE vault_blob_map SET remote_state = 'uploaded'", []).unwrap();
        store.insert_local_batch(VAULT, vec![], vec![blob(1)], vec![]).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM vault_blob_map WHERE remote_state = 'pending'"), 0);
    }

    #[test]
    fn local_batch_requires_the_vault_identity() {
        let mut store = device("a");
        let err = store.insert_local_batch("vault-x", vec![], vec![], vec![("k".into(), "v".into())]).unwrap_err();
        assert!(matches!(err, StoreError::NoIdentity));
        assert_eq!(store.meta_get("k").unwrap(), None);
    }

    /// G10 A15: the planned path re-projects from its in-memory set and
    /// writes only the rows that changed; the persisted projection (hash,
    /// `entities` incl. `evicted_local`, `msg_tree`, `memo_lineage`) must be
    /// exactly what a full SQL load and rewrite produce, across memo edits,
    /// a new conversation message, a tombstone and a lineage change.
    #[test]
    fn planned_batch_projects_like_the_loaded_path() {
        use crate::sync::sim_tests::op;
        let conv = "0190a000-0000-7000-8000-0000000000cc";
        let msg = |n: u64, wall: u64, id: &str, parent: Option<&str>| {
            op(
                n,
                wall,
                Kind::Conv,
                conv,
                "msg.add",
                serde_json::json!({"conv_id": conv, "msg_id": id, "parent_id": parent, "role": "user",
                                   "content": id, "attachments": [], "ts": wall}),
            )
        };
        let seed = |store: &mut Store| {
            local(store, create(1, 1_000, M, "x"));
            local(store, create(2, 2_000, K, "k"));
            local(store, op(3, 3_000, Kind::Conv, conv, "create", serde_json::json!({"title": "t"})));
            local(store, msg(4, 4_000, "m1", None));
            // An evicted row must keep its flag through an unrelated write.
            store.conn().execute("UPDATE entities SET evicted_local = 1 WHERE id = ?1", [K]).unwrap();
        };
        let batch = || {
            vec![
                edit(5, 5_000, M, 1, "y"),
                crate::sync::sim_tests::delete(6, 6_000, K),
                msg(7, 7_000, "m2", Some("m1")),
                create(8, 8_000, "0190a000-0000-7000-8000-0000000000dd", "new"),
            ]
        };

        let mut planned = device("a");
        seed(&mut planned);
        let (ops, seen) = planned.plan_local_batch(VAULT, |current| (batch(), current.entities.len())).unwrap();
        assert_eq!(seen, 3, "the planner saw the projection before its own ops");
        assert_eq!(ops.len(), 4);
        assert_eq!(ops[0].dot, Dot { device: "a".to_owned(), seq: 5 });
        assert_eq!(ops[3].dot, Dot { device: "a".to_owned(), seq: 8 });
        assert_eq!(ops[3].body.ctx, BTreeMap::from([("a".to_owned(), 7)]), "ctx advanced per op");
        let planned_hash = ops[0].projection_hash;
        let planned_rows = planned.dump_state().unwrap();

        // The same ops through the sequential full-rewrite path.
        let mut full = device("a");
        seed(&mut full);
        let mut last = None;
        for body in batch() {
            last = Some(full.insert_local_op(VAULT, body, UploadState::Never).unwrap());
        }
        assert_eq!(last.unwrap().projection_hash, planned_hash, "same projection hash");
        assert_eq!(planned_rows, full.dump_state().unwrap(), "same persisted rows");

        // And a full rewrite over the planned store's own rows changes nothing.
        let reloaded = reproject_conn(planned.conn(), VAULT).unwrap();
        assert_eq!(reloaded.hash, planned_hash);
        assert_eq!(planned.dump_state().unwrap(), planned_rows);
        assert_eq!(count(&planned, "SELECT COUNT(*) FROM msg_tree"), 2);
        assert_eq!(count(&planned, "SELECT COUNT(*) FROM entities WHERE evicted_local = 1"), 1);
        assert_eq!(count(&planned, &format!("SELECT deleted FROM entities WHERE id = '{K}'")), 1, "tombstone");
    }

    #[test]
    fn planned_batch_with_no_ops_commits_nothing() {
        let mut store = device("a");
        local(&mut store, create(1, 1_000, M, "x"));
        let before = store.dump_state().unwrap();
        let (ops, out) = store.plan_local_batch(VAULT, |_| (Vec::new(), 7u8)).unwrap();
        assert!(ops.is_empty());
        assert_eq!(out, 7);
        assert_eq!(store.dump_state().unwrap(), before);
        let next = store.insert_local_op(VAULT, edit(5, 5_000, M, 1, "next"), UploadState::Never).unwrap();
        assert_eq!(next.dot.seq, 2, "no seq consumed");
    }

    #[test]
    fn planned_batch_is_all_or_nothing() {
        let mut store = device("a");
        local(&mut store, create(1, 1_000, M, "x"));
        let before = store.dump_state().unwrap();
        let mut bad = create(3, 3_000, K, "z");
        bad.payload = serde_json::json!({"text": 1});
        let err = store.plan_local_batch(VAULT, |_| (vec![edit(2, 2_000, M, 1, "y"), bad], ())).unwrap_err();
        assert!(!matches!(err, StoreError::NoIdentity), "{err:?}");
        assert_eq!(store.dump_state().unwrap(), before, "nothing stored");
        let err = store.plan_local_batch("vault-x", |_| (vec![create(9, 9_000, K, "w")], ())).unwrap_err();
        assert!(matches!(err, StoreError::NoIdentity), "{err:?}");
    }
}
