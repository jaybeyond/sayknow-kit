//! The sync engine (§6, G7 design "sync/engine.rs"): one single-flight cycle
//! over a bound provider, plus the scheduler that decides when cycles run.
//!
//! A cycle runs, in order:
//! 0. Gates: wipe state (no network at all), the vault key, and
//!    `vault::check_vaults` (manifest 404 twice → pause, never recreate;
//!    another vault id → banner notice, never auto-merge).
//! 1. Identity check (`identity::detect`, T1–T8). A trigger settles every
//!    `attempted` segment first (W-2) and only then plans and applies the
//!    rotation (R4 keychain first, R5–R7 one SQLite transaction). If an
//!    attempt stays unknown the rotation is deferred: no uploads, no restamp.
//! 2. Catch-up: every other device's stream (retired own ids included) in
//!    seq order; a name holding several readable copies is a fork; forked
//!    streams are relisted in full; Drive names whose copy set changed are
//!    refetched; snapshots not applied this session are applied first (a
//!    compacted stream is only reachable through them); a gap no snapshot
//!    covers yet is retried next cycle, never held. The apply half
//!    ([`apply_snapshot_copies`], [`apply_stream`]) is shared with vault J2
//!    through [`fetch_vault`] / [`apply_vault_download`] (D4). Attachment
//!    blobs referenced by applied ops are then downloaded, bounded per
//!    cycle; a missing / oversize / unverifiable blob stays evicted with a
//!    notice and never blocks op apply (D5).
//! 3. Upload: settle old attempts, then seal new ops (persisted before any
//!    put, never resealed, split below [`SEGMENT_PLAIN_CAP`], A4), then
//!    upload the pending blobs (create-only; `AlreadyExists` → decrypt and
//!    compare the HMAC: dedupe, else fail closed; a blob too large for one
//!    object is `skipped_oversize` with one notice, D5b), and only then put
//!    the sealed segments. D5a: a segment is put only when no blob its own
//!    ops reference is still pending (derived from the segment's ops, not
//!    from timing). Each put marks `attempted`, `put_create`, then verifies
//!    through `publish::settle_attempted` (Published / PublishedWithFork /
//!    Rejected → quarantine + T6 rotation / StillAttempted). A `put_create`
//!    answered RateLimited / QuotaExceeded leaves the segment `attempted` and
//!    ends the cycle with that status (Retry-After honoured, D10).
//! 4. Compaction: snapshot create-only + verify, then delete own segments
//!    only as `segments_deletable` allows, then local checkpoints.
//! 5. `devices/<self>.rec`: the first publish of a device id is unconditional; afterwards
//!    `put_replace` + If-Match on the listed etag; Drive updates exactly the
//!    file id it just read (`put_replace_ref`: 404 → NotFound, never a
//!    create, D1a). A published record that is absent is never recreated
//!    (D1); 412 / 404 → re-read and T4 check.
//!
//! Wipe gate (D7, D7a): the own manifest is re-read before every put batch
//! (blobs + segments, the snapshot, the record). A miss counts only after a
//! second read separated by the settle pause; then the cycle stops
//! `ManifestMissing` and creates nothing more. The engine never deletes
//! remote objects on a manifest miss and leaves local upload state as it is:
//! what this cycle already created is left for the wiper's final sweep.
//!
//! D3a: more than one copy of the own manifest seen in two consecutive cycles
//! raises [`EngineNotice::ManifestCopies`] (the count only); nothing is
//! deleted.
//!
//! Locks: the store and clock mutexes are never held across a provider
//! `.await` (the cycle future is `Send`, which a held `MutexGuard` would
//! break; `cycle_future_is_send` pins that), and the clock is only taken
//! with the store released (lock order clipboard → store → clock).
//!
//! Decisions (recorded for review):
//! - Contract deviation: `CycleCtx` has a third type parameter for the
//!   `publish::Pause` implementation (its future is not object safe).
//! - Re-projection after a fork: every apply transaction already re-projects
//!   in full; the engine re-projects once more after a fork, measures it into
//!   `SyncStats::reproject_ms_max`, and emits a notice. The incremental
//!   fixpoint path is not added: the full fixpoint is the reference
//!   behaviour and no measurement here shows it slow; the counter exposes it.
//! - Obs rule (ii) uses the routed memo: `Store::insert_local_op` already
//!   joins `memo_lineage.routed_to` (no engine code needed).
//! - A3: a cycle that ends with an `attempted` segment still unknown counts;
//!   at [`A3_NOTICE_CYCLES`] consecutive cycles the "upload outcome unknown"
//!   notice is raised. Uploads stay stopped meanwhile (the chain needs it).
//! - Drive resends: each settle on a store that is not create-only counts
//!   (persisted in meta, `resend:<vault>:<device>:<first>`); at
//!   [`MAX_DRIVE_RESENDS`] the engine stops resending that segment and
//!   raises a user-visible notice. Created file ids go to `remote_index`.
//! - A removed device (D1): `rec_published:<vault>:<device>` holds the wall
//!   time of the last successful record put and `rec_put_sha:...` the
//!   SHA-256 of its bytes; `rec_verified:...` is set only once a read of the
//!   record returns exactly those bytes (D1b: a lagging read of an older
//!   version proves nothing). A record absent on two reads stops
//!   the cycle (`DeviceRemoved`, zero uploads until a rejoin) when it was
//!   read back since the put or the put is older than [`SETTLE_WINDOW_MS`]
//!   (ADV-7: a fresh put a lagging store does not show yet is no removal).
//!   Inside that window the record is neither trusted nor written.
//! - An unreadable own record, or an oversize segment/snapshot, stops (no
//!   upload) with a notice; nothing is deleted or overwritten.
//! - Retryable: `Transient`, `RateLimited`, and `Permanent` text naming HTTP
//!   408 / 423 or `RequestTimeTooSkewed` (ADV-6). The R2 adapter maps a 403
//!   `RequestTimeTooSkewed` to `Transient` (A7).
//! - Blob download per cycle: at most [`BLOB_FETCHES_PER_CYCLE`] objects of
//!   at most [`MAX_BLOB_BYTES`] plaintext; the rest waits for later cycles.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::compaction::{self, Snapshot};
use super::crypto::{derive_subkey, hex, hmac_sha256, Purpose, SecretKey};
use super::envelope::{check_record_owner, check_snapshot_owner, open, seal, ObjectAddress, ObjectKind};
use super::identity::{self, DeviceIdentity, IdentityCheck, Observation, RemoteRecord, RemoteSegment, SeqRange, Trigger};
use super::ops::{canonical_json, segment_hash, Hlc, HlcClock, OpError, Segment};
use super::provider::{Bound, ListQuery, ListedObject, ObjectStore, ProviderError, RemoteKind, MAX_OBJECT_BYTES};
use super::publish::{settle_attempted, Outcome, Pause, RemoteObject, Verdict, RECHECK_DELAYS};
use super::rotation::{RotationError, UploadState};
use super::secrets::{load_vault_key, SecretError, SecretStore};
use super::store::{
    open_segment, ApplyOutcome, CursorState, Store, StoreError, BLOB_EVICTED, BLOB_PUBLISHED, BLOB_SKIPPED_OVERSIZE,
    SEGMENT_PLAIN_CAP,
};
use super::telemetry::{log_event, FailureClass, Field, SyncStats, VaultTag};
use super::vault;

/// Consecutive cycles with an unknown upload outcome before the A3 notice.
pub(crate) const A3_NOTICE_CYCLES: u32 = 3;
/// Consecutive cycles that must see several own manifest copies before the
/// D3a notice (one cycle may land inside a Drive reissue's create→delete).
pub(crate) const MANIFEST_COPY_CYCLES: u32 = 2;
/// Settles per segment on a store that is not create-only (Drive, A2).
pub(crate) const MAX_DRIVE_RESENDS: u64 = 3;
/// A device whose record was last seen this long ago no longer holds back
/// segment deletion (§4.5 "활성 기기(90일)").
pub(crate) const ACTIVE_DEVICE_MS: u64 = 90 * 24 * 60 * 60 * 1000;
/// A record put younger than this that the store does not show yet (and
/// that was not read back since) is lag, not a removal (D1, ADV-7).
pub(crate) const SETTLE_WINDOW_MS: u64 = 60 * 1000;
/// Blob objects fetched per cycle at most (D5 "bounded").
pub(crate) const BLOB_FETCHES_PER_CYCLE: usize = 16;
/// Largest blob plaintext downloaded; a larger object stays evicted.
pub(crate) const MAX_BLOB_BYTES: usize = 32 << 20;

const META_WIPE_STATE: &str = "wipe_state";
const META_CHECKPOINT_RESTORED: &str = "checkpoint_restored";

/// Wall-clock source, injectable for tests.
pub(crate) type NowFn = dyn Fn() -> u64 + Send + Sync;

/// What the webview may see about a cycle: fixed enum texts and counts only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "notice", rename_all = "snake_case")]
pub(crate) enum EngineNotice {
    NotJoined,
    MissingVaultKey,
    /// Keychain and database disagree in a way only a rejoin can repair.
    IdentityLost,
    KeychainUnavailable,
    WipeInProgress,
    /// The own manifest is gone (another device wiped the vault): paused,
    /// never recreated.
    ManifestMissing,
    /// Another vault id exists under this account (two-vault banner).
    OtherVaults { count: usize },
    /// D3a: several copies of the own manifest persisted across two cycles
    /// (a crashed or concurrent RK reissue). One vault; nothing is deleted;
    /// the user cleans up. Carries the count only.
    ManifestCopies { count: usize },
    /// This device's record was removed: no uploads until a rejoin.
    DeviceRemoved,
    ReconsentRequired,
    PermissionDenied,
    RateLimited,
    QuotaExceeded,
    BindingMismatch,
    Unsupported,
    /// A listing answered NotFound (missing bucket/folder): never empty.
    ConfigurationError,
    /// The own record cannot be read; uploads stop (fail closed).
    RecordUnreadable,
    Rotated { triggers: Vec<&'static str> },
    UploadRejected,
    /// A blob name holds an object that is not this content (D5): nothing is
    /// overwritten and no segment referencing it is uploaded while it stays.
    BlobRejected,
    /// An attachment could not be downloaded (missing, over the cap, or
    /// unverifiable): it stays evicted; its message applies normally. Also
    /// raised once when an own attachment is too large to upload (D5b,
    /// `skipped_oversize`): readers see it evicted.
    BlobEvicted,
    ForkDetected,
    Reprojected,
    /// A3: "sync paused: upload outcome unknown".
    UploadOutcomeUnknown,
    /// A2: a Drive segment reached its resend cap.
    ResendCapReached,
    SegmentTooLarge,
    SnapshotTooLarge,
    SnapshotVerifyFailed,
    SnapshotConflict,
    Quarantined,
    StreamHeld,
    /// Another device's stream starts past our cursor and no snapshot covers
    /// it yet; retried every cycle, the stream is not held.
    StreamGap,
    UpdateRequired,
}

/// Why a cycle stopped until the user acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    NotJoined,
    MissingVaultKey,
    IdentityLost,
    WipeInProgress,
    ManifestMissing,
    DeviceRemoved,
    Reconsent,
    PermissionDenied,
    BindingMismatch,
    Unsupported,
    Configuration,
    RecordUnreadable,
    /// The active vault changed under this cycle (D6 generation fence): it
    /// was abandoned before sending more. No notice, no failure.
    VaultSwitched,
}

impl StopReason {
    fn notice(self) -> Option<EngineNotice> {
        Some(match self {
            Self::NotJoined => EngineNotice::NotJoined,
            Self::MissingVaultKey => EngineNotice::MissingVaultKey,
            Self::IdentityLost => EngineNotice::IdentityLost,
            Self::WipeInProgress => EngineNotice::WipeInProgress,
            Self::ManifestMissing => EngineNotice::ManifestMissing,
            Self::DeviceRemoved => EngineNotice::DeviceRemoved,
            Self::Reconsent => EngineNotice::ReconsentRequired,
            Self::PermissionDenied => EngineNotice::PermissionDenied,
            Self::BindingMismatch => EngineNotice::BindingMismatch,
            Self::Unsupported => EngineNotice::Unsupported,
            Self::Configuration => EngineNotice::ConfigurationError,
            Self::RecordUnreadable => EngineNotice::RecordUnreadable,
            Self::VaultSwitched => return None,
        })
    }

    fn class(self) -> Option<FailureClass> {
        match self {
            Self::Reconsent | Self::PermissionDenied => Some(FailureClass::Auth),
            Self::BindingMismatch => Some(FailureClass::Binding),
            Self::Unsupported => Some(FailureClass::Unsupported),
            Self::Configuration | Self::RecordUnreadable => Some(FailureClass::Permanent),
            Self::IdentityLost | Self::MissingVaultKey => Some(FailureClass::Local),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NotJoined => "not_joined",
            Self::MissingVaultKey => "missing_vault_key",
            Self::IdentityLost => "identity_lost",
            Self::WipeInProgress => "wipe",
            Self::ManifestMissing => "manifest_missing",
            Self::DeviceRemoved => "device_removed",
            Self::Reconsent => "reconsent",
            Self::PermissionDenied => "permission_denied",
            Self::BindingMismatch => "binding",
            Self::Unsupported => "unsupported",
            Self::Configuration => "configuration",
            Self::RecordUnreadable => "record_unreadable",
            Self::VaultSwitched => "vault_switched",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum CycleStatus {
    Ok,
    /// Waits for the user (reconsent, rejoin, configuration, ...).
    Stopped { reason: StopReason },
    /// Retry after `retry_after` when the provider named one.
    RateLimited { retry_after: Option<Duration> },
    /// Retry with backoff.
    Failed { class: FailureClass },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CycleReport {
    pub(crate) status: CycleStatus,
    /// Sorted, unique.
    pub(crate) notices: Vec<EngineNotice>,
    pub(crate) applied_segments: usize,
    pub(crate) uploaded_segments: usize,
    /// The projection may have changed: the caller re-mirrors memos.
    pub(crate) changed: bool,
}

/// State one engine keeps between cycles (in memory; what must survive a
/// restart lives in the store's meta / remote_index).
#[derive(Debug, Default)]
pub(crate) struct Engine {
    /// Consecutive cycles that ended with an unknown upload outcome (A3).
    unsettled_cycles: u32,
    /// Digest of each own-stream copy by provider_ref (T5 input).
    digests: HashMap<String, [u8; 32]>,
    /// Copy set already fetched per key (Drive: a later second copy of an
    /// applied name must be fetched again).
    seen_refs: HashMap<String, BTreeSet<String>>,
    /// Own keys deleted by compaction; a lagging listing must not read them
    /// as foreign writes (T5).
    recently_deleted: BTreeSet<String>,
    /// Snapshot keys already applied (or quarantined) this session.
    applied_snaps: BTreeSet<String>,
    /// What the last written record said, minus `last_seen_hlc`.
    last_record: Option<[u8; 32]>,
    /// Blob ids still to download (D5); refreshed after ops apply.
    blob_backlog: Option<Vec<String>>,
    /// Consecutive cycles that saw more than one own manifest copy (D3a).
    manifest_copy_cycles: u32,
}

/// Everything one cycle needs. The store and clock are shared with the
/// command layer and taken only briefly.
pub(crate) struct CycleCtx<'a, P: ObjectStore, S: SecretStore, Z: Pause> {
    pub(crate) provider: &'a Bound<P>,
    pub(crate) secrets: &'a S,
    pub(crate) store: &'a Mutex<Store>,
    pub(crate) clock: &'a Mutex<HlcClock>,
    pub(crate) pause: &'a Z,
    pub(crate) now: &'a NowFn,
    pub(crate) stats: &'a SyncStats,
    pub(crate) engine: &'a mut Engine,
    pub(crate) vault: &'a str,
    pub(crate) key_epoch: u32,
    /// `None` when unreadable this cycle: T2 is off (fail-open, amendment).
    pub(crate) install_fp: Option<&'a str>,
    pub(crate) device_label: &'a str,
    pub(crate) app_version: &'a str,
    /// Where local checkpoints go after a compaction; `None` skips them.
    pub(crate) checkpoint_dir: Option<&'a Path>,
    /// D6 vault-generation fence: `true` while this cycle's vault is still
    /// the current one. Checked at cycle start and before every put batch;
    /// `false` stops the cycle (`VaultSwitched`) before anything more is
    /// sent. `None` = no fence.
    pub(crate) fence: Option<&'a (dyn Fn() -> bool + Send + Sync)>,
}

/// `devices/<id>.rec` plaintext (§4.2), canonical JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceRecord {
    pub(crate) v: u64,
    pub(crate) device_id: String,
    pub(crate) label: String,
    pub(crate) last_seq: u64,
    /// Lowercase hex of the 32-byte chain head.
    pub(crate) chain_head: String,
    pub(crate) frontier: BTreeMap<String, u64>,
    pub(crate) last_seen_hlc: Hlc,
    pub(crate) app_version: String,
}

impl DeviceRecord {
    fn chain_head_bytes(&self) -> Option<[u8; 32]> {
        let bytes = data_encoding::HEXLOWER.decode(self.chain_head.as_bytes()).ok()?;
        bytes.try_into().ok()
    }

    fn remote(&self) -> Option<RemoteRecord> {
        Some(RemoteRecord { last_seq: self.last_seq, chain_head: self.chain_head_bytes()? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordError {
    Crypto,
    Malformed,
    Owner,
}

pub(crate) fn seal_record(
    dek: &SecretKey,
    vault: &str,
    epoch: u32,
    record: &DeviceRecord,
) -> Result<Vec<u8>, RecordError> {
    let value = serde_json::to_value(record).map_err(|_| RecordError::Malformed)?;
    let plain = canonical_json(&value).map_err(|_| RecordError::Malformed)?;
    let address = ObjectAddress::device_record(vault, epoch, &record.device_id);
    seal(&derive_subkey(dek, vault, epoch, Purpose::DeviceRecord), &address, &plain).map_err(|_| RecordError::Crypto)
}

/// Opens `devices/<device>.rec` and cross-checks the owner it claims.
pub(crate) fn open_record(
    dek: &SecretKey,
    vault: &str,
    epoch: u32,
    device: &str,
    bytes: &[u8],
) -> Result<DeviceRecord, RecordError> {
    let address = ObjectAddress::device_record(vault, epoch, device);
    let key = derive_subkey(dek, vault, epoch, Purpose::DeviceRecord);
    let plain = open(&key, &address, bytes).map_err(|_| RecordError::Crypto)?;
    let record: DeviceRecord = serde_json::from_slice(&plain).map_err(|_| RecordError::Malformed)?;
    if record.v != 1 || record.chain_head_bytes().is_none() {
        return Err(RecordError::Malformed);
    }
    check_record_owner(&record.device_id, &address).map_err(|_| RecordError::Owner)?;
    Ok(record)
}

/// G9 U4: one device of the vault, as its verified record states it. The
/// device id and the record's wall time only; no label or version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceSeen {
    pub(crate) device_id: String,
    pub(crate) last_seen_ms: u64,
}

/// G9 U4: every `devices/<id>.rec` of `vault` that opens under the DEK at its
/// own address (owner cross-checked). A record that does not open (forged,
/// another vault's, corrupt) is left out; a name that vanished between the
/// listing and the read is skipped. Any other provider error is returned so
/// the caller keeps what it had. Separate from [`Engine::cycle`]: the cycle
/// reads other devices' records only while compacting.
pub(crate) async fn read_devices<P: ObjectStore>(
    p: &P,
    dek: &SecretKey,
    vault: &str,
    epoch: u32,
) -> Result<Vec<DeviceSeen>, ProviderError> {
    let mut names = BTreeMap::new();
    for object in p.list(ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Rec), device_id: None }).await? {
        let Some(address) = ObjectAddress::from_remote_key(&object.key, epoch) else { continue };
        if address.kind == ObjectKind::DeviceRecord && address.vault_id == vault {
            names.insert(address.device_id, object.key);
        }
    }
    let mut devices = Vec::new();
    for (device, key) in names {
        let bytes = match p.get(&key).await {
            Ok(bytes) => bytes,
            Err(ProviderError::NotFound) => continue,
            Err(err) => return Err(err),
        };
        if let Ok(record) = open_record(dek, vault, epoch, &device, &bytes) {
            devices.push(DeviceSeen { device_id: record.device_id, last_seen_ms: record.last_seen_hlc.wall_ms });
        }
    }
    Ok(devices)
}

/// G9 U5: the projection's restored-after-delete notices as (kind, digest)
/// pairs, `digest = SHA-256("sayknow-restored-v1" ‖ kind ‖ "/" ‖ id)`. The
/// entity id never leaves this function; callers count and compare digests.
pub(crate) fn restored_after_delete(notices: &[super::merge::Notice]) -> Vec<(super::ops::Kind, [u8; 32])> {
    use super::merge::Notice;
    notices
        .iter()
        .filter_map(|n| match n {
            Notice::RestoredAfterDeleteConflict { kind, entity_id } => Some((
                *kind,
                Sha256::new()
                    .chain_update(b"sayknow-restored-v1")
                    .chain_update(kind.as_str().as_bytes())
                    .chain_update(b"/")
                    .chain_update(entity_id.as_bytes())
                    .finalize()
                    .into(),
            )),
            Notice::MemoConflictCopy { .. } => None,
        })
        .collect()
}

/// A snapshot's id is derived from its plaintext, so the name binds content.
pub(crate) fn snapshot_id(plain: &[u8]) -> String {
    hex(&Sha256::digest(plain)[..16])
}

fn open_snapshot(key: &SecretKey, address: &ObjectAddress, bytes: &[u8]) -> Result<Snapshot, OpError> {
    let plain = open(key, address, bytes).map_err(|_| OpError::Schema("snapshot does not open".to_owned()))?;
    let snapshot = Snapshot::decode(&plain)?;
    if snapshot_id(&plain) != address.object_ref {
        return Err(OpError::Schema("snapshot id does not match its content".to_owned()));
    }
    check_snapshot_owner(&snapshot.device_id, &address.object_ref, address)
        .map_err(|_| OpError::Schema("snapshot owner mismatch".to_owned()))?;
    if snapshot.vault_id != address.vault_id {
        return Err(OpError::Schema("snapshot vault mismatch".to_owned()));
    }
    Ok(snapshot)
}

fn rec_published_key(vault: &str, device: &str) -> String {
    format!("rec_published:{vault}:{device}")
}

fn rec_verified_key(vault: &str, device: &str) -> String {
    format!("rec_verified:{vault}:{device}")
}

/// SHA-256 (hex) of the bytes of the last successful own record put (D1b).
fn rec_put_sha_key(vault: &str, device: &str) -> String {
    format!("rec_put_sha:{vault}:{device}")
}

// ---------------------------------------------------------------------------
// Blobs (§4.2.3 blob rule, D5). Shared with vault J5 (re-upload under V2 ids).

/// How a blob upload settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobUpload {
    /// Created now.
    Uploaded,
    /// The name already held this exact content (HMAC matches): deduped.
    Deduped,
    /// The name holds something else (or nothing readable): fail closed,
    /// nothing was overwritten.
    Mismatch,
    /// Sealed, it would not fit one object (D5b): nothing was sent.
    Oversize,
}

/// Whether `bytes` are the content `blob_id` names (`HMAC(K_name, bytes)`).
fn blob_matches(name_key: &SecretKey, blob_id: &str, bytes: &[u8]) -> bool {
    hex(&hmac_sha256(name_key, bytes)) == blob_id
}

/// Uploads one blob create-only, sealed with `K_blob` under its object
/// address. `AlreadyExists` → every stored copy is decrypted and its HMAC
/// compared: all match → `Deduped`; otherwise `Mismatch` (fail closed).
/// A `blob_id` that does not name `bytes` is refused before any request; a
/// blob whose sealed object would reach [`MAX_OBJECT_BYTES`] is `Oversize`,
/// also unsent.
pub(crate) async fn upload_blob<P: ObjectStore>(
    p: &P,
    dek: &SecretKey,
    vault: &str,
    epoch: u32,
    blob_id: &str,
    bytes: &[u8],
) -> Result<BlobUpload, ProviderError> {
    let name_key = derive_subkey(dek, vault, epoch, Purpose::BlobName);
    if !blob_matches(&name_key, blob_id, bytes) {
        return Ok(BlobUpload::Mismatch);
    }
    let address = ObjectAddress::blob(vault, epoch, blob_id);
    let key = address.remote_key().ok_or_else(|| ProviderError::Permanent("invalid blob id".to_owned()))?;
    let blob_key = derive_subkey(dek, vault, epoch, Purpose::Blob);
    let sealed = seal(&blob_key, &address, bytes).map_err(|_| ProviderError::Permanent("blob seal failed".to_owned()))?;
    if sealed.len() >= MAX_OBJECT_BYTES {
        return Ok(BlobUpload::Oversize);
    }
    match p.put_create(&key, &sealed).await {
        Ok(_) => Ok(BlobUpload::Uploaded),
        Err(ProviderError::AlreadyExists) => {
            let copies = p.get_all(&key).await?;
            let ours = !copies.is_empty()
                && copies.iter().all(|c| {
                    open(&blob_key, &address, &c.bytes).is_ok_and(|plain| blob_matches(&name_key, blob_id, &plain))
                });
            Ok(if ours { BlobUpload::Deduped } else { BlobUpload::Mismatch })
        }
        Err(err) => Err(err),
    }
}

/// Downloads one blob, bounded by `cap` plaintext bytes. `Ok(None)` when it
/// is missing, larger than the cap, or does not decrypt to the content its
/// id names (the attachment then stays evicted).
pub(crate) async fn download_blob<P: ObjectStore>(
    p: &P,
    dek: &SecretKey,
    vault: &str,
    epoch: u32,
    blob_id: &str,
    cap: usize,
) -> Result<Option<Vec<u8>>, ProviderError> {
    let address = ObjectAddress::blob(vault, epoch, blob_id);
    let Some(key) = address.remote_key() else { return Ok(None) };
    let name_key = derive_subkey(dek, vault, epoch, Purpose::BlobName);
    let blob_key = derive_subkey(dek, vault, epoch, Purpose::Blob);
    for copy in p.get_all(&key).await? {
        if copy.bytes.len() > cap.saturating_add(41) {
            continue;
        }
        if let Ok(plain) = open(&blob_key, &address, &copy.bytes) {
            if plain.len() <= cap && blob_matches(&name_key, blob_id, &plain) {
                return Ok(Some(plain.to_vec()));
            }
        }
    }
    Ok(None)
}

fn resend_key(vault: &str, device: &str, first: u64) -> String {
    format!("resend:{vault}:{device}:{first}")
}

fn trigger_label(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::T1DeviceMismatch => "t1",
        Trigger::T2InstallChanged => "t2",
        Trigger::T3PartialRollback => "t3",
        Trigger::T4FullRollback => "t4",
        Trigger::T5SameIdentityClone => "t5",
        Trigger::T6NameTaken => "t6",
        Trigger::T7CheckpointRestore => "t7",
        Trigger::T8VaultSwitch => "t8",
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The class and retry policy of a provider failure (ADV-4, ADV-6).
pub(crate) fn classify(err: &ProviderError) -> FailureClass {
    match err {
        ProviderError::Auth { .. } => FailureClass::Auth,
        ProviderError::RateLimited { .. } => FailureClass::Rate,
        ProviderError::QuotaExceeded => FailureClass::Quota,
        ProviderError::BindingMismatch => FailureClass::Binding,
        ProviderError::Unsupported(_) => FailureClass::Unsupported,
        ProviderError::Transient(_) => FailureClass::Transient,
        ProviderError::Permanent(text) if retryable_permanent(text) => FailureClass::Transient,
        ProviderError::Permanent(_)
        | ProviderError::NotFound
        | ProviderError::AlreadyExists
        | ProviderError::PreconditionFailed => FailureClass::Permanent,
    }
}

/// HTTP 408 / 423 and R2 clock skew are worth retrying (ADV-6).
fn retryable_permanent(text: &str) -> bool {
    text == "HTTP 408" || text == "HTTP 423" || text.contains("RequestTimeTooSkewed")
}

/// Why a cycle step stopped the rest of the cycle.
#[derive(Debug)]
enum Halt {
    Provider(ProviderError),
    Store(StoreError),
    Secret,
    Stop(StopReason),
}

impl From<ProviderError> for Halt {
    fn from(err: ProviderError) -> Self {
        Halt::Provider(err)
    }
}

impl From<StoreError> for Halt {
    fn from(err: StoreError) -> Self {
        Halt::Store(err)
    }
}

impl From<SecretError> for Halt {
    fn from(_: SecretError) -> Self {
        Halt::Secret
    }
}

impl From<OpError> for Halt {
    fn from(err: OpError) -> Self {
        Halt::Store(StoreError::Op(err))
    }
}

/// Per-cycle bookkeeping.
#[derive(Default)]
struct Run {
    notices: BTreeSet<EngineNotice>,
    applied: usize,
    uploaded: usize,
    changed: bool,
    /// The upload step must not run (rotation deferred, outcome unknown).
    uploads_blocked: bool,
    /// An `attempted` segment is still unknown at the end of this cycle.
    unsettled: bool,
    /// A failure that did not stop the cycle but should back off.
    soft_failure: Option<FailureClass>,
    max_hlc: Option<Hlc>,
}

impl Run {
    fn notice(&mut self, notice: EngineNotice) {
        self.notices.insert(notice);
    }

    fn soft(&mut self, err: &ProviderError, stats: &SyncStats) {
        let class = classify(err);
        stats.failed(class);
        self.soft_failure.get_or_insert(class);
    }
}

struct Keys {
    dek: SecretKey,
    seg: SecretKey,
    snap: SecretKey,
}

enum Settle {
    Published,
    Rejected,
    Unknown,
}

impl Engine {
    /// One single-flight cycle (callers serialize; the scheduler runs cycles
    /// inline, so two never overlap).
    pub(crate) async fn cycle<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>) -> CycleReport
    where
        P: ObjectStore,
        S: SecretStore + Sync,
        Z: Pause + Sync,
    {
        let started = Instant::now();
        let mut run = Run::default();
        let result = run_cycle(ctx, &mut run).await;

        if let Some(max) = run.max_hlc.take() {
            // Store released: the clock comes last (lock order).
            lock(ctx.clock).observe(max, (ctx.now)());
        }
        if run.unsettled {
            ctx.engine.unsettled_cycles = ctx.engine.unsettled_cycles.saturating_add(1);
            if ctx.engine.unsettled_cycles >= A3_NOTICE_CYCLES {
                run.notice(EngineNotice::UploadOutcomeUnknown);
            }
        } else if result.is_ok() {
            ctx.engine.unsettled_cycles = 0;
        }

        let status = match result {
            Ok(()) => match run.soft_failure {
                Some(class) => CycleStatus::Failed { class },
                None => CycleStatus::Ok,
            },
            Err(halt) => halt_status(halt, ctx.stats, &mut run),
        };
        let error = match status {
            CycleStatus::Ok => None,
            CycleStatus::Stopped { reason } => reason.class(),
            CycleStatus::RateLimited { .. } => Some(FailureClass::Rate),
            CycleStatus::Failed { class } => Some(class),
        };
        let millis = started.elapsed().as_millis() as u64;
        ctx.stats.cycle_done(millis, error);
        log_event(
            "cycle",
            &[
                ("vault", Field::Vault(VaultTag::of(ctx.vault))),
                ("status", Field::Label(status_label(&status))),
                ("applied", Field::Num(run.applied as u64)),
                ("uploaded", Field::Num(run.uploaded as u64)),
                ("notices", Field::Num(run.notices.len() as u64)),
                ("ms", Field::Num(millis)),
            ],
        );
        CycleReport {
            status,
            notices: run.notices.into_iter().collect(),
            applied_segments: run.applied,
            uploaded_segments: run.uploaded,
            changed: run.changed,
        }
    }
}

fn status_label(status: &CycleStatus) -> &'static str {
    match status {
        CycleStatus::Ok => "ok",
        CycleStatus::Stopped { reason } => reason.label(),
        CycleStatus::RateLimited { .. } => "rate_limited",
        CycleStatus::Failed { class } => class.as_str(),
    }
}

fn halt_status(halt: Halt, stats: &SyncStats, run: &mut Run) -> CycleStatus {
    let stop = |reason: StopReason, run: &mut Run| {
        if let Some(class) = reason.class() {
            stats.failed(class);
        }
        if let Some(notice) = reason.notice() {
            run.notice(notice);
        }
        CycleStatus::Stopped { reason }
    };
    match halt {
        Halt::Stop(reason) => stop(reason, run),
        Halt::Secret => {
            stats.failed(FailureClass::Local);
            run.notice(EngineNotice::KeychainUnavailable);
            CycleStatus::Failed { class: FailureClass::Local }
        }
        Halt::Store(err) => {
            let kind = match err {
                StoreError::Sql(_) => "sql",
                StoreError::Op(_) => "op",
                StoreError::Rotation(_) => "rotation",
                StoreError::Corrupt(_) => "corrupt",
                StoreError::NoIdentity => "no_identity",
                StoreError::Invalid(_) => "invalid",
                StoreError::Io(_) => "io",
            };
            log_event("store_error", &[("kind", Field::Label(kind))]);
            stats.failed(FailureClass::Local);
            CycleStatus::Failed { class: FailureClass::Local }
        }
        Halt::Provider(err) => match err {
            ProviderError::Auth { reconsent: true } => stop(StopReason::Reconsent, run),
            ProviderError::Auth { reconsent: false } => stop(StopReason::PermissionDenied, run),
            ProviderError::BindingMismatch => stop(StopReason::BindingMismatch, run),
            ProviderError::Unsupported(_) => stop(StopReason::Unsupported, run),
            ProviderError::RateLimited { retry_after } => {
                stats.failed(FailureClass::Rate);
                run.notice(EngineNotice::RateLimited);
                CycleStatus::RateLimited { retry_after }
            }
            ProviderError::QuotaExceeded => {
                stats.failed(FailureClass::Quota);
                run.notice(EngineNotice::QuotaExceeded);
                CycleStatus::Failed { class: FailureClass::Quota }
            }
            other => {
                let class = classify(&other);
                stats.failed(class);
                CycleStatus::Failed { class }
            }
        },
    }
}

/// `list` where NotFound is a configuration error, never an empty listing
/// (ADV-9).
async fn list<P: ObjectStore>(p: &Bound<P>, q: ListQuery<'_>) -> Result<Vec<ListedObject>, Halt> {
    match p.list(q).await {
        Ok(listed) => Ok(listed),
        Err(ProviderError::NotFound) => Err(Halt::Stop(StopReason::Configuration)),
        Err(err) => Err(err.into()),
    }
}

async fn run_cycle<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    // 0. Gates.
    let wiping = {
        let store = lock(ctx.store);
        vault::wipe_in_progress(&store)? || store.meta_get(META_WIPE_STATE)?.as_deref() == Some("started")
    };
    if wiping {
        return Err(Halt::Stop(StopReason::WipeInProgress));
    }
    fence_gate(ctx)?;
    let dek = load_vault_key(ctx.secrets, vault)?.ok_or(Halt::Stop(StopReason::MissingVaultKey))?;
    let keys = Keys {
        seg: derive_subkey(&dek, vault, ctx.key_epoch, Purpose::Segment),
        snap: derive_subkey(&dek, vault, ctx.key_epoch, Purpose::Snapshot),
        dek,
    };
    let check = match vault::check_vaults(ctx.provider, vault).await {
        Ok(check) => check,
        Err(ProviderError::NotFound) => return Err(Halt::Stop(StopReason::Configuration)),
        Err(err) => return Err(err.into()),
    };
    if !check.manifest_present {
        // D7a: `check_vaults` read twice back to back; a miss counts only
        // after a read separated by the settle pause.
        manifest_gate(ctx).await?;
    }
    // D3a: several copies in two consecutive cycles → a notice, no delete.
    if check.manifest_copies > 1 {
        ctx.engine.manifest_copy_cycles = ctx.engine.manifest_copy_cycles.saturating_add(1);
        if ctx.engine.manifest_copy_cycles >= MANIFEST_COPY_CYCLES {
            run.notice(EngineNotice::ManifestCopies { count: check.manifest_copies });
            log_event("manifest_copies", &[("count", Field::Num(check.manifest_copies as u64))]);
        }
    } else {
        ctx.engine.manifest_copy_cycles = 0;
    }
    if !check.other_vaults.is_empty() {
        run.notice(EngineNotice::OtherVaults { count: check.other_vaults.len() });
    }

    // 1. Identity.
    identity_step(ctx, run, &keys).await?;
    // 2. Catch-up, then the attachments it referenced (bounded, D5).
    catch_up(ctx, run, &keys).await?;
    blob_download_step(ctx, run, &keys).await?;
    // 3. Upload.
    if !run.uploads_blocked {
        upload_step(ctx, run, &keys).await?;
    }
    // 4. Compaction, 5. record: only with an identity in good standing.
    if !run.uploads_blocked {
        compaction_step(ctx, run, &keys).await?;
        record_step(ctx, &keys).await?;
    }
    Ok(())
}

/// D6: the cycle's vault must still be the current one.
fn fence_gate<P, S, Z>(ctx: &CycleCtx<'_, P, S, Z>) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore,
    Z: Pause,
{
    match ctx.fence {
        Some(current) if !current() => Err(Halt::Stop(StopReason::VaultSwitched)),
        _ => Ok(()),
    }
}

/// Before every put batch: the D6 generation fence, then (D7) the own
/// manifest must still exist. D7a: a miss counts only when a second read,
/// separated by the settle pause, misses too (a lagging Drive name query is
/// no wipe). A confirmed miss stops the cycle; nothing is deleted remotely
/// and no local upload state changes (orphans are the wiper's final sweep).
async fn manifest_gate<P, S, Z>(ctx: &CycleCtx<'_, P, S, Z>) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    fence_gate(ctx)?;
    let key = vault::manifest_key(ctx.vault).ok_or(Halt::Stop(StopReason::Configuration))?;
    for attempt in 0..2 {
        if attempt > 0 {
            ctx.pause.pause(RECHECK_DELAYS[0]).await;
        }
        match ObjectStore::get_all(ctx.provider, &key).await {
            Ok(copies) if !copies.is_empty() => return fence_gate(ctx),
            Ok(_) | Err(ProviderError::NotFound) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err(Halt::Stop(StopReason::ManifestMissing))
}

/// What this device knows about its own published record (D1).
struct RecordPublication {
    /// Wall ms of the last successful put; `None` before the first publish.
    put_ms: Option<u64>,
    /// A read since that put returned exactly its bytes (D1b).
    verified: bool,
    /// Lowercase hex SHA-256 of the last put's bytes; `None` before D1b.
    put_sha: Option<String>,
}

impl RecordPublication {
    fn load(store: &Store, vault: &str, device: &str) -> Result<Self, StoreError> {
        // A pre-D1 value ("1") parses as a put at epoch 0: long settled.
        let put_ms = store.meta_get(&rec_published_key(vault, device))?.map(|v| v.parse().unwrap_or(0));
        let verified = store.meta_get(&rec_verified_key(vault, device))?.is_some_and(|v| v == "1");
        let put_sha = store.meta_get(&rec_put_sha_key(vault, device))?.filter(|v| !v.is_empty());
        Ok(Self { put_ms, verified, put_sha })
    }

    /// Two misses on a record in this state mean it was removed (ADV-7).
    fn removable(&self, now: u64) -> bool {
        self.put_ms.is_some_and(|at| self.verified || now.saturating_sub(at) >= SETTLE_WINDOW_MS)
    }

    /// D1b: `bytes` read back are exactly what the last put wrote. Without a
    /// recorded hash nothing verifies (the settle window still applies).
    fn read_matches_put(&self, bytes: &[u8]) -> bool {
        self.put_sha.as_deref().is_some_and(|sha| sha == hex(&Sha256::digest(bytes)))
    }
}

/// Reads `devices/<device>.rec`. `Ok(None)` when absent and not (yet) a
/// removal; a record this device published that is absent twice is a
/// removal ([`RecordPublication::removable`]). A read whose bytes hash to
/// the last put's marks that put verified (D1b).
async fn read_own_record<P, S, Z>(
    ctx: &CycleCtx<'_, P, S, Z>,
    keys: &Keys,
    device: &str,
) -> Result<Option<RemoteRecord>, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let key = ObjectAddress::device_record(vault, ctx.key_epoch, device)
        .remote_key()
        .ok_or(Halt::Stop(StopReason::IdentityLost))?;
    let publication = RecordPublication::load(&lock(ctx.store), vault, device)?;
    let published = publication.put_ms.is_some();
    let mut bytes = None;
    for attempt in 0..2 {
        match ctx.provider.get(&key).await {
            Ok(found) => {
                bytes = Some(found);
                break;
            }
            Err(ProviderError::NotFound) if published && attempt == 0 => ctx.pause.pause(RECHECK_DELAYS[0]).await,
            Err(ProviderError::NotFound) if publication.removable((ctx.now)()) => {
                return Err(Halt::Stop(StopReason::DeviceRemoved))
            }
            Err(ProviderError::NotFound) => return Ok(None),
            Err(err) => return Err(err.into()),
        }
    }
    let Some(bytes) = bytes else { return Ok(None) };
    match open_record(&keys.dek, vault, ctx.key_epoch, device, &bytes).ok().and_then(|r| r.remote()) {
        Some(record) => {
            if published && !publication.verified && publication.read_matches_put(&bytes) {
                lock(ctx.store).meta_set(&rec_verified_key(vault, device), "1")?;
            }
            Ok(Some(record))
        }
        None => {
            lock(ctx.store).quarantine_key(vault, &key, "own record does not open")?;
            ctx.stats.quarantined();
            Err(Halt::Stop(StopReason::RecordUnreadable))
        }
    }
}

/// Every copy under `log/<device>/` with a digest of its bytes (T5). Copies
/// are fetched once per provider_ref.
async fn own_remote_segments<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, device: &str) -> Result<Vec<RemoteSegment>, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let listed =
        list(ctx.provider, ListQuery::Props { vault_id: Some(ctx.vault), kind: Some(RemoteKind::Seg), device_id: Some(device) })
            .await?;
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for object in listed {
        if !ctx.engine.recently_deleted.contains(&object.key) {
            by_key.entry(object.key).or_default().push(object.provider_ref);
        }
    }
    let mut out = Vec::new();
    let mut live = BTreeSet::new();
    for (key, refs) in by_key {
        let Some(address) = ObjectAddress::from_remote_key(&key, ctx.key_epoch) else { continue };
        let Some(range) = segment_range(&address) else { continue };
        if refs.iter().any(|r| !ctx.engine.digests.contains_key(r)) {
            for copy in ObjectStore::get_all(ctx.provider, &key).await? {
                ctx.engine.digests.insert(copy.provider_ref.clone(), Sha256::digest(&copy.bytes).into());
            }
        }
        for r in &refs {
            // A copy listed but not returned by `get_all` (lag) proves nothing.
            if let Some(digest) = ctx.engine.digests.get(r) {
                out.push(RemoteSegment { range, digest: *digest });
                live.insert(r.clone());
            }
        }
    }
    ctx.engine.digests.retain(|r, _| live.contains(r));
    Ok(out)
}

fn segment_range(address: &ObjectAddress) -> Option<SeqRange> {
    if address.kind != ObjectKind::Segment {
        return None;
    }
    let (first, last) = address.object_ref.split_once('-')?;
    Some(SeqRange { first: first.parse().ok()?, last: last.parse().ok()? })
}

async fn identity_step<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let (local, checkpoint_restored) = {
        let store = lock(ctx.store);
        let local = match store.local_identity(vault) {
            Ok(local) => local,
            Err(StoreError::NoIdentity) => None,
            Err(err) => return Err(err.into()),
        };
        (local, store.meta_get(META_CHECKPOINT_RESTORED)?.is_some_and(|v| !v.is_empty()))
    };
    let keychain = identity::load_identity(ctx.secrets, vault)?;
    let Some(local) = local else {
        return Err(Halt::Stop(if keychain.is_some() { StopReason::IdentityLost } else { StopReason::NotJoined }));
    };
    let device = local.device_id.clone();
    let remote_record = read_own_record(ctx, keys, &device).await?;
    let local_segments: Vec<SeqRange> = lock(ctx.store)
        .pending_segments(vault, &device)?
        .into_iter()
        .map(|(first, last, _)| SeqRange { first, last })
        .collect();
    let remote_segments = own_remote_segments(ctx, &device).await?;
    let observation = Observation {
        keychain: keychain.as_ref(),
        local: Some(&local),
        current_install_fp: ctx.install_fp,
        remote_record,
        local_segments: &local_segments,
        remote_segments: &remote_segments,
        put_create_conflict: false,
        checkpoint_restored,
        vault_switched: false,
    };
    match identity::detect(&observation) {
        IdentityCheck::Fresh => Err(Halt::Stop(StopReason::NotJoined)),
        IdentityCheck::Healthy { adopt_generation } => {
            if let Some(generation) = adopt_generation {
                lock(ctx.store).set_generation(vault, generation)?;
            }
            Ok(())
        }
        IdentityCheck::Rotate(triggers) => rotate(ctx, run, keys, &device, &triggers).await,
    }
}

/// R1–R8 for `old` (uploads are already stopped: the caller has not
/// uploaded this cycle). W-2: every `attempted` segment is settled first; if
/// one stays unknown the rotation waits for a later cycle.
async fn rotate<P, S, Z>(
    ctx: &mut CycleCtx<'_, P, S, Z>,
    run: &mut Run,
    keys: &Keys,
    old: &str,
    triggers: &[Trigger],
) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    run.uploads_blocked = true;
    // R3 / W-2.
    // D6/D7: the settle may resend; gate it like any put batch.
    manifest_gate(ctx).await?;
    if settle_all(ctx, run, keys, old).await? {
        run.unsettled = true;
        return Ok(());
    }
    // R2: verified body hashes per seq under log/<old>/.
    let remote = remote_hashes(ctx, keys, old).await?;
    // R4: the new identity, keychain first.
    let new_identity = DeviceIdentity::new(vault, ctx.install_fp.map(str::to_owned)).ok_or(Halt::Secret)?;
    let source = lock(ctx.store).rotation_input(vault, remote.clone())?;
    let plan = match Store::plan_rotation(&source, &new_identity.device_id, || uuid::Uuid::now_v7().to_string()) {
        Ok(plan) => plan,
        Err(StoreError::Rotation(RotationError::UnsettledAttempts(_))) => {
            run.unsettled = true;
            return Ok(());
        }
        Err(other) => return Err(other.into()),
    };
    identity::save_identity(ctx.secrets, &new_identity)?;
    {
        let mut store = lock(ctx.store);
        store.apply_rotation(vault, &plan, &remote, &new_identity)?;
        if let Some(fp) = ctx.install_fp {
            store.meta_set("install_fp", fp)?;
        }
        store.meta_set(META_CHECKPOINT_RESTORED, "")?;
    }
    ctx.engine.last_record = None;
    ctx.stats.rotated();
    let labels: Vec<&'static str> = triggers.iter().map(|t| trigger_label(*t)).collect();
    log_event(
        "rotation",
        &[
            ("vault", Field::Vault(VaultTag::of(vault))),
            ("triggers", Field::Num(labels.len() as u64)),
            ("rewritten", Field::Num(plan.rewritten.len() as u64)),
            ("reissued", Field::Num(plan.reissued.len() as u64)),
        ],
    );
    run.notice(EngineNotice::Rotated { triggers: labels });
    run.changed = true;
    // R8: uploads resume next cycle, from the new id.
    Ok(())
}

/// Body hashes per seq of every readable copy under `log/<device>/`.
async fn remote_hashes<P, S, Z>(
    ctx: &CycleCtx<'_, P, S, Z>,
    keys: &Keys,
    device: &str,
) -> Result<BTreeMap<u64, Vec<[u8; 32]>>, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let listed =
        list(ctx.provider, ListQuery::Props { vault_id: Some(ctx.vault), kind: Some(RemoteKind::Seg), device_id: Some(device) })
            .await?;
    let keys_listed: BTreeSet<String> = listed.into_iter().map(|o| o.key).collect();
    let mut out: BTreeMap<u64, Vec<[u8; 32]>> = BTreeMap::new();
    for key in keys_listed {
        let Some(address) = ObjectAddress::from_remote_key(&key, ctx.key_epoch) else { continue };
        for copy in ObjectStore::get_all(ctx.provider, &key).await? {
            // An unreadable copy proves nothing about S; it is quarantined by
            // catch-up when the retired stream is read.
            let Ok(segment) = open_segment(&keys.seg, &address, &copy.bytes) else { continue };
            for op in &segment.ops {
                out.entry(op.seq).or_default().push(super::ops::body_hash(&op.body)?);
            }
        }
    }
    Ok(out)
}

/// Settles every `attempted` own segment of `device`. Returns whether one is
/// still unknown. A `Rejected` one is marked and quarantined here; the
/// caller's rotation reissues it.
async fn settle_all<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys, device: &str) -> Result<bool, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let attempted: Vec<(u64, u64)> = lock(ctx.store)
        .segment_states(ctx.vault, device)?
        .into_iter()
        .filter(|(_, _, state)| *state == UploadState::Attempted)
        .map(|(first, last, _)| (first, last))
        .collect();
    let mut unknown = false;
    for (first, last) in attempted {
        if let Settle::Unknown = settle_segment(ctx, run, keys, device, first, last, false).await? {
            unknown = true;
        }
    }
    Ok(unknown)
}

/// Resolves one `attempted` segment through `publish::settle_attempted`.
async fn settle_segment<P, S, Z>(
    ctx: &mut CycleCtx<'_, P, S, Z>,
    run: &mut Run,
    keys: &Keys,
    device: &str,
    first: u64,
    last: u64,
    just_created: bool,
) -> Result<Settle, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let address = ObjectAddress::segment(vault, ctx.key_epoch, device, first, last);
    let key = address.remote_key().ok_or(Halt::Stop(StopReason::IdentityLost))?;
    let (bytes, local_hash) = {
        let store = lock(ctx.store);
        let bytes = store
            .sealed_bytes(vault, device, first)?
            .map(|p| p.bytes)
            .ok_or_else(|| StoreError::Corrupt("attempted segment without sealed bytes".to_owned()))?;
        let own = open_segment(&keys.seg, &address, &bytes)
            .map_err(|_| StoreError::Corrupt("own sealed segment does not open".to_owned()))?;
        (bytes, segment_hash(&own)?)
    };
    if !ctx.provider.create_only() && !just_created {
        let meta = resend_key(vault, device, first);
        let mut store = lock(ctx.store);
        let count: u64 = store.meta_get(&meta)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if count >= MAX_DRIVE_RESENDS {
            run.notice(EngineNotice::ResendCapReached);
            run.unsettled = true;
            return Ok(Settle::Unknown);
        }
        store.meta_set(&meta, &(count + 1).to_string())?;
    }
    let judge = |remote: &[u8]| match open_segment(&keys.seg, &address, remote) {
        Ok(segment) => match segment_hash(&segment) {
            Ok(hash) if hash == local_hash => Verdict::Ours,
            Ok(_) => Verdict::Other,
            Err(_) => Verdict::Invalid,
        },
        Err(_) => Verdict::Invalid,
    };
    let settled = settle_attempted(ctx.provider, ctx.pause, &key, &bytes, judge).await;
    for extra in &settled.extra_self_copies {
        // Only byte-for-byte chain copies of ours (a raced Drive resend).
        ctx.provider.delete_ref(extra).await?;
    }
    if settled.invalid_seen {
        relist_invalid(ctx, run, keys, &address, &key).await?;
    }
    match settled.outcome {
        Outcome::Published | Outcome::PublishedWithFork => {
            {
                let mut store = lock(ctx.store);
                store.mark_published(vault, device, first)?;
                if !ctx.provider.create_only() {
                    store.meta_set(&resend_key(vault, device, first), "0")?;
                }
            }
            if !just_created {
                ctx.stats.uploaded(bytes.len());
            }
            if settled.outcome == Outcome::PublishedWithFork {
                lock(ctx.store).record_own_fork(vault, device, first)?;
                ctx.stats.forked();
                run.notice(EngineNotice::ForkDetected);
                reproject(ctx, run)?;
            }
            Ok(Settle::Published)
        }
        Outcome::Rejected => {
            {
                let mut store = lock(ctx.store);
                store.mark_rejected(vault, device, first)?;
                store.quarantine_key(vault, &key, "create-only name taken by another writer")?;
            }
            ctx.stats.rejected();
            ctx.stats.quarantined();
            run.notice(EngineNotice::UploadRejected);
            Ok(Settle::Rejected)
        }
        Outcome::StillAttempted => {
            run.unsettled = true;
            Ok(Settle::Unknown)
        }
    }
}

/// `invalid_seen`: relist the key and quarantine an unreadable copy that is
/// still there; a copy that vanished meanwhile is fine (amendment D3).
async fn relist_invalid<P, S, Z>(
    ctx: &CycleCtx<'_, P, S, Z>,
    run: &mut Run,
    keys: &Keys,
    address: &ObjectAddress,
    key: &str,
) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let copies = match ObjectStore::get_all(ctx.provider, key).await {
        Ok(copies) => copies,
        Err(ProviderError::NotFound) => Vec::new(),
        Err(err) => {
            // Unknown now; the next probe that sees it will relist again.
            run.soft(&err, ctx.stats);
            return Ok(());
        }
    };
    if copies.iter().any(|c| open_segment(&keys.seg, address, &c.bytes).is_err()) {
        lock(ctx.store).quarantine_key(ctx.vault, key, "unreadable object under own key")?;
        ctx.stats.quarantined();
        run.notice(EngineNotice::Quarantined);
    }
    Ok(())
}

fn reproject<P, S, Z>(ctx: &CycleCtx<'_, P, S, Z>, run: &mut Run) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let started = Instant::now();
    lock(ctx.store).reproject(ctx.vault)?;
    let millis = started.elapsed().as_millis() as u64;
    ctx.stats.reprojected(millis);
    run.notice(EngineNotice::Reprojected);
    run.changed = true;
    log_event("reproject", &[("vault", Field::Vault(VaultTag::of(ctx.vault))), ("ms", Field::Num(millis))]);
    Ok(())
}

/// One device's listed segments: (first, last, key) → listed copy refs.
type Stream = BTreeMap<(u64, u64, String), BTreeSet<String>>;

/// What applying snapshots and stream segments did (engine catch-up and
/// vault J2 share it, D4). Counts are events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CatchUp {
    /// Segments and snapshots that changed the op set.
    pub(crate) applied: usize,
    /// Streams that start past the cursor with no covering snapshot.
    pub(crate) gaps: usize,
    pub(crate) held: usize,
    /// Quarantined objects plus same-op-id/different-content conflicts.
    pub(crate) quarantined: usize,
    pub(crate) future_version: usize,
    /// Forks started.
    pub(crate) forked: usize,
}

/// Whether the next listed segment `first` lies past what a stream in
/// `state` has applied, with nothing (a snapshot) covering the hole.
fn stream_gap(state: Option<CursorState>, applied: u64, first: u64) -> bool {
    state.is_none_or(|s| s == CursorState::Ok) && first > applied.saturating_add(1)
}

/// Whether a listed segment is worth fetching for a stream in `state`.
/// `refs_changed`: a Drive name whose copy set changed since last fetched.
fn segment_wanted(state: Option<CursorState>, applied: u64, last: u64, copies: usize, refs_changed: bool) -> bool {
    match state {
        // Relisted in full (G3 amendment); duplicates are no-ops.
        Some(CursorState::Forked) | Some(CursorState::Retired) => true,
        // No automatic recovery from a simple break (v1); only a second
        // copy (a fork) can move it.
        Some(CursorState::Held) => copies > 1 && refs_changed,
        _ => last > applied || refs_changed,
    }
}

/// One name: a single copy applies normally; several distinct readable
/// copies are fork variants (§4.2.3 (a)); unreadable copies quarantine.
/// Returns the outcome and the variants that applied (for HLC / size
/// stats).
fn apply_name(
    store: &mut Store,
    seg_key: &SecretKey,
    vault: &str,
    device: &str,
    address: &ObjectAddress,
    copies: &[RemoteObject],
) -> Result<(ApplyOutcome, Vec<Segment>), StoreError> {
    let mut variants: Vec<Segment> = Vec::new();
    let mut last = ApplyOutcome::Duplicate;
    for copy in copies {
        match open_segment(seg_key, address, &copy.bytes) {
            Ok(segment) => {
                let hash = segment_hash(&segment)?;
                let mut known = false;
                for v in &variants {
                    known |= segment_hash(v)? == hash;
                }
                if !known {
                    variants.push(segment);
                }
            }
            // The store's own path quarantines / holds and reports it.
            Err(_) => last = store.apply_sealed(vault, seg_key, address, &copy.bytes)?,
        }
    }
    let Some(range) = segment_range(address) else { return Ok((last, Vec::new())) };
    let outcome = match variants.as_slice() {
        [] => return Ok((last, Vec::new())),
        [one] => store.apply_segment(vault, device, range.first, range.last, one)?,
        _ => store.apply_fork_variant(vault, device, range.first, range.last, &variants)?,
    };
    let applied = if matches!(outcome, ApplyOutcome::Applied { .. }) { variants } else { Vec::new() };
    Ok((outcome, applied))
}

/// How one applied name moves its stream: keep going (the cursor reached
/// `last`), skip it (deferred), or stop this stream for this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamStep {
    Advanced,
    Deferred,
    Stop,
}

/// Folds one [`apply_name`] outcome into `tally`.
fn stream_step(outcome: &ApplyOutcome, tally: &mut CatchUp) -> StreamStep {
    match outcome {
        ApplyOutcome::Applied { fork_started, conflicts, .. } => {
            tally.applied += 1;
            tally.quarantined += usize::from(*conflicts > 0);
            tally.forked += usize::from(*fork_started);
            StreamStep::Advanced
        }
        ApplyOutcome::Duplicate => StreamStep::Advanced,
        // A forked stream's child listed before its parent: retried.
        ApplyOutcome::Deferred { .. } => StreamStep::Deferred,
        ApplyOutcome::Held { .. } => {
            tally.held += 1;
            StreamStep::Stop
        }
        ApplyOutcome::FutureVersion { .. } => {
            tally.future_version += 1;
            StreamStep::Stop
        }
        ApplyOutcome::Quarantined { .. } => {
            tally.quarantined += 1;
            StreamStep::Stop
        }
        ApplyOutcome::Skipped(_) => StreamStep::Stop,
    }
}

/// Every copy of one snapshot name: verified (id = content hash, owner,
/// vault) and applied; an unverifiable copy is quarantined.
fn apply_snapshot_copies(
    store: &mut Store,
    snap_key: &SecretKey,
    vault: &str,
    key: &str,
    address: &ObjectAddress,
    copies: &[RemoteObject],
    tally: &mut CatchUp,
) -> Result<(), StoreError> {
    for copy in copies {
        match open_snapshot(snap_key, address, &copy.bytes) {
            Ok(snapshot) => {
                if let ApplyOutcome::Applied { fork_started, .. } = store.apply_snapshot(vault, &snapshot, key)? {
                    tally.applied += 1;
                    tally.forked += usize::from(fork_started);
                }
            }
            Err(OpError::FutureVersion(_)) => tally.future_version += 1,
            Err(_) => {
                store.quarantine_key(vault, key, "snapshot does not verify")?;
                tally.quarantined += 1;
            }
        }
    }
    Ok(())
}

/// The engine's view of a [`CatchUp`]: notices, stats, `changed`.
fn absorb(ctx_stats: &SyncStats, run: &mut Run, tally: &CatchUp) {
    run.applied += tally.applied;
    run.changed |= tally.applied > 0;
    for _ in 0..tally.quarantined {
        ctx_stats.quarantined();
    }
    for _ in 0..tally.forked {
        ctx_stats.forked();
    }
    if tally.quarantined > 0 {
        run.notice(EngineNotice::Quarantined);
    }
    if tally.forked > 0 {
        run.notice(EngineNotice::ForkDetected);
    }
    if tally.held > 0 {
        run.notice(EngineNotice::StreamHeld);
    }
    if tally.future_version > 0 {
        run.notice(EngineNotice::UpdateRequired);
    }
    if tally.gaps > 0 {
        run.notice(EngineNotice::StreamGap);
    }
}

/// Step 2: every other device's stream (and our own retired ids).
async fn catch_up<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let own = lock(ctx.store).device_id(vault)?;
    let listed =
        list(ctx.provider, ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Seg), device_id: None }).await?;
    let mut streams: BTreeMap<String, Stream> = BTreeMap::new();
    for object in listed {
        let Some(address) = ObjectAddress::from_remote_key(&object.key, ctx.key_epoch) else { continue };
        let Some(range) = segment_range(&address) else { continue };
        if address.device_id == own {
            continue;
        }
        streams
            .entry(address.device_id.clone())
            .or_default()
            .entry((range.first, range.last, object.key))
            .or_default()
            .insert(object.provider_ref);
    }
    // Snapshots first (§4.5): a compacted stream (or one compacted away
    // entirely) is only reachable through them. Applying one is idempotent.
    let mut tally = snapshot_catch_up(ctx, keys).await?;
    for (device, keyed) in streams {
        let cursor = lock(ctx.store).cursor(vault, &device)?;
        let state = cursor.as_ref().map(|c| c.state);
        let mut applied = cursor.as_ref().map_or(0, |c| c.applied_seq);
        for ((first, last, key), refs) in keyed {
            if stream_gap(state, applied, first) {
                // A gap: the snapshot covering it is not listed yet (lag).
                // Never hold the stream for that; retry next cycle.
                tally.gaps += 1;
                break;
            }
            let refs_changed = !ctx.provider.create_only() && ctx.engine.seen_refs.get(&key) != Some(&refs);
            if !segment_wanted(state, applied, last, refs.len(), refs_changed) {
                continue;
            }
            let copies = ObjectStore::get_all(ctx.provider, &key).await?;
            if copies.is_empty() {
                // Vanished (deleted after compaction, or list lag): tolerated.
                continue;
            }
            for copy in &copies {
                ctx.stats.downloaded(copy.bytes.len());
            }
            let Some(address) = ObjectAddress::from_remote_key(&key, ctx.key_epoch) else { continue };
            let (outcome, variants) = apply_name(&mut lock(ctx.store), &keys.seg, vault, &device, &address, &copies)?;
            for segment in &variants {
                for op in &segment.ops {
                    ctx.stats.op_sizes(op.body.ctx.len(), op.body.obs.len());
                    run.max_hlc = Some(run.max_hlc.map_or(op.body.hlc, |m| m.max(op.body.hlc)));
                }
            }
            match stream_step(&outcome, &mut tally) {
                StreamStep::Advanced => {
                    ctx.engine.seen_refs.insert(key, copies.iter().map(|c| c.provider_ref.clone()).collect());
                    applied = applied.max(last);
                }
                StreamStep::Deferred => {}
                StreamStep::Stop => break,
            }
        }
    }
    absorb(ctx.stats, run, &tally);
    if tally.forked > 0 {
        reproject(ctx, run)?;
    }
    Ok(())
}

/// Fetches and applies every snapshot not applied this session.
async fn snapshot_catch_up<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, keys: &Keys) -> Result<CatchUp, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let listed =
        list(ctx.provider, ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Snap), device_id: None }).await?;
    let wanted: BTreeSet<String> =
        listed.into_iter().map(|o| o.key).filter(|k| !ctx.engine.applied_snaps.contains(k)).collect();
    let mut tally = CatchUp::default();
    for key in wanted {
        let Some(address) = ObjectAddress::from_remote_key(&key, ctx.key_epoch) else { continue };
        let copies = ObjectStore::get_all(ctx.provider, &key).await?;
        for copy in &copies {
            ctx.stats.downloaded(copy.bytes.len());
        }
        apply_snapshot_copies(&mut lock(ctx.store), &keys.snap, vault, &key, &address, &copies, &mut tally)?;
        ctx.engine.applied_snaps.insert(key);
    }
    Ok(tally)
}

/// Step 2b (D5): attachment blobs that applied ops reference, at most
/// [`BLOB_FETCHES_PER_CYCLE`] per cycle. A blob that is missing, over
/// [`MAX_BLOB_BYTES`], or unverifiable stays evicted (no bytes) with a
/// notice; op apply never waits for it. A provider failure leaves the rest
/// for the next cycle (soft).
async fn blob_download_step<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    if run.applied > 0 || ctx.engine.blob_backlog.is_none() {
        ctx.engine.blob_backlog = Some(lock(ctx.store).missing_blobs(vault)?);
    }
    let Some(backlog) = ctx.engine.blob_backlog.as_mut() else { return Ok(()) };
    let take = backlog.len().min(BLOB_FETCHES_PER_CYCLE);
    let batch: Vec<String> = backlog.drain(..take).collect();
    for (i, blob_id) in batch.iter().enumerate() {
        match download_blob(ctx.provider, &keys.dek, vault, ctx.key_epoch, blob_id, MAX_BLOB_BYTES).await {
            Ok(Some(bytes)) => {
                ctx.stats.downloaded(bytes.len());
                lock(ctx.store).store_remote_blob(vault, blob_id, &bytes)?;
                run.changed = true;
            }
            Ok(None) => {
                lock(ctx.store).set_blob_state(vault, blob_id, BLOB_EVICTED)?;
                run.notice(EngineNotice::BlobEvicted);
            }
            Err(err @ (ProviderError::BindingMismatch | ProviderError::Auth { .. })) => return Err(err.into()),
            Err(err) => {
                run.soft(&err, ctx.stats);
                // Not fetched: back to the front of the backlog.
                if let Some(backlog) = ctx.engine.blob_backlog.as_mut() {
                    backlog.splice(0..0, batch[i..].iter().cloned());
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

/// One device's fetched segments: (first, last, key) → every copy.
type FetchedStream = BTreeMap<(u64, u64, String), Vec<RemoteObject>>;

/// Every snapshot and segment copy of a vault, fetched for J2 (D4).
pub(crate) struct VaultDownload {
    vault: String,
    epoch: u32,
    snaps: Vec<(String, Vec<RemoteObject>)>,
    streams: BTreeMap<String, FetchedStream>,
}

impl VaultDownload {
    /// No snapshot is listed and some stream does not start at seq 1 or is
    /// not contiguous: nothing could cover the hole (J2 fails before any
    /// apply).
    pub(crate) fn uncovered_gap(&self) -> bool {
        if !self.snaps.is_empty() {
            return false;
        }
        self.streams.values().any(|stream| {
            let mut next = 1u64;
            for &(first, last, _) in stream.keys() {
                if first != next || last < first {
                    return true;
                }
                next = last.saturating_add(1);
            }
            false
        })
    }
}

/// J2 download through the engine's listing rules: every snapshot and every
/// segment copy of `vault` (Props listings, so Drive uses appProperties).
/// Listed names that vanished before the fetch are skipped.
pub(crate) async fn fetch_vault<P: ObjectStore>(p: &P, vault: &str, epoch: u32) -> Result<VaultDownload, ProviderError> {
    let mut snap_keys = BTreeSet::new();
    for object in p.list(ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Snap), device_id: None }).await? {
        let parsed = ObjectAddress::from_remote_key(&object.key, epoch);
        if parsed.is_some_and(|a| a.kind == ObjectKind::Snapshot && a.vault_id == vault) {
            snap_keys.insert(object.key);
        }
    }
    let mut snaps = Vec::new();
    for key in snap_keys {
        let copies = p.get_all(&key).await?;
        if !copies.is_empty() {
            snaps.push((key, copies));
        }
    }
    let mut names: BTreeMap<String, BTreeSet<(u64, u64, String)>> = BTreeMap::new();
    for object in p.list(ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Seg), device_id: None }).await? {
        let Some(address) = ObjectAddress::from_remote_key(&object.key, epoch) else { continue };
        let Some(range) = segment_range(&address) else { continue };
        if address.vault_id != vault {
            continue;
        }
        names.entry(address.device_id).or_default().insert((range.first, range.last, object.key));
    }
    let mut streams = BTreeMap::new();
    for (device, keyed) in names {
        let mut stream = BTreeMap::new();
        for name in keyed {
            let copies = p.get_all(&name.2).await?;
            if !copies.is_empty() {
                stream.insert(name, copies);
            }
        }
        streams.insert(device, stream);
    }
    Ok(VaultDownload { vault: vault.to_owned(), epoch, snaps, streams })
}

/// J2 apply through the engine's apply path: snapshots first, then each
/// stream's tail past its cursor (a snapshot frontier included), into the
/// vault-scoped rows of `vault` (entities are persisted only for the active
/// vault, so a non-active target leaves the active one untouched). `own`
/// streams are skipped. A gap no snapshot covers stops that stream and is
/// counted.
pub(crate) fn apply_vault_download(
    store: &mut Store,
    dek: &SecretKey,
    vault: &str,
    epoch: u32,
    own: Option<&str>,
    download: &VaultDownload,
) -> Result<CatchUp, StoreError> {
    if download.vault != vault || download.epoch != epoch {
        return Err(StoreError::Invalid("download is for another vault".to_owned()));
    }
    let seg_key = derive_subkey(dek, vault, epoch, Purpose::Segment);
    let snap_key = derive_subkey(dek, vault, epoch, Purpose::Snapshot);
    let mut tally = CatchUp::default();
    for (key, copies) in &download.snaps {
        let Some(address) = ObjectAddress::from_remote_key(key, epoch) else { continue };
        apply_snapshot_copies(store, &snap_key, vault, key, &address, copies, &mut tally)?;
    }
    for (device, stream) in &download.streams {
        if own == Some(device.as_str()) {
            continue;
        }
        let cursor = store.cursor(vault, device)?;
        let state = cursor.as_ref().map(|c| c.state);
        let mut applied = cursor.as_ref().map_or(0, |c| c.applied_seq);
        for ((first, last, key), copies) in stream {
            if stream_gap(state, applied, *first) {
                tally.gaps += 1;
                break;
            }
            if !segment_wanted(state, applied, *last, copies.len(), false) {
                continue;
            }
            let Some(address) = ObjectAddress::from_remote_key(key, epoch) else { continue };
            let (outcome, _) = apply_name(store, &seg_key, vault, device, &address, copies)?;
            match stream_step(&outcome, &mut tally) {
                StreamStep::Advanced => applied = applied.max(*last),
                StreamStep::Deferred => {}
                StreamStep::Stop => break,
            }
        }
    }
    if tally.forked > 0 {
        store.reproject(vault)?;
    }
    Ok(tally)
}

/// Seals unsealed own ops into segments below [`SEGMENT_PLAIN_CAP`] (A4):
/// keychain generation first, then the sealed bytes (persisted before any
/// put), then SQLite's generation.
fn seal_new<P, S, Z>(ctx: &CycleCtx<'_, P, S, Z>, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let mut store = lock(ctx.store);
    let Some(local) = store.local_identity(vault)? else { return Ok(()) };
    let max_seq = store.frontier(vault)?.get(&local.device_id).copied().unwrap_or(0);
    if max_seq <= local.sealed_seq {
        return Ok(());
    }
    let keychain = identity::load_identity(ctx.secrets, vault)?.ok_or(Halt::Stop(StopReason::IdentityLost))?;
    if keychain.device_id != local.device_id {
        return Err(Halt::Stop(StopReason::IdentityLost));
    }
    let next = identity::advance_generation(ctx.secrets, &keychain)?;
    let epoch = ctx.key_epoch;
    let sealed = store.seal_pending_capped(vault, SEGMENT_PLAIN_CAP, |segment, plain| {
        let address = ObjectAddress::segment(vault, epoch, &segment.device_id, segment.first_seq, segment.last_seq);
        seal(&keys.seg, &address, plain)
    })?;
    if !sealed.is_empty() {
        store.set_generation(vault, next.generation)?;
    }
    Ok(())
}

/// A failed `put_create` of an upload: auth, binding, rate limit and quota
/// end the cycle with their status (Retry-After honoured, D10); anything else
/// is a soft failure that stops uploads for this cycle.
fn put_failure(err: ProviderError, run: &mut Run, stats: &SyncStats) -> Result<(), Halt> {
    match err {
        ProviderError::BindingMismatch
        | ProviderError::Auth { .. }
        | ProviderError::RateLimited { .. }
        | ProviderError::QuotaExceeded => Err(err.into()),
        err => {
            run.soft(&err, stats);
            Ok(())
        }
    }
}

/// Step 3a (D5): every pending blob of the vault, create-only, after seal
/// and before any segment. Returns false after a soft failure (no segment
/// goes out this cycle). A fail-closed mismatch (nothing overwritten,
/// BlobRejected) stays pending, which blocks exactly the segments that
/// reference it (D5a). A blob too large for one object is marked
/// `skipped_oversize` with one BlobEvicted notice and blocks nothing (D5b).
async fn blob_upload_step<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys) -> Result<bool, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let pending = lock(ctx.store).pending_blobs(vault)?;
    for (blob_id, bytes) in pending {
        let Some(key) = ObjectAddress::blob(vault, ctx.key_epoch, &blob_id).remote_key() else {
            // Not a valid name: it stays pending and holds its segments.
            continue;
        };
        match upload_blob(ctx.provider, &keys.dek, vault, ctx.key_epoch, &blob_id, &bytes).await {
            Ok(BlobUpload::Uploaded) => {
                ctx.stats.uploaded(bytes.len());
                lock(ctx.store).set_blob_state(vault, &blob_id, BLOB_PUBLISHED)?;
            }
            Ok(BlobUpload::Deduped) => {
                lock(ctx.store).set_blob_state(vault, &blob_id, BLOB_PUBLISHED)?;
            }
            Ok(BlobUpload::Mismatch) => {
                lock(ctx.store).quarantine_key(vault, &key, "blob name holds other content")?;
                ctx.stats.quarantined();
                ctx.stats.rejected();
                run.notice(EngineNotice::BlobRejected);
            }
            Ok(BlobUpload::Oversize) => {
                // Leaves `pending` for good, so the notice is raised once.
                lock(ctx.store).set_blob_state(vault, &blob_id, BLOB_SKIPPED_OVERSIZE)?;
                run.notice(EngineNotice::BlobEvicted);
                log_event("blob_skipped_oversize", &[]);
            }
            Err(err) => {
                put_failure(err, run, ctx.stats)?;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Step 3.
async fn upload_step<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let device = lock(ctx.store).device_id(vault)?;
    let attempted: Vec<(u64, u64)> = lock(ctx.store)
        .segment_states(vault, &device)?
        .into_iter()
        .filter(|(_, _, s)| *s == UploadState::Attempted)
        .map(|(f, l, _)| (f, l))
        .collect();
    let unsealed = {
        let store = lock(ctx.store);
        match store.local_identity(vault)? {
            Some(local) => store.frontier(vault)?.get(&local.device_id).copied().unwrap_or(0) > local.sealed_seq,
            None => false,
        }
    };
    let has_never = lock(ctx.store).segment_states(vault, &device)?.iter().any(|(_, _, s)| *s == UploadState::Never);
    let blobs_pending = !lock(ctx.store).pending_blob_ids(vault)?.is_empty();
    if attempted.is_empty() && !unsealed && !has_never && !blobs_pending {
        return Ok(());
    }
    // D7: the vault must still exist before this put batch.
    manifest_gate(ctx).await?;
    // Earlier unknown outcomes first; the chain cannot pass them. Their
    // blobs went up before their first put (D5a), and a resend carries the
    // same bytes.
    for (first, last) in attempted {
        match settle_segment(ctx, run, keys, &device, first, last, false).await? {
            Settle::Published => {}
            Settle::Rejected => return rotate(ctx, run, keys, &device, &[Trigger::T6NameTaken]).await,
            Settle::Unknown => return Ok(()),
        }
    }
    // D5a: seal first, then every pending blob (so every blob a sealed op
    // references has had its upload), and only then the segments.
    seal_new(ctx, keys)?;
    if !blob_upload_step(ctx, run, keys).await? {
        return Ok(());
    }
    let never: Vec<(u64, u64)> = lock(ctx.store)
        .segment_states(vault, &device)?
        .into_iter()
        .filter(|(_, _, s)| *s == UploadState::Never)
        .map(|(f, l, _)| (f, l))
        .collect();
    for (first, last) in never {
        // D5a: proven by data, not timing: a segment whose ops reference a
        // blob that is not up yet (failed closed, or added after this
        // cycle's blob step) is never put; the chain waits behind it.
        let blocked = {
            let store = lock(ctx.store);
            !store.segment_blob_ids(vault, &device, first, last)?.is_disjoint(&store.pending_blob_ids(vault)?)
        };
        if blocked {
            return Ok(());
        }
        let address = ObjectAddress::segment(vault, ctx.key_epoch, &device, first, last);
        let key = address.remote_key().ok_or(Halt::Stop(StopReason::IdentityLost))?;
        let bytes = lock(ctx.store)
            .sealed_bytes(vault, &device, first)?
            .map(|p| p.bytes)
            .ok_or_else(|| StoreError::Corrupt("sealed segment without bytes".to_owned()))?;
        if bytes.len() >= MAX_OBJECT_BYTES {
            // Nothing sent, nothing marked: the ops stay local (no loss).
            run.notice(EngineNotice::SegmentTooLarge);
            return Ok(());
        }
        lock(ctx.store).mark_attempted(vault, &device, first)?;
        let created = match ctx.provider.put_create(&key, &bytes).await {
            Ok(listed) => {
                lock(ctx.store).record_remote_object(
                    vault,
                    &key,
                    &listed.provider_ref,
                    listed.size,
                    listed.etag.as_deref(),
                )?;
                ctx.stats.uploaded(bytes.len());
                true
            }
            Err(ProviderError::AlreadyExists) => false,
            Err(err) => {
                // Outcome unknown: stays `attempted`, settled next cycle.
                run.unsettled = true;
                return put_failure(err, run, ctx.stats);
            }
        };
        match settle_segment(ctx, run, keys, &device, first, last, created).await? {
            Settle::Published => run.uploaded += 1,
            Settle::Rejected => return rotate(ctx, run, keys, &device, &[Trigger::T6NameTaken]).await,
            Settle::Unknown => return Ok(()),
        }
    }
    Ok(())
}

/// Step 4.
async fn compaction_step<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, run: &mut Run, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let now = (ctx.now)();
    let (snapshot, own) = {
        let store = lock(ctx.store);
        if !compaction::compaction_due(&store, vault, now)? {
            return Ok(());
        }
        let snapshot = compaction::build_snapshot(&store, vault)?;
        if !compaction::verify_snapshot(&store, vault, &snapshot)? {
            run.notice(EngineNotice::SnapshotVerifyFailed);
            return Ok(());
        }
        (snapshot, store.device_id(vault)?)
    };
    let plain = snapshot.encode()?;
    let snap_id = snapshot_id(&plain);
    let address = ObjectAddress::snapshot(vault, ctx.key_epoch, &own, &snap_id);
    let key = address.remote_key().ok_or(Halt::Stop(StopReason::IdentityLost))?;
    let sealed = seal(&keys.snap, &address, &plain).map_err(|_| StoreError::Corrupt("snapshot seal failed".to_owned()))?;
    if sealed.len() >= MAX_OBJECT_BYTES {
        run.notice(EngineNotice::SnapshotTooLarge);
        return Ok(());
    }
    // D7: the vault must still exist before this put batch.
    manifest_gate(ctx).await?;
    match ctx.provider.put_create(&key, &sealed).await {
        Ok(_) => ctx.stats.uploaded(sealed.len()),
        // Same name = same plaintext by construction; verified below.
        Err(ProviderError::AlreadyExists) => {}
        Err(err) => return Err(err.into()),
    }
    // Verify after write: every copy under the name must be ours.
    let copies = ObjectStore::get_all(ctx.provider, &key).await?;
    let verified = !copies.is_empty()
        && copies.iter().all(|c| open_snapshot(&keys.snap, &address, &c.bytes).is_ok_and(|s| s == snapshot));
    if !verified {
        run.notice(EngineNotice::SnapshotConflict);
        return Ok(());
    }
    let Some(cursors) = active_cursors(ctx, keys, &own, now).await? else { return Ok(()) };
    let deletable = {
        let store = lock(ctx.store);
        compaction::segments_deletable(&store, vault, &own, &cursors, true, &snapshot)?
    };
    let mut deleted = Vec::new();
    let mut failure = None;
    fence_gate(ctx)?;
    for (first, last) in &deletable {
        let Some(seg_key) = ObjectAddress::segment(vault, ctx.key_epoch, &own, *first, *last).remote_key() else {
            continue;
        };
        match ctx.provider.delete(&seg_key).await {
            Ok(()) => {
                ctx.engine.recently_deleted.insert(seg_key);
                deleted.push((*first, *last));
            }
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    if !deleted.is_empty() {
        lock(ctx.store).forget_segments(vault, &own, &deleted)?;
    }
    if let Some(err) = failure {
        return Err(err.into());
    }
    if let Some(dir) = ctx.checkpoint_dir {
        compaction::checkpoint(&mut lock(ctx.store), dir)?;
    }
    Ok(())
}

/// How far each active device has read our stream; `None` (delete nothing)
/// when any record cannot be read.
async fn active_cursors<P, S, Z>(
    ctx: &CycleCtx<'_, P, S, Z>,
    keys: &Keys,
    own: &str,
    now: u64,
) -> Result<Option<Vec<u64>>, Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let listed =
        list(ctx.provider, ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Rec), device_id: None }).await?;
    let mut cursors = Vec::new();
    for object in listed {
        let Some(address) = ObjectAddress::from_remote_key(&object.key, ctx.key_epoch) else { return Ok(None) };
        if address.device_id == own {
            continue;
        }
        let bytes = match ctx.provider.get(&object.key).await {
            Ok(bytes) => bytes,
            Err(ProviderError::NotFound) => continue,
            Err(err) => return Err(err.into()),
        };
        let Ok(record) = open_record(&keys.dek, vault, ctx.key_epoch, &address.device_id, &bytes) else {
            return Ok(None);
        };
        if record.last_seen_hlc.wall_ms.saturating_add(ACTIVE_DEVICE_MS) < now {
            continue;
        }
        cursors.push(record.frontier.get(own).copied().unwrap_or(0));
    }
    Ok(Some(cursors))
}

/// Step 5: `devices/<self>.rec`, written when its content changed (D1).
/// First publish of a device id: no precondition. Afterwards only a conditional replace of a
/// record that is present: create-only stores replace with If-Match on the
/// listed etag; Drive updates exactly the file id it just read
/// (`put_replace_ref`, D1a). An absent published record is never recreated;
/// it is re-read, and two misses past the settle window stop the cycle as
/// `DeviceRemoved`.
async fn record_step<P, S, Z>(ctx: &mut CycleCtx<'_, P, S, Z>, keys: &Keys) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let vault = ctx.vault;
    let (local, frontier) = {
        let store = lock(ctx.store);
        let Some(local) = store.local_identity(vault)? else { return Ok(()) };
        (local, store.frontier(vault)?)
    };
    let mut record = DeviceRecord {
        v: 1,
        device_id: local.device_id.clone(),
        label: ctx.device_label.to_owned(),
        last_seq: local.sealed_seq,
        chain_head: hex(&local.chain_head),
        frontier,
        last_seen_hlc: Hlc::default(),
        app_version: ctx.app_version.to_owned(),
    };
    let digest: [u8; 32] = {
        let value = serde_json::to_value(&record).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        Sha256::digest(canonical_json(&value)?).into()
    };
    if ctx.engine.last_record == Some(digest) {
        return Ok(());
    }
    record.last_seen_hlc = Hlc { wall_ms: (ctx.now)(), ctr: 0 };
    let bytes = seal_record(&keys.dek, vault, ctx.key_epoch, &record)
        .map_err(|_| StoreError::Corrupt("record seal failed".to_owned()))?;
    let key = ObjectAddress::device_record(vault, ctx.key_epoch, &local.device_id)
        .remote_key()
        .ok_or(Halt::Stop(StopReason::IdentityLost))?;
    let published = RecordPublication::load(&lock(ctx.store), vault, &local.device_id)?.put_ms.is_some();
    // D7: the vault must still exist before the record put.
    manifest_gate(ctx).await?;
    let written = if !published {
        // First publish of this device id: the record is a replaceable
        // object, never create-only, so it is written with no precondition
        // (a copy left by a put whose response was lost is simply replaced).
        Some(ctx.provider.put_replace(&key, &bytes, None).await?)
    } else {
        // The observed object: its etag (keyed stores) or its file id
        // (Drive, which has no etag precondition, G6).
        let observed = if ctx.provider.create_only() {
            let listed = list(
                ctx.provider,
                ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Rec), device_id: Some(&local.device_id) },
            )
            .await?;
            listed.into_iter().find(|o| o.key == key).and_then(|o| o.etag)
        } else {
            ObjectStore::get_all(ctx.provider, &key).await?.first().map(|c| c.provider_ref.clone())
        };
        let Some(observed) = observed else {
            // Absent (or the listing lags): never recreated. Two misses past
            // the settle window stop the cycle as DeviceRemoved.
            let _ = read_own_record(ctx, keys, &local.device_id).await?;
            return Ok(());
        };
        let put = if ctx.provider.create_only() {
            ctx.provider.put_replace(&key, &bytes, Some(&observed)).await
        } else {
            // D1a: a media update of exactly the file just read; gone → 404
            // (NotFound). Never a re-list, never a create.
            ctx.provider.put_replace_ref(&key, &observed, &bytes).await
        };
        match put {
            Ok(listed) => Some(listed),
            Err(ProviderError::PreconditionFailed) | Err(ProviderError::NotFound) => {
                // Re-read; a T4 condition rotates in the next cycle's step 1,
                // a removal stops now (D1 settle guard), anything else
                // retries with a fresh observation.
                let _ = read_own_record(ctx, keys, &local.device_id).await?;
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        }
    };
    if written.is_some() {
        mark_record_put(ctx, vault, &local.device_id, &bytes)?;
        ctx.engine.last_record = Some(digest);
        ctx.stats.uploaded(bytes.len());
    }
    Ok(())
}

/// A record put of `bytes` succeeded now: not yet read back (D1b keeps the
/// hash a later read must match).
fn mark_record_put<P, S, Z>(ctx: &CycleCtx<'_, P, S, Z>, vault: &str, device: &str, bytes: &[u8]) -> Result<(), Halt>
where
    P: ObjectStore,
    S: SecretStore + Sync,
    Z: Pause + Sync,
{
    let mut store = lock(ctx.store);
    store.meta_set(&rec_published_key(vault, device), &(ctx.now)().to_string())?;
    store.meta_set(&rec_put_sha_key(vault, device), &hex(&Sha256::digest(bytes)))?;
    store.meta_set(&rec_verified_key(vault, device), "0")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scheduler: 5 s debounce, 5 min period, single-flight, jittered backoff
// capped at 30 min, Retry-After honoured, cancellable. `next_delay` is the
// pure decision; `run_scheduler` drives it.

pub(crate) const DEBOUNCE: Duration = Duration::from_secs(5);
pub(crate) const PERIOD: Duration = Duration::from_secs(5 * 60);
pub(crate) const BACKOFF_BASE: Duration = Duration::from_secs(5);
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(30 * 60);
/// A `Retry-After` beyond this is treated as this (a broken header must not
/// park sync for days); shorter ones are honoured exactly.
pub(crate) const RETRY_AFTER_CAP: Duration = Duration::from_secs(24 * 60 * 60);
/// Local changes keep pushing the debounce back, but never past this from
/// the first pending change (A3 of the G7 review, D2).
pub(crate) const DEBOUNCE_MAX_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SchedState {
    /// Consecutive failed cycles (backoff exponent).
    pub(crate) failures: u32,
    pub(crate) running: bool,
    /// A local change arrived while a cycle ran.
    pub(crate) dirty: bool,
    /// Waiting for the user (reconsent, rejoin, ...): only `Resume` runs.
    pub(crate) stopped: bool,
    /// The armed wait is a backoff or a Retry-After: a local change must not
    /// shorten it (D2).
    pub(crate) backing_off: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleOutcome {
    Success,
    Retry,
    RateLimited(Option<Duration>),
    Stop,
}

impl CycleOutcome {
    pub(crate) fn of(report: &CycleReport) -> Self {
        match report.status {
            CycleStatus::Ok => CycleOutcome::Success,
            CycleStatus::Failed { .. } => CycleOutcome::Retry,
            CycleStatus::RateLimited { retry_after } => CycleOutcome::RateLimited(retry_after),
            CycleStatus::Stopped { .. } => CycleOutcome::Stop,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedEvent {
    LocalChange,
    Timer,
    CycleDone(CycleOutcome),
    /// The user fixed what stopped sync (or asked for "sync now").
    Resume,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Run a cycle now (the state is already `running`).
    RunNow,
    /// Arm the timer for this long.
    Wait(Duration),
    /// Arm the timer for at least this long: an earlier armed deadline is
    /// pushed out to it, a later one stays (deadline = max(existing,
    /// now + d), D2).
    AtLeast(Duration),
    /// Keep whatever timer is armed (or none).
    Idle,
    Exit,
}

/// Jittered exponential backoff in `[d/2, d]`, `d = min(base·2^(n−1), cap)`.
/// `rng` yields a value in `[0, 1)`.
pub(crate) fn backoff(failures: u32, rng: &mut impl FnMut() -> f64) -> Duration {
    let exp = BACKOFF_BASE.saturating_mul(1u32 << failures.saturating_sub(1).min(20));
    let full = exp.min(BACKOFF_CAP);
    let jitter = rng().clamp(0.0, 1.0);
    full / 2 + (full / 2).mul_f64(jitter)
}

/// The scheduler's pure decision function.
pub(crate) fn next_delay(state: &mut SchedState, event: SchedEvent, rng: &mut impl FnMut() -> f64) -> Decision {
    match event {
        SchedEvent::Cancel => Decision::Exit,
        SchedEvent::LocalChange if state.running => {
            state.dirty = true;
            Decision::Idle
        }
        SchedEvent::LocalChange if state.stopped => Decision::Idle,
        // Never shortens an armed backoff / Retry-After (D2).
        SchedEvent::LocalChange if state.backing_off => Decision::AtLeast(DEBOUNCE),
        SchedEvent::LocalChange => Decision::Wait(DEBOUNCE),
        SchedEvent::Timer if state.running || state.stopped => Decision::Idle,
        SchedEvent::Timer => {
            state.running = true;
            state.dirty = false;
            state.backing_off = false;
            Decision::RunNow
        }
        SchedEvent::Resume => {
            state.stopped = false;
            state.failures = 0;
            state.backing_off = false;
            if state.running {
                state.dirty = true;
                return Decision::Idle;
            }
            state.running = true;
            Decision::RunNow
        }
        SchedEvent::CycleDone(outcome) => {
            state.running = false;
            let dirty = std::mem::take(&mut state.dirty);
            match outcome {
                CycleOutcome::Success => {
                    state.failures = 0;
                    state.backing_off = false;
                    Decision::Wait(if dirty { DEBOUNCE } else { PERIOD })
                }
                CycleOutcome::Retry | CycleOutcome::RateLimited(None) => {
                    state.failures = state.failures.saturating_add(1);
                    state.backing_off = true;
                    Decision::Wait(backoff(state.failures, rng))
                }
                CycleOutcome::RateLimited(Some(after)) => {
                    state.failures = state.failures.saturating_add(1);
                    state.backing_off = true;
                    Decision::Wait(after.min(RETRY_AFTER_CAP))
                }
                CycleOutcome::Stop => {
                    state.stopped = true;
                    state.backing_off = false;
                    Decision::Idle
                }
            }
        }
    }
}

/// Uniform `[0, 1)` from the OS RNG (jitter only; no secret derives from it).
pub(crate) fn os_jitter() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

/// Where a debounce wait of `delay` started at `now` ends: never later than
/// `max_wait` after the first pending change (`pending_since`).
pub(crate) fn debounce_deadline<T>(now: T, pending_since: Option<T>, delay: Duration, max_wait: Duration) -> T
where
    T: Copy + Ord + std::ops::Add<Duration, Output = T>,
{
    let at = now + delay;
    match pending_since {
        Some(first) => at.min(first + max_wait),
        None => at,
    }
}

/// D2a: where a local change that arrives while backing off moves the
/// timer: `max(backoff, min(now + delay, first + max_wait))`. The backoff /
/// Retry-After is never shortened, and the debounce slide is capped at
/// max-wait from the first pending change, so edits never starve sync past
/// `max(backoff, max-wait)`.
pub(crate) fn backoff_change_deadline<T>(now: T, backoff: Option<T>, first: T, delay: Duration, max_wait: Duration) -> T
where
    T: Copy + Ord + std::ops::Add<Duration, Output = T>,
{
    let debounce = debounce_deadline(now, Some(first), delay, max_wait);
    backoff.map_or(debounce, |floor| floor.max(debounce))
}

/// Drives cycles until `Cancel` or the sender drops. Cycles run inline, so
/// they never overlap (single-flight); events that arrive meanwhile are
/// handled while the cycle runs, and `Cancel` drops the cycle at its current
/// await point (settle pauses included, D2). The first cycle runs
/// immediately.
pub(crate) async fn run_scheduler<F, Fut>(
    events: tokio::sync::mpsc::UnboundedReceiver<SchedEvent>,
    cycle: F,
    rng: impl FnMut() -> f64,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = CycleOutcome>,
{
    run_scheduler_with(events, cycle, rng, DEBOUNCE_MAX_WAIT).await;
}

async fn run_scheduler_with<F, Fut>(
    mut events: tokio::sync::mpsc::UnboundedReceiver<SchedEvent>,
    mut cycle: F,
    mut rng: impl FnMut() -> f64,
    max_wait: Duration,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = CycleOutcome>,
{
    use tokio::time::Instant;
    let mut state = SchedState::default();
    let mut decision = next_delay(&mut state, SchedEvent::Timer, &mut rng);
    let mut deadline: Option<Instant> = None;
    // First local change not yet covered by a cycle (debounce max-wait).
    let mut pending_since: Option<Instant> = None;
    // The armed backoff / Retry-After deadline (D2a): the floor a local
    // change's debounce may never undercut.
    let mut backoff_until: Option<Instant> = None;
    loop {
        match decision {
            Decision::Exit => return,
            Decision::RunNow => {
                deadline = None;
                pending_since = None;
                backoff_until = None;
                let running = cycle();
                tokio::pin!(running);
                let outcome = loop {
                    tokio::select! {
                        biased;
                        outcome = &mut running => break Some(outcome),
                        event = events.recv() => {
                            let event = event.unwrap_or(SchedEvent::Cancel);
                            if event == SchedEvent::LocalChange && pending_since.is_none() {
                                pending_since = Some(Instant::now());
                            }
                            if next_delay(&mut state, event, &mut rng) == Decision::Exit {
                                break None;
                            }
                        }
                    }
                };
                let Some(outcome) = outcome else { return };
                decision = next_delay(&mut state, SchedEvent::CycleDone(outcome), &mut rng);
                continue;
            }
            Decision::Wait(delay) => {
                let at = Instant::now() + delay;
                deadline = Some(at);
                // A Wait while backing off is the backoff itself (only a
                // finished cycle arms one); any other Wait ends it.
                backoff_until = state.backing_off.then_some(at);
            }
            Decision::AtLeast(delay) => {
                let now = Instant::now();
                let first = *pending_since.get_or_insert(now);
                deadline = Some(backoff_change_deadline(now, backoff_until, first, delay, max_wait));
            }
            Decision::Idle => {}
        }
        let event = match deadline {
            Some(at) => tokio::select! {
                event = events.recv() => event.unwrap_or(SchedEvent::Cancel),
                () = tokio::time::sleep_until(at) => SchedEvent::Timer,
            },
            None => events.recv().await.unwrap_or(SchedEvent::Cancel),
        };
        if event == SchedEvent::Timer {
            deadline = None;
        }
        decision = next_delay(&mut state, event, &mut rng);
        if event == SchedEvent::LocalChange {
            if let Decision::Wait(delay) = decision {
                // A plain debounce: pushed back by each change, but capped
                // at max-wait from the first pending one.
                let now = Instant::now();
                let first = *pending_since.get_or_insert(now);
                decision = Decision::Wait(debounce_deadline(now, Some(first), delay, max_wait).saturating_duration_since(now));
            } else if pending_since.is_none() && !matches!(decision, Decision::Exit) {
                pending_since = Some(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::SecretKey;
    use crate::sync::merge::{conflict_copy_id, EntityState, MemoState, Notice};
    use crate::sync::ops::{Kind, OpBody};
    use crate::sync::provider::fake::{FakeStore, Op};
    use crate::sync::provider::{binding_fp, Quota};
    use crate::sync::secrets::{store_vault_key, MemoryStore};
    use crate::sync::sim_tests::{create, delete, edit, oid, op, TempDir, M};
    use crate::sync::store::{now_ms, BlobRow};
    use crate::sync::telemetry::{capture, export_diagnostics};
    use std::sync::Arc;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const VAULT: &str = "vault-g7";
    const EPOCH: u32 = 1;
    const DEK: [u8; 32] = [9; 32];

    struct NoPause;

    impl Pause for NoPause {
        async fn pause(&self, _: Duration) {}
    }

    /// A per-reader view of one shared [`FakeStore`]: hides keys from
    /// listings (list lag), hides copies (per-reader Drive lag), serves stale
    /// bytes for a name, makes a copy vanish on the n-th `get_all`, or drops
    /// every segment put (a Drive that never shows the upload).
    #[derive(Default)]
    struct Hides {
        list_prefixes: Mutex<Vec<String>>,
        refs: Mutex<BTreeSet<String>>,
        stale: Mutex<BTreeMap<String, Vec<u8>>>,
        vanish: Mutex<Option<(String, String, usize)>>,
        get_alls: Mutex<BTreeMap<String, usize>>,
        black_hole: AtomicBool,
        puts: AtomicUsize,
        /// The next N `get_all` reads of a manifest come back empty (a
        /// lagging Drive name query, D7a).
        manifest_misses: AtomicUsize,
        /// The next `put_replace_ref` finds its copy removed just before it
        /// lands (a removal racing the record update, D1a).
        vanish_on_replace_ref: AtomicBool,
    }

    struct View<'a> {
        inner: &'a FakeStore,
        hides: &'a Hides,
    }

    impl Hides {
        fn hide_list(&self, prefix: &str) {
            lock(&self.list_prefixes).push(prefix.to_owned());
        }

        fn clear(&self) {
            lock(&self.list_prefixes).clear();
            lock(&self.refs).clear();
            lock(&self.stale).clear();
        }

        fn listed_hidden(&self, o: &ListedObject) -> bool {
            lock(&self.list_prefixes).iter().any(|p| o.key.starts_with(p.as_str())) || lock(&self.refs).contains(&o.provider_ref)
        }

        fn holed(&self, key: &str) -> bool {
            self.black_hole.load(Ordering::SeqCst) && key.contains("/log/")
        }
    }

    impl ObjectStore for View<'_> {
        fn create_only(&self) -> bool {
            self.inner.create_only()
        }

        fn identity(&self) -> String {
            self.inner.identity()
        }

        async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
            let listed = self.inner.list(q).await?;
            Ok(listed.into_iter().filter(|o| !self.hides.listed_hidden(o)).collect())
        }

        async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
            let stale = lock(&self.hides.stale).get(key).cloned();
            match stale {
                Some(bytes) => Ok(bytes),
                None => self.inner.get(key).await,
            }
        }

        async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
            if key.contains("/log/") {
                self.hides.puts.fetch_add(1, Ordering::SeqCst);
            }
            if self.hides.holed(key) {
                return Err(ProviderError::Transient("connection reset".to_owned()));
            }
            self.inner.put_create(key, bytes).await
        }

        async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
            self.inner.put_replace(key, bytes, if_match).await
        }

        async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
            if self.hides.vanish_on_replace_ref.swap(false, Ordering::SeqCst) {
                self.inner.delete_ref(provider_ref).await?;
            }
            self.inner.put_replace_ref(key, provider_ref, bytes).await
        }

        async fn delete(&self, key: &str) -> Result<(), ProviderError> {
            self.inner.delete(key).await
        }

        async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
            if self.hides.holed(key) {
                return Ok(Vec::new());
            }
            if key.ends_with("/manifest.json")
                && self.hides.manifest_misses.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
            {
                return Ok(Vec::new());
            }
            let call = {
                let mut counts = lock(&self.hides.get_alls);
                let n = counts.entry(key.to_owned()).or_default();
                *n += 1;
                *n
            };
            let vanish = lock(&self.hides.vanish).clone();
            if let Some((k, r, at)) = vanish {
                if k == key && at == call {
                    self.inner.delete_ref(&r).await?;
                }
            }
            let all = self.inner.get_all(key).await?;
            let hidden = lock(&self.hides.refs).clone();
            Ok(all.into_iter().filter(|c| !hidden.contains(&c.provider_ref)).collect())
        }

        async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
            self.inner.delete_ref(provider_ref).await
        }

        async fn quota(&self) -> Result<Quota, ProviderError> {
            self.inner.quota().await
        }

        async fn ensure_container(&self) -> Result<(), ProviderError> {
            self.inner.ensure_container().await
        }
    }

    fn bound<'a>(inner: &'a FakeStore, hides: &'a Hides) -> Bound<View<'a>> {
        Bound::new(View { inner, hides }, binding_fp(&inner.identity()))
    }

    fn remote(create_only: bool) -> FakeStore {
        remote_for(create_only, VAULT)
    }

    fn remote_for(create_only: bool, vault: &str) -> FakeStore {
        let fake = FakeStore::new(create_only);
        fake.seed(&vault::manifest_key(vault).unwrap(), b"{\"manifest\":true}");
        fake
    }

    fn real_now() -> u64 {
        now_ms()
    }

    struct Dev {
        store: Mutex<Store>,
        clock: Mutex<HlcClock>,
        secrets: MemoryStore,
        engine: Engine,
        stats: SyncStats,
        vault: String,
        install_fp: Option<String>,
        /// Added to the wall clock this device's engine sees.
        clock_offset_ms: u64,
    }

    impl Dev {
        fn new(device: &str) -> Dev {
            Dev::in_vault(VAULT, device, DEK)
        }

        fn in_vault(vault: &str, device: &str, dek: [u8; 32]) -> Dev {
            let secrets = MemoryStore::default();
            store_vault_key(&secrets, vault, &SecretKey::from_bytes(dek)).unwrap();
            let identity = DeviceIdentity {
                v: 1,
                vault_id: vault.to_owned(),
                device_id: device.to_owned(),
                install_fp: None,
                generation: 0,
            };
            identity::save_identity(&secrets, &identity).unwrap();
            let mut store = Store::open_in_memory().unwrap();
            store.init_identity(&identity).unwrap();
            store.set_active_vault(vault).unwrap();
            Dev {
                store: Mutex::new(store),
                clock: Mutex::new(HlcClock::default()),
                secrets,
                engine: Engine::default(),
                stats: SyncStats::default(),
                vault: vault.to_owned(),
                install_fp: None,
                clock_offset_ms: 0,
            }
        }

        /// A VM / backup clone: same database bytes, same keychain.
        fn clone_into(&self, dir: &TempDir) -> Dev {
            let path = dir.0.join("clone.db");
            lock(&self.store).conn().execute("VACUUM INTO ?1", [path.to_string_lossy().into_owned()]).unwrap();
            let secrets = MemoryStore::default();
            for account in self.secrets.accounts() {
                secrets.put_raw(&account, &self.secrets.raw(&account).unwrap());
            }
            Dev {
                store: Mutex::new(Store::open(&path).unwrap()),
                clock: Mutex::new(HlcClock::default()),
                secrets,
                engine: Engine::default(),
                stats: SyncStats::default(),
                vault: self.vault.clone(),
                install_fp: self.install_fp.clone(),
                clock_offset_ms: self.clock_offset_ms,
            }
        }

        async fn cycle<P: ObjectStore>(&mut self, provider: &Bound<P>) -> CycleReport {
            self.cycle_with_pause(provider, &NoPause).await
        }

        async fn cycle_with_pause<P: ObjectStore, Z: Pause + Sync>(&mut self, provider: &Bound<P>, pause: &Z) -> CycleReport {
            let offset = self.clock_offset_ms;
            let now = move || real_now() + offset;
            let mut ctx = CycleCtx {
                provider,
                secrets: &self.secrets,
                store: &self.store,
                clock: &self.clock,
                pause,
                now: &now,
                stats: &self.stats,
                engine: &mut self.engine,
                vault: &self.vault,
                key_epoch: EPOCH,
                install_fp: self.install_fp.as_deref(),
                device_label: "test device",
                app_version: "0.0.0-test",
                checkpoint_dir: None,
                fence: None,
            };
            Engine::cycle(&mut ctx).await
        }

        fn local(&self, body: OpBody) {
            lock(&self.store).insert_local_op(&self.vault, body, UploadState::Never).unwrap();
        }

        fn device(&self) -> String {
            lock(&self.store).device_id(&self.vault).unwrap()
        }

        fn hash(&self) -> [u8; 32] {
            lock(&self.store).projection(&self.vault).unwrap().hash
        }

        fn entity(&self, kind: Kind, id: &str) -> Option<EntityState> {
            lock(&self.store).projection(&self.vault).unwrap().entities.get(&(kind, id.to_owned())).cloned()
        }

        fn memo(&self, id: &str) -> MemoState {
            match self.entity(Kind::Memo, id) {
                Some(EntityState::Memo(m)) => m,
                other => panic!("memo {id} is {other:?}"),
            }
        }

        fn notices(&self) -> Vec<Notice> {
            lock(&self.store).projection(&self.vault).unwrap().notices
        }
    }

    fn seg_key_of(vault: &str, device: &str, first: u64, last: u64) -> String {
        ObjectAddress::segment(vault, EPOCH, device, first, last).remote_key().unwrap()
    }

    fn rec_key(device: &str) -> String {
        ObjectAddress::device_record(VAULT, EPOCH, device).remote_key().unwrap()
    }

    /// Every op in `log/<device>/` on the remote, decrypted.
    async fn remote_ops(fake: &FakeStore, vault: &str, dek: [u8; 32], device: &str) -> Vec<OpBody> {
        let key = derive_subkey(&SecretKey::from_bytes(dek), vault, EPOCH, Purpose::Segment);
        let listed = fake
            .list(ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Seg), device_id: Some(device) })
            .await
            .unwrap();
        let mut out = Vec::new();
        for o in listed {
            let address = ObjectAddress::from_remote_key(&o.key, EPOCH).unwrap();
            for copy in fake.copies(&o.key) {
                out.extend(open_segment(&key, &address, &copy).unwrap().ops.into_iter().map(|s| s.body));
            }
        }
        out
    }

    fn conv(n: u64, wall: u64, op_type: &str, payload: Value) -> OpBody {
        op(n, wall, Kind::Conv, "conv-1", op_type, payload)
    }

    fn msg(n: u64, wall: u64, id: &str, parent: Option<&str>, role: &str) -> OpBody {
        let mut payload = json!({"conv_id": "conv-1", "msg_id": id, "role": role, "content": "c", "attachments": [], "ts": wall});
        if let Some(parent) = parent {
            payload["parent_id"] = json!(parent);
        }
        conv(n, wall, "msg.add", payload)
    }

    fn assert_ok(report: &CycleReport) {
        assert_eq!(report.status, CycleStatus::Ok, "{report:?}");
    }

    // -- AC 4, 5, 6 -------------------------------------------------------

    /// AC 4: two devices regenerate the same answer concurrently: both
    /// siblings survive under the same parent and both settle on one leaf.
    #[tokio::test]
    async fn ac4_concurrent_regenerate_keeps_both_siblings_and_one_active_leaf() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let (mut a, mut b) = (Dev::new("dev-a"), Dev::new("dev-b"));
        a.local(conv(1, 100, "create", json!({"title": "t"})));
        a.local(msg(2, 101, "msg-1", None, "user"));
        a.local(msg(3, 102, "msg-2", Some("msg-1"), "assistant"));
        assert_ok(&a.cycle(&p).await);
        assert_ok(&b.cycle(&p).await);

        a.local(msg(4, 201, "msg-3", Some("msg-1"), "assistant"));
        a.local(conv(5, 201, "set_active", json!({"leaf": "msg-3"})));
        b.local(msg(6, 202, "msg-4", Some("msg-1"), "assistant"));
        b.local(conv(7, 202, "set_active", json!({"leaf": "msg-4"})));
        assert_ok(&a.cycle(&p).await);
        assert_ok(&b.cycle(&p).await);
        assert_ok(&a.cycle(&p).await);

        assert_eq!(a.hash(), b.hash());
        for dev in [&a, &b] {
            let Some(EntityState::Conv(c)) = dev.entity(Kind::Conv, "conv-1") else { panic!("conv missing") };
            let siblings: BTreeSet<&str> = c
                .messages
                .iter()
                .filter(|(_, m)| m.parent_id.as_deref() == Some("msg-1"))
                .map(|(id, _)| id.as_str())
                .collect();
            assert_eq!(siblings, BTreeSet::from(["msg-2", "msg-3", "msg-4"]));
            assert_eq!(c.active_leaf.as_deref(), Some("msg-4"), "one leaf: the later register write");
        }
    }

    /// AC 5: a clear deletes only what it observed; a translation added
    /// concurrently (unobserved) survives on every device.
    #[tokio::test]
    async fn ac5_clear_never_deletes_an_unobserved_new_translation() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let (mut a, mut b) = (Dev::new("dev-a"), Dev::new("dev-b"));
        a.local(op(1, 100, Kind::Tr, "tr-1", "tr.add", json!({"src": "a", "dst": "b"})));
        assert_ok(&a.cycle(&p).await);
        assert_ok(&b.cycle(&p).await);

        a.local(op(2, 200, Kind::Tr, "tr-2", "tr.add", json!({"src": "c", "dst": "d"})));
        // b clears history: one delete per id it observed.
        b.local(op(3, 201, Kind::Tr, "tr-1", "tr.delete", json!({})));
        assert_ok(&a.cycle(&p).await);
        assert_ok(&b.cycle(&p).await);
        assert_ok(&a.cycle(&p).await);

        assert_eq!(a.hash(), b.hash());
        for dev in [&a, &b] {
            assert_eq!(dev.entity(Kind::Tr, "tr-1"), Some(EntityState::Deleted));
            assert!(matches!(dev.entity(Kind::Tr, "tr-2"), Some(EntityState::Tr(_))), "unobserved add kept");
        }
        let deletes: Vec<String> = remote_ops(&fake, VAULT, DEK, "dev-b")
            .await
            .into_iter()
            .filter(|o| o.op_type == "tr.delete")
            .map(|o| o.entity_id)
            .collect();
        assert_eq!(deletes, vec!["tr-1".to_owned()]);
    }

    /// AC 6: evicting a memo locally emits zero delete ops; the peer keeps it
    /// and the local eviction flag survives later re-projections.
    #[tokio::test]
    async fn ac6_local_eviction_emits_no_delete() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let (mut a, mut b) = (Dev::new("dev-a"), Dev::new("dev-b"));
        a.local(create(1, 100, M, "keep me"));
        assert_ok(&a.cycle(&p).await);
        assert_ok(&b.cycle(&p).await);

        lock(&a.store).conn().execute("UPDATE entities SET evicted_local = 1 WHERE id = ?1", [M]).unwrap();
        a.local(create(2, 200, "memo-2", "other"));
        assert_ok(&a.cycle(&p).await);
        assert_ok(&b.cycle(&p).await);

        let ops = remote_ops(&fake, VAULT, DEK, "dev-a").await;
        assert_eq!(ops.len(), 2);
        assert_eq!(ops.iter().filter(|o| o.op_type.contains("delete")).count(), 0);
        assert_eq!(b.memo(M).text, "keep me");
        assert!(lock(&a.store).evicted_entities().unwrap().contains(&("memo".to_owned(), M.to_owned())));
    }

    // -- AC 19b (engine part) ----------------------------------------------

    /// AC 19b at the engine: on Drive, a clone that passes T1–T4 (stale
    /// record, lagging listing) uploads next to the original. Reader B sees
    /// only one variant (per-reader lag) and deletes blindly; C reads all at
    /// once; D reads B's stream before A's. Every reader converges, no edit
    /// is lost, the blind delete is restored, and B's re-delete then wins.
    #[tokio::test]
    async fn ac19b_engine_drive_clone_fork_converges_across_readers() {
        let fake = remote(false);
        let open = Hides::default();
        let p = bound(&fake, &open);
        let (mut a, mut b, mut c, mut d) = (Dev::new("dev-a"), Dev::new("dev-b"), Dev::new("dev-c"), Dev::new("dev-d"));

        b.local(create(1, 100, M, "base"));
        assert_ok(&b.cycle(&p).await);
        assert_ok(&a.cycle(&p).await);
        let stale_rec = fake.copies(&rec_key("dev-a")).remove(0);

        let dir = TempDir::new("g7-ac19b");
        let mut a2 = a.clone_into(&dir);
        a.local(edit(2, 200, M, 1, "variant one"));
        a2.local(edit(3, 210, M, 1, "variant two"));
        assert_ok(&a.cycle(&p).await);

        // A′ races A: its listing lags and its record read is stale.
        let lagging = Hides::default();
        lagging.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
        lock(&lagging.stale).insert(rec_key("dev-a"), stale_rec);
        let report = a2.cycle(&bound(&fake, &lagging)).await;
        assert_ok(&report);
        assert!(report.notices.contains(&EngineNotice::ForkDetected), "{report:?}");
        let key = seg_key_of(VAULT, "dev-a", 1, 1);
        assert_eq!(fake.copies(&key).len(), 2, "both variants stored under one name");

        // B sees only A's copy, then deletes M blindly.
        let refs: Vec<String> = fake
            .list(ListQuery::Props { vault_id: Some(VAULT), kind: Some(RemoteKind::Seg), device_id: Some("dev-a") })
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.provider_ref)
            .collect();
        assert_eq!(refs.len(), 2);
        let b_view = Hides::default();
        lock(&b_view.refs).insert(refs[1].clone());
        let pb = bound(&fake, &b_view);
        assert_ok(&b.cycle(&pb).await);
        assert_eq!(b.memo(M).text, "variant one");
        b.local(delete(4, 300, M));
        assert_ok(&b.cycle(&pb).await);

        // C: everything at once. D: B's stream first, then A's.
        assert_ok(&c.cycle(&p).await);
        let d_view = Hides::default();
        d_view.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
        assert_ok(&d.cycle(&bound(&fake, &d_view)).await);
        d_view.clear();
        assert_ok(&d.cycle(&bound(&fake, &d_view)).await);

        // B's listing catches up: the fork is found and re-projected.
        b_view.clear();
        let report = b.cycle(&pb).await;
        assert_ok(&report);
        assert!(report.notices.contains(&EngineNotice::ForkDetected), "{report:?}");
        assert!(report.notices.contains(&EngineNotice::Reprojected), "{report:?}");
        assert!(b.stats.snapshot().reprojections >= 1);

        let copy_id = conflict_copy_id(M, &oid(2));
        for dev in [&b, &c, &d] {
            assert_eq!(dev.hash(), c.hash());
            assert_eq!(lock(&dev.store).forks(VAULT).unwrap(), BTreeMap::from([("dev-a".to_owned(), 1)]));
            assert_eq!(dev.memo(M).text, "variant two", "blind delete restored");
            assert_eq!(dev.memo(&copy_id).text, "variant one", "no edit lost");
            assert!(dev.notices().iter().any(|n| matches!(n, Notice::RestoredAfterDeleteConflict { .. })));
            assert!(lock(&dev.store).quarantine_rows(VAULT).unwrap().is_empty());
        }

        // B re-deletes having seen both variants: every reader agrees.
        b.local(delete(5, 400, M));
        b.local(delete(6, 400, &copy_id));
        assert_ok(&b.cycle(&p).await);
        assert_ok(&c.cycle(&p).await);
        assert_ok(&d.cycle(&p).await);
        for dev in [&b, &c, &d] {
            assert_eq!(dev.hash(), b.hash());
            assert_eq!(dev.entity(Kind::Memo, M), Some(EntityState::Deleted));
            assert_eq!(dev.entity(Kind::Memo, &copy_id), Some(EntityState::Deleted));
        }
    }

    // -- A2 / A3 / D3 / T6 ---------------------------------------------------

    /// A3: a put that keeps failing with a transport error while a T2
    /// rotation is pending: no upload, no restamp, no new seal, and after N
    /// cycles the "upload outcome unknown" notice.
    #[tokio::test]
    async fn a3_unknown_outcome_pauses_rotation_and_notices_after_n_cycles() {
        let fake = remote(true);
        fake.fail_next_n(Op::PutCreate, ProviderError::Transient("reset".to_owned()), 1_000);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::Failed { class: FailureClass::Transient });
        assert!(!report.notices.contains(&EngineNotice::UploadOutcomeUnknown));

        a.install_fp = Some("install-new".to_owned()); // T2 from now on
        a.local(create(2, 200, "memo-2", "later"));
        for cycle in 2..=A3_NOTICE_CYCLES + 1 {
            let report = a.cycle(&p).await;
            let noticed = report.notices.contains(&EngineNotice::UploadOutcomeUnknown);
            assert_eq!(noticed, cycle >= A3_NOTICE_CYCLES, "cycle {cycle}: {report:?}");
            assert!(!report.notices.iter().any(|n| matches!(n, EngineNotice::Rotated { .. })));
        }
        assert_eq!(a.device(), "dev-a", "no rotation while an outcome is unknown");
        let store = lock(&a.store);
        assert_eq!(store.dots_of(VAULT, &oid(1)).unwrap().len(), 1, "no restamp");
        assert_eq!(store.op_state(VAULT, &oid(1)).unwrap().unwrap().2, UploadState::Attempted);
        assert_eq!(store.pending_segments(VAULT, "dev-a").unwrap().len(), 1, "nothing new sealed");
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).is_empty());
    }

    /// A2: a Drive upload that never becomes visible is resent at most
    /// `MAX_DRIVE_RESENDS` times (persisted across restarts), then the user
    /// sees an error; a created upload persists its Drive file id.
    #[tokio::test]
    async fn a2_drive_resends_are_capped_and_file_ids_persist() {
        let fake = remote(false);
        let hides = Hides::default();
        hides.black_hole.store(true, Ordering::SeqCst);
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_eq!(a.cycle(&p).await.status, CycleStatus::Failed { class: FailureClass::Transient });
        for _ in 0..MAX_DRIVE_RESENDS {
            let report = a.cycle(&p).await;
            assert!(!report.notices.contains(&EngineNotice::ResendCapReached), "{report:?}");
        }
        let puts = hides.puts.load(Ordering::SeqCst);
        assert_eq!(puts as u64, 1 + MAX_DRIVE_RESENDS, "first put plus one resend per settle");
        let report = a.cycle(&p).await;
        assert!(report.notices.contains(&EngineNotice::ResendCapReached));
        a.engine = Engine::default(); // restart: the count is persisted
        assert!(a.cycle(&p).await.notices.contains(&EngineNotice::ResendCapReached));
        assert_eq!(hides.puts.load(Ordering::SeqCst), puts, "no resend past the cap");
        assert_eq!(lock(&a.store).op_state(VAULT, &oid(1)).unwrap().unwrap().2, UploadState::Attempted);

        let mut b = Dev::new("dev-b");
        let open = Hides::default();
        b.local(create(2, 100, "memo-2", "y"));
        assert_ok(&b.cycle(&bound(&fake, &open)).await);
        let key = seg_key_of(VAULT, "dev-b", 1, 1);
        let file_id = lock(&b.store).remote_object_ref(VAULT, &key).unwrap().unwrap();
        let listed = fake
            .list(ListQuery::Props { vault_id: Some(VAULT), kind: Some(RemoteKind::Seg), device_id: Some("dev-b") })
            .await
            .unwrap();
        assert_eq!(listed[0].provider_ref, file_id);
        assert_ne!(file_id, key, "Drive names a copy by file id");
    }

    /// D3: `invalid_seen` relists the key: an unreadable copy still there
    /// is quarantined; one that vanished in between is tolerated.
    #[tokio::test]
    async fn d3_invalid_seen_relists_and_tolerates_a_vanished_object() {
        for vanishes in [false, true] {
            let fake = remote(false);
            let key = seg_key_of(VAULT, "dev-a", 1, 1);
            fake.seed(&key, b"garbage that is not an envelope");
            let garbage_ref = fake.list(ListQuery::Prefix(&key)).await.unwrap()[0].provider_ref.clone();
            let hides = Hides::default();
            hides.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
            if vanishes {
                // get_all #1 is the settle probe, #2 the relist.
                *lock(&hides.vanish) = Some((key.clone(), garbage_ref, 2));
            }
            let mut a = Dev::new("dev-a");
            a.local(create(1, 100, M, "x"));
            let report = a.cycle(&bound(&fake, &hides)).await;
            assert_ok(&report);
            assert_eq!(report.uploaded_segments, 1);
            let rows = lock(&a.store).quarantine_rows(VAULT).unwrap();
            if vanishes {
                assert!(rows.is_empty(), "{rows:?}");
                assert_eq!(fake.copies(&key).len(), 1);
            } else {
                assert_eq!(rows.iter().filter(|(k, _)| *k == key).count(), 1, "{rows:?}");
                assert_eq!(fake.copies(&key).len(), 2, "another writer's copy is never deleted");
            }
        }
    }

    /// T6: on a create-only store our name is taken by another writer's
    /// valid segment: Rejected → quarantine, rotate (T6), notice; the other
    /// object is untouched and the op is reissued under the new id.
    #[tokio::test]
    async fn t6_rejected_upload_quarantines_and_rotates() {
        let fake = remote(true);
        let key = seg_key_of(VAULT, "dev-a", 1, 1);
        // A clone of dev-a wrote its own (1, 1) first.
        let clone = Dev::new("dev-a");
        clone.local(create(9, 90, "memo-clone", "clone text"));
        let clone_bytes = {
            let seg = derive_subkey(&SecretKey::from_bytes(DEK), VAULT, EPOCH, Purpose::Segment);
            let mut store = lock(&clone.store);
            store
                .seal_pending(VAULT, |s, plain| {
                    seal(&seg, &ObjectAddress::segment(VAULT, EPOCH, &s.device_id, s.first_seq, s.last_seq), plain)
                })
                .unwrap()
                .unwrap()
                .bytes
        };
        fake.seed(&key, &clone_bytes);
        let hides = Hides::default();
        hides.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x-text"));

        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert!(report.notices.contains(&EngineNotice::UploadRejected), "{report:?}");
        assert!(report.notices.contains(&EngineNotice::Rotated { triggers: vec!["t6"] }), "{report:?}");
        let new_id = a.device();
        assert_ne!(new_id, "dev-a");
        let rows = lock(&a.store).quarantine_rows(VAULT).unwrap();
        assert!(rows.iter().any(|(k, _)| *k == key), "{rows:?}");
        assert!(rows.iter().any(|(k, _)| *k == format!("op/{}", oid(1))), "{rows:?}");
        assert_eq!(fake.copies(&key), vec![clone_bytes], "the other writer's object is untouched");
        let s = a.stats.snapshot();
        assert_eq!((s.rejected, s.rotations), (1, 1));

        hides.clear();
        assert_ok(&a.cycle(&p).await);
        let ops = remote_ops(&fake, VAULT, DEK, &new_id).await;
        assert_eq!(ops.len(), 1);
        assert_ne!(ops[0].op_id, oid(1), "reissued under a new op id");
        assert_eq!(ops[0].payload["text"], "x-text");
        assert_eq!(a.memo(M).text, "x-text");
        assert_eq!(a.memo("memo-clone").text, "clone text", "the clone's write is read, not lost");
    }

    // -- compaction / snapshot catch-up ---------------------------------------

    /// Step 4 then a newcomer: A's segments are 31 days old and read by B;
    /// A snapshots, verifies, and deletes them. C (fresh) reaches A's stream
    /// only through the snapshot and then continues on A's next segment.
    #[tokio::test]
    async fn compaction_deletes_read_segments_and_a_newcomer_catches_up_by_snapshot() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let (mut a, mut b, mut c) = (Dev::new("dev-a"), Dev::new("dev-b"), Dev::new("dev-c"));
        a.local(create(1, 100, M, "one"));
        assert_ok(&a.cycle(&p).await);
        a.local(edit(2, 200, M, 1, "two"));
        assert_ok(&a.cycle(&p).await);
        // A record still reading only seq 1 would hold deletion back.
        assert_ok(&b.cycle(&p).await);

        a.clock_offset_ms = compaction::MAX_SEGMENT_AGE_MS + 24 * 60 * 60 * 1000;
        assert_ok(&a.cycle(&p).await);
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).is_empty());
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).is_empty());
        let snaps = fake
            .list(ListQuery::Props { vault_id: Some(VAULT), kind: Some(RemoteKind::Snap), device_id: Some("dev-a") })
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1);
        assert!(lock(&a.store).pending_segments(VAULT, "dev-a").unwrap().is_empty());

        // A's next cycle reads its own deleted range as gone, not foreign.
        a.local(edit(3, 300, M, 2, "three"));
        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(a.device(), "dev-a", "{report:?}");

        let report = c.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(c.memo(M).text, "three");
        assert_eq!(c.hash(), a.hash());
        assert!(lock(&c.store).quarantine_rows(VAULT).unwrap().is_empty());
        assert!(!report.notices.contains(&EngineNotice::StreamGap));
    }

    /// Before the snapshot is listed (lag) a gap is retried, not held.
    #[tokio::test]
    async fn a_gap_without_a_snapshot_is_retried_not_held() {
        let fake = remote(true);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "one"));
        assert_ok(&a.cycle(&bound(&fake, &hides)).await);
        a.local(edit(2, 200, M, 1, "two"));
        assert_ok(&a.cycle(&bound(&fake, &hides)).await);
        let first = seg_key_of(VAULT, "dev-a", 1, 1);
        let bytes = fake.copies(&first).remove(0);
        fake.delete(&first).await.unwrap();

        let mut c = Dev::new("dev-c");
        let report = c.cycle(&bound(&fake, &hides)).await;
        assert_ok(&report);
        assert!(report.notices.contains(&EngineNotice::StreamGap));
        assert_eq!(lock(&c.store).cursor(VAULT, "dev-a").unwrap(), None, "not held");
        fake.seed(&first, &bytes);
        assert_ok(&c.cycle(&bound(&fake, &hides)).await);
        assert_eq!(c.memo(M).text, "two");
    }

    // -- stops ----------------------------------------------------------------

    #[tokio::test]
    async fn binding_mismatch_stops_before_any_request() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = Bound::new(View { inner: &fake, hides: &hides }, binding_fp("fake|someone-else"));
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::BindingMismatch });
        assert_eq!(report.notices, vec![EngineNotice::BindingMismatch]);
        assert_eq!(fake.requests(), 0, "zero transmission");
        assert_eq!(a.stats.snapshot().failures.binding, 1);
        assert!(lock(&a.store).pending_segments(VAULT, "dev-a").unwrap().is_empty(), "nothing sealed");
    }

    #[tokio::test]
    async fn removed_device_uploads_nothing_and_never_recreates_its_record() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&p).await);
        assert_eq!(fake.copies(&rec_key("dev-a")).len(), 1);

        fake.delete(&rec_key("dev-a")).await.unwrap(); // removed by another device
        // The put is older than the settle window (ADV-7): two misses are a removal.
        a.clock_offset_ms = SETTLE_WINDOW_MS;
        a.local(create(2, 200, "memo-2", "y"));
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved });
        assert!(report.notices.contains(&EngineNotice::DeviceRemoved));
        assert_eq!(report.uploaded_segments, 0);
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).is_empty());
        assert!(fake.copies(&rec_key("dev-a")).is_empty(), "record not recreated");
        assert!(lock(&a.store).pending_segments(VAULT, "dev-a").unwrap().len() == 1, "nothing new sealed");
    }

    /// D1 / ADV-7: a record put that a lagging store does not show yet (never
    /// read back, inside the settle window) is no removal, and is never
    /// recreated either; once it was read back, two misses are a removal.
    #[tokio::test]
    async fn d1_fresh_record_put_missing_is_lag_inside_the_settle_window_and_never_recreated() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&p).await);
        let rec = fake.copies(&rec_key("dev-a"));
        assert_eq!(rec.len(), 1);

        // Not visible yet (lag) right after the put: not a removal.
        fake.delete(&rec_key("dev-a")).await.unwrap();
        a.local(create(2, 200, "memo-2", "y"));
        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert!(fake.copies(&rec_key("dev-a")).is_empty(), "an absent published record is never recreated");

        // It shows up: read back (verified), and later writes replace it.
        fake.put_replace(&rec_key("dev-a"), &rec[0], None).await.unwrap();
        a.local(create(3, 300, "memo-3", "z"));
        assert_ok(&a.cycle(&p).await);
        assert_eq!(fake.copies(&rec_key("dev-a")).len(), 1);

        // A quiet cycle reads it back (verified, nothing to rewrite) ...
        assert_ok(&a.cycle(&p).await);
        // ... then gone: a removal at once, no settle wait.
        fake.delete(&rec_key("dev-a")).await.unwrap();
        a.local(create(4, 400, "memo-4", "w"));
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 4, 4)).is_empty());
        assert!(fake.copies(&rec_key("dev-a")).is_empty());
    }

    #[tokio::test]
    async fn listing_not_found_is_a_configuration_stop() {
        let fake = remote(true);
        fake.fail_next(Op::List, ProviderError::NotFound);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        let report = a.cycle(&bound(&fake, &hides)).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::Configuration });
        assert!(report.notices.contains(&EngineNotice::ConfigurationError));
    }

    #[tokio::test]
    async fn retryable_failures_are_classified() {
        assert_eq!(classify(&ProviderError::Permanent("HTTP 408".to_owned())), FailureClass::Transient);
        assert_eq!(classify(&ProviderError::Permanent("HTTP 423".to_owned())), FailureClass::Transient);
        assert_eq!(classify(&ProviderError::Permanent("RequestTimeTooSkewed".to_owned())), FailureClass::Transient);
        assert_eq!(classify(&ProviderError::Permanent("HTTP 400".to_owned())), FailureClass::Permanent);
        assert_eq!(classify(&ProviderError::NotFound), FailureClass::Permanent);
        let fake = remote(true);
        fake.fail_next(Op::List, ProviderError::RateLimited { retry_after: Some(Duration::from_secs(7)) });
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        let report = a.cycle(&bound(&fake, &hides)).await;
        assert_eq!(report.status, CycleStatus::RateLimited { retry_after: Some(Duration::from_secs(7)) });
        assert_eq!(CycleOutcome::of(&report), CycleOutcome::RateLimited(Some(Duration::from_secs(7))));
    }

    #[test]
    fn device_record_round_trips_and_binds_its_owner() {
        let dek = SecretKey::from_bytes(DEK);
        let record = DeviceRecord {
            v: 1,
            device_id: "dev-a".to_owned(),
            label: "Mac".to_owned(),
            last_seq: 3,
            chain_head: hex(&[4; 32]),
            frontier: BTreeMap::from([("dev-b".to_owned(), 2)]),
            last_seen_hlc: Hlc { wall_ms: 5, ctr: 0 },
            app_version: "1".to_owned(),
        };
        let bytes = seal_record(&dek, VAULT, EPOCH, &record).unwrap();
        assert_eq!(open_record(&dek, VAULT, EPOCH, "dev-a", &bytes).unwrap(), record);
        assert!(open_record(&dek, VAULT, EPOCH, "dev-b", &bytes).is_err(), "address binds the device");
    }

    // -- telemetry -------------------------------------------------------------

    /// Logs, the diagnostics export, and cycle reports never carry payloads,
    /// keys, vault/device/entity/op ids.
    #[tokio::test]
    async fn telemetry_is_content_free() {
        const SV: &str = "vault-sentinelvlt";
        const SECRET: &str = "SENTINEL-PAYLOAD-91c3";
        let dek = [0x5a; 32];
        capture::take();
        let fake = remote_for(true, SV);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::in_vault(SV, "dev-sentinela", dek);
        let mut b = Dev::in_vault(SV, "dev-sentinelb", dek);
        a.local(create(1, 100, M, SECRET));
        let mut reports = Vec::new();
        reports.push(a.cycle(&p).await);
        reports.push(b.cycle(&p).await);
        b.local(edit(2, 200, M, 1, SECRET));
        reports.push(b.cycle(&p).await);
        reports.push(a.cycle(&p).await);
        let wrong = Bound::new(View { inner: &fake, hides: &hides }, binding_fp("other"));
        reports.push(a.cycle(&wrong).await);

        let lines = capture::take();
        assert!(lines.iter().any(|l| l.starts_with("sync.cycle")), "{lines:?}");
        let mut text = lines.join("\n");
        text.push_str(&export_diagnostics(&a.stats, Some(SV), &[("state", "ok")]));
        text.push_str(&serde_json::to_string(&reports).unwrap());
        for needle in [SV, "sentinel", SECRET, M, &oid(1), &oid(2), &hex(&dek), "dev-"] {
            assert!(!text.to_lowercase().contains(&needle.to_lowercase()), "telemetry leaks {needle}: {text}");
        }
        assert!(text.contains(VaultTag::of(SV).as_str()));
    }

    #[tokio::test]
    async fn cycle_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let mut ctx = CycleCtx {
            provider: &p,
            secrets: &a.secrets,
            store: &a.store,
            clock: &a.clock,
            pause: &NoPause,
            now: &real_now,
            stats: &a.stats,
            engine: &mut a.engine,
            vault: VAULT,
            key_epoch: EPOCH,
            install_fp: None,
            device_label: "d",
            app_version: "v",
            checkpoint_dir: None,
            fence: None,
        };
        let fut = Engine::cycle(&mut ctx);
        assert_send(&fut);
        assert_ok(&fut.await);
    }

    // -- scheduler ----------------------------------------------------------------

    #[test]
    fn next_delay_debounces_and_runs_single_flight() {
        let mut s = SchedState::default();
        let mut rng = || 0.5;
        assert_eq!(next_delay(&mut s, SchedEvent::LocalChange, &mut rng), Decision::Wait(DEBOUNCE));
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut rng), Decision::RunNow);
        assert!(s.running);
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut rng), Decision::Idle, "no overlap");
        assert_eq!(next_delay(&mut s, SchedEvent::LocalChange, &mut rng), Decision::Idle);
        assert!(s.dirty);
        let done = SchedEvent::CycleDone(CycleOutcome::Success);
        assert_eq!(next_delay(&mut s, done, &mut rng), Decision::Wait(DEBOUNCE), "change during the run");
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut rng), Decision::RunNow);
        assert_eq!(next_delay(&mut s, done, &mut rng), Decision::Wait(PERIOD));
        assert_eq!(next_delay(&mut s, SchedEvent::Cancel, &mut rng), Decision::Exit);
    }

    #[test]
    fn next_delay_backs_off_with_jitter_up_to_the_cap_and_resets() {
        let mut s = SchedState::default();
        let retry = SchedEvent::CycleDone(CycleOutcome::Retry);
        let mut low = || 0.0;
        assert_eq!(next_delay(&mut s, retry, &mut low), Decision::Wait(BACKOFF_BASE / 2));
        assert_eq!(next_delay(&mut s, retry, &mut low), Decision::Wait(BACKOFF_BASE));
        let mut high = || 0.999_999;
        for _ in 0..40 {
            let Decision::Wait(d) = next_delay(&mut s, retry, &mut high) else { panic!() };
            assert!(d <= BACKOFF_CAP && d >= BACKOFF_CAP / 2 || s.failures < 10, "{d:?}");
        }
        let Decision::Wait(d) = next_delay(&mut s, retry, &mut high) else { panic!() };
        assert!(d <= BACKOFF_CAP && d >= BACKOFF_CAP.mul_f64(0.99));
        for failures in 1..50 {
            for r in [0.0, 0.3, 0.999] {
                let d = backoff(failures, &mut || r);
                assert!(d >= BACKOFF_BASE / 2 && d <= BACKOFF_CAP, "{failures} {r} {d:?}");
            }
        }
        let mut mid = || 0.5;
        assert_eq!(next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::Success), &mut mid), Decision::Wait(PERIOD));
        assert_eq!(s.failures, 0);
    }

    #[test]
    fn next_delay_honours_retry_after_and_waits_for_resume_after_a_stop() {
        let mut s = SchedState::default();
        let mut rng = || 0.5;
        let after = SchedEvent::CycleDone(CycleOutcome::RateLimited(Some(Duration::from_secs(42))));
        assert_eq!(next_delay(&mut s, after, &mut rng), Decision::Wait(Duration::from_secs(42)));
        let huge = SchedEvent::CycleDone(CycleOutcome::RateLimited(Some(Duration::from_secs(9 * 24 * 3600))));
        assert_eq!(next_delay(&mut s, huge, &mut rng), Decision::Wait(RETRY_AFTER_CAP));
        assert_eq!(next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::Stop), &mut rng), Decision::Idle);
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut rng), Decision::Idle);
        assert_eq!(next_delay(&mut s, SchedEvent::LocalChange, &mut rng), Decision::Idle);
        assert_eq!(next_delay(&mut s, SchedEvent::Resume, &mut rng), Decision::RunNow);
        assert_eq!((s.stopped, s.failures, s.running), (false, 0, true));
    }

    #[tokio::test]
    async fn run_scheduler_runs_inline_and_stops_on_cancel() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(SchedEvent::Resume).unwrap();
        tx.send(SchedEvent::Cancel).unwrap();
        let runs = AtomicUsize::new(0);
        let in_flight = AtomicBool::new(false);
        run_scheduler(
            rx,
            || async {
                assert!(!in_flight.swap(true, Ordering::SeqCst), "cycles overlap");
                let n = runs.fetch_add(1, Ordering::SeqCst);
                in_flight.store(false, Ordering::SeqCst);
                if n == 0 {
                    CycleOutcome::Stop
                } else {
                    CycleOutcome::Success
                }
            },
            || 0.5,
        )
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "first run, then one after Resume");
        assert!((0.0..1.0).contains(&os_jitter()));
    }

    // -- D10: put_create rate limit / quota ----------------------------------

    /// A 429 on the segment put: the cycle ends RateLimited with the
    /// provider's Retry-After (and the notice); the segment stays attempted
    /// and is settled next cycle.
    #[tokio::test]
    async fn d10_put_create_rate_limited_propagates_retry_after_and_keeps_the_segment_attempted() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        fake.fail_next(Op::PutCreate, ProviderError::RateLimited { retry_after: Some(Duration::from_secs(7)) });
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::RateLimited { retry_after: Some(Duration::from_secs(7)) }, "{report:?}");
        assert_eq!(CycleOutcome::of(&report), CycleOutcome::RateLimited(Some(Duration::from_secs(7))));
        assert!(report.notices.contains(&EngineNotice::RateLimited), "{report:?}");
        assert_eq!(a.stats.snapshot().failures.rate, 1);
        assert_eq!(lock(&a.store).op_state(VAULT, &oid(1)).unwrap().unwrap().2, UploadState::Attempted);
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).is_empty());

        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(lock(&a.store).op_state(VAULT, &oid(1)).unwrap().unwrap().2, UploadState::Published);
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).len(), 1);
    }

    #[tokio::test]
    async fn d10_put_create_quota_exceeded_is_a_quota_notice() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        fake.fail_next(Op::PutCreate, ProviderError::QuotaExceeded);
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::Failed { class: FailureClass::Quota }, "{report:?}");
        assert!(report.notices.contains(&EngineNotice::QuotaExceeded), "{report:?}");
        assert_eq!(a.stats.snapshot().failures.quota, 1);
        assert_eq!(lock(&a.store).op_state(VAULT, &oid(1)).unwrap().unwrap().2, UploadState::Attempted);
    }

    // -- CB-B4: cycle gates (AC 27) -------------------------------------------

    /// Puts, replaces and deletes that reached the store (gets and lists
    /// excluded): a gate test asserts zero of them.
    #[derive(Default)]
    struct Writes(AtomicUsize, Mutex<Vec<String>>);

    struct Counting<'a> {
        inner: &'a FakeStore,
        writes: &'a Writes,
    }

    impl ObjectStore for Counting<'_> {
        fn create_only(&self) -> bool {
            self.inner.create_only()
        }
        fn identity(&self) -> String {
            self.inner.identity()
        }
        async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
            self.inner.list(q).await
        }
        async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
            self.inner.get(key).await
        }
        async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
            self.writes.0.fetch_add(1, Ordering::SeqCst);
            lock(&self.writes.1).push(key.to_owned());
            self.inner.put_create(key, bytes).await
        }
        async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
            self.writes.0.fetch_add(1, Ordering::SeqCst);
            self.inner.put_replace(key, bytes, if_match).await
        }
        async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
            self.writes.0.fetch_add(1, Ordering::SeqCst);
            self.inner.put_replace_ref(key, provider_ref, bytes).await
        }
        async fn delete(&self, key: &str) -> Result<(), ProviderError> {
            self.writes.0.fetch_add(1, Ordering::SeqCst);
            self.inner.delete(key).await
        }
        async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
            self.inner.get_all(key).await
        }
        async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
            self.writes.0.fetch_add(1, Ordering::SeqCst);
            self.inner.delete_ref(provider_ref).await
        }
        async fn quota(&self) -> Result<Quota, ProviderError> {
            self.inner.quota().await
        }
        async fn ensure_container(&self) -> Result<(), ProviderError> {
            self.inner.ensure_container().await
        }
    }

    fn counting<'a>(inner: &'a FakeStore, writes: &'a Writes) -> Bound<Counting<'a>> {
        Bound::new(Counting { inner, writes }, binding_fp(&inner.identity()))
    }

    /// (a) Another device's wipe deleted the manifest: device B's cycle stops
    /// on ManifestMissing with zero writes, and the manifest stays absent.
    #[tokio::test]
    async fn cb_b4_manifest_missing_stops_device_b_with_zero_writes() {
        let fake = remote(true);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&bound(&fake, &hides)).await);
        let mut b = Dev::new("dev-b");
        assert_ok(&b.cycle(&bound(&fake, &hides)).await);

        fake.delete(&vault::manifest_key(VAULT).unwrap()).await.unwrap();
        b.local(create(2, 200, "memo-2", "y"));
        let writes = Writes::default();
        let report = b.cycle(&counting(&fake, &writes)).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
        assert!(report.notices.contains(&EngineNotice::ManifestMissing));
        assert_eq!(writes.0.load(Ordering::SeqCst), 0, "no put_create / put_replace / delete");
        assert!(fake.copies(&vault::manifest_key(VAULT).unwrap()).is_empty(), "manifest not recreated");
        assert!(fake.copies(&seg_key_of(VAULT, "dev-b", 1, 1)).is_empty());
    }

    /// (b) A second vault's manifest: noticed, the cycle stays Ok, and the
    /// other vault gets no write or delete.
    #[tokio::test]
    async fn cb_b4_other_vault_is_noticed_and_never_written() {
        const OTHER: &str = "vault-other";
        let fake = remote(true);
        let other_manifest = vault::manifest_key(OTHER).unwrap();
        fake.seed(&other_manifest, b"{\"manifest\":true}");
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        let report = a.cycle(&bound(&fake, &hides)).await;
        assert_ok(&report);
        assert!(report.notices.contains(&EngineNotice::OtherVaults { count: 1 }), "{report:?}");
        assert_eq!(fake.copies(&other_manifest), vec![b"{\"manifest\":true}".to_vec()]);
        let other_keys: Vec<String> = fake
            .list(ListQuery::Prefix(&format!("v1/{OTHER}/")))
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(other_keys, vec![other_manifest]);
    }

    /// (c) A wipe started on this device: stop with zero provider requests.
    #[tokio::test]
    async fn cb_b4_wipe_in_progress_stops_with_zero_requests() {
        let fake = remote(true);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        lock(&a.store).meta_set(META_WIPE_STATE, "started").unwrap();
        let before = fake.requests();
        let report = a.cycle(&bound(&fake, &hides)).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::WipeInProgress }, "{report:?}");
        assert_eq!(fake.requests(), before, "zero provider requests");
    }

    /// D6: a stale vault generation stops before anything is sent.
    #[tokio::test]
    async fn d6_stale_fence_stops_before_any_request() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        let stale = || false;
        let now = real_now;
        let before = fake.requests();
        let report = {
            let mut ctx = CycleCtx {
                provider: &p,
                secrets: &a.secrets,
                store: &a.store,
                clock: &a.clock,
                pause: &NoPause,
                now: &now,
                stats: &a.stats,
                engine: &mut a.engine,
                vault: &a.vault,
                key_epoch: EPOCH,
                install_fp: None,
                device_label: "test device",
                app_version: "0.0.0-test",
                checkpoint_dir: None,
                fence: Some(&stale),
            };
            Engine::cycle(&mut ctx).await
        };
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::VaultSwitched }, "{report:?}");
        assert!(report.notices.is_empty(), "{report:?}");
        assert_eq!(fake.requests(), before);
        assert_eq!(a.stats.snapshot().last_error, None);
    }

    // -- D5: blobs --------------------------------------------------------------

    fn blob_row(dek: &SecretKey, bytes: &[u8]) -> BlobRow {
        let name_key = derive_subkey(dek, VAULT, EPOCH, Purpose::BlobName);
        BlobRow {
            blob_id: hex(&hmac_sha256(&name_key, bytes)),
            mime: "image/png".to_owned(),
            bytes: bytes.to_vec(),
            sha256: Sha256::digest(bytes).into(),
        }
    }

    /// A conversation with one message carrying `blob` (ops 1, 2).
    fn conv_with_attachment(a: &Dev, n: u64, blob: &BlobRow) {
        attach_into(&a.store, n, blob);
    }

    /// [`conv_with_attachment`] on a store borrowed on its own (so a provider
    /// hook can call it mid-cycle).
    fn attach_into(store: &Mutex<Store>, n: u64, blob: &BlobRow) {
        let conv = format!("conv-{n}");
        let bodies = vec![
            op(n, 100, Kind::Conv, &conv, "create", json!({"title": "t"})),
            op(
                n + 1,
                101,
                Kind::Conv,
                &conv,
                "msg.add",
                json!({"conv_id": conv, "msg_id": format!("msg-{n}"), "role": "user", "content": "see",
                       "attachments": [{"blob_id": blob.blob_id, "mime": blob.mime}], "ts": 101}),
            ),
        ];
        lock(store).insert_local_batch(VAULT, bodies, vec![blob.clone()], Vec::new()).unwrap();
    }

    fn blob_key(blob_id: &str) -> String {
        ObjectAddress::blob(VAULT, EPOCH, blob_id).remote_key().unwrap()
    }

    /// Upload before the segment, dedupe on a second device, download into a
    /// device that lacks the bytes.
    #[tokio::test]
    async fn d5_blob_uploads_before_its_segment_dedupes_and_downloads() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let blob = blob_row(&dek, b"PNG-bytes-1");
        conv_with_attachment(&a, 1, &blob);
        let writes = Writes::default();
        let report = a.cycle(&counting(&fake, &writes)).await;
        assert_ok(&report);
        let order = lock(&writes.1).clone();
        let blob_at = order.iter().position(|k| *k == blob_key(&blob.blob_id)).expect("blob put");
        let seg_at = order.iter().position(|k| k.contains("/log/")).expect("segment put");
        assert!(blob_at < seg_at, "blob before the segment that references it: {order:?}");
        let stored = fake.copies(&blob_key(&blob.blob_id));
        assert_eq!(stored.len(), 1, "blob uploaded");
        assert_eq!(lock(&a.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some(BLOB_PUBLISHED));
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 2)).len(), 1);

        // Dedupe: the same content uploaded again by another device.
        assert_eq!(
            upload_blob(&p, &dek, VAULT, EPOCH, &blob.blob_id, &blob.bytes).await.unwrap(),
            BlobUpload::Deduped
        );
        assert_eq!(fake.copies(&blob_key(&blob.blob_id)), stored, "nothing overwritten");

        // Download: device B catches up and fetches the attachment.
        let mut b = Dev::new("dev-b");
        let report = b.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(lock(&b.store).blob_bytes(&blob.blob_id).unwrap().map(|(_, bytes)| bytes), Some(blob.bytes.clone()));
        assert_eq!(lock(&b.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some(BLOB_PUBLISHED));
    }

    /// A blob put fails transiently: no segment goes out that cycle.
    #[tokio::test]
    async fn d5_no_segment_before_its_blob_is_up() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let blob = blob_row(&dek, b"PNG-bytes-2");
        conv_with_attachment(&a, 1, &blob);
        fake.fail_next(Op::PutCreate, ProviderError::Transient("reset".to_owned()));
        let report = a.cycle(&p).await;
        assert!(matches!(report.status, CycleStatus::Failed { class: FailureClass::Transient }), "{report:?}");
        assert!(fake.copies(&blob_key(&blob.blob_id)).is_empty());
        assert!(fake.list(ListQuery::Prefix(&format!("v1/{VAULT}/log/"))).await.unwrap().is_empty(), "no segment");
        assert_ok(&a.cycle(&p).await);
        assert_eq!(fake.copies(&blob_key(&blob.blob_id)).len(), 1);
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 2)).len(), 1);
    }

    /// Something else under the blob's name: fail closed — nothing
    /// overwritten, no segment, BlobRejected; a wrong id is refused unsent.
    #[tokio::test]
    async fn d5_blob_name_holding_other_content_fails_closed() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let blob = blob_row(&dek, b"PNG-bytes-3");
        fake.seed(&blob_key(&blob.blob_id), b"garbage that is not this blob");
        conv_with_attachment(&a, 1, &blob);
        let report = a.cycle(&p).await;
        assert!(report.notices.contains(&EngineNotice::BlobRejected), "{report:?}");
        assert_eq!(fake.copies(&blob_key(&blob.blob_id)), vec![b"garbage that is not this blob".to_vec()]);
        assert!(fake.list(ListQuery::Prefix(&format!("v1/{VAULT}/log/"))).await.unwrap().is_empty(), "no segment");
        assert_eq!(lock(&a.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some("pending"));

        let before = fake.requests();
        let other = blob_row(&dek, b"other");
        assert_eq!(
            upload_blob(&p, &dek, VAULT, EPOCH, &other.blob_id, b"not other").await.unwrap(),
            BlobUpload::Mismatch
        );
        assert_eq!(fake.requests(), before, "refused before any request");
    }

    /// A referenced blob that is missing (or over the cap) stays evicted
    /// with a notice; the message still applies; it is not fetched again.
    #[tokio::test]
    async fn d5_missing_or_oversize_blob_is_evicted_and_never_blocks_apply() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let blob = blob_row(&dek, b"PNG-bytes-4");
        conv_with_attachment(&a, 1, &blob);
        assert_ok(&a.cycle(&p).await);
        fake.delete(&blob_key(&blob.blob_id)).await.unwrap();

        let mut b = Dev::new("dev-b");
        let report = b.cycle(&p).await;
        assert_ok(&report);
        assert!(report.notices.contains(&EngineNotice::BlobEvicted), "{report:?}");
        assert_eq!(report.applied_segments, 1, "the message applied");
        assert_eq!(lock(&b.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some(BLOB_EVICTED));
        assert_eq!(lock(&b.store).blob_bytes(&blob.blob_id).unwrap(), None);
        let report = b.cycle(&p).await;
        assert!(!report.notices.contains(&EngineNotice::BlobEvicted), "not fetched again: {report:?}");

        // Over the cap: refused like a missing one.
        let big = blob_row(&dek, &[7u8; 64]);
        upload_blob(&p, &dek, VAULT, EPOCH, &big.blob_id, &big.bytes).await.unwrap();
        assert_eq!(download_blob(&p, &dek, VAULT, EPOCH, &big.blob_id, 63).await.unwrap(), None);
        assert_eq!(download_blob(&p, &dek, VAULT, EPOCH, &big.blob_id, 64).await.unwrap(), Some(big.bytes.clone()));
    }

    // -- A4: seal split -----------------------------------------------------------

    /// Unsealed ops larger than the cap together are sealed into several
    /// chained segments, each below the cap, and all upload and apply.
    #[tokio::test]
    async fn a4_seal_splits_segments_below_the_cap_and_they_chain() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let text = "z".repeat(3 << 20);
        for n in 1..=4 {
            a.local(create(n, 100 * n, &format!("memo-{n}"), &text));
        }
        let report = a.cycle(&p).await;
        assert_ok(&report);
        let segs = lock(&a.store).pending_segments(VAULT, "dev-a").unwrap();
        assert!(segs.len() >= 2, "split: {segs:?}");
        let mut next = 1;
        for &(first, last, _) in &segs {
            assert_eq!(first, next, "contiguous");
            next = last + 1;
            let bytes = lock(&a.store).sealed_bytes(VAULT, "dev-a", first).unwrap().unwrap().bytes;
            assert!(bytes.len() < SEGMENT_PLAIN_CAP + 64, "{}", bytes.len());
            assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", first, last)), vec![bytes]);
        }
        assert_eq!(next, 5);
        let mut b = Dev::new("dev-b");
        let report = b.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(report.applied_segments, segs.len(), "the chain applies in order");
    }

    #[test]
    fn a4_seal_pending_capped_never_splits_one_op_and_chains_hashes() {
        let mut store = crate::sync::sim_tests::device("dev-a");
        for n in 1..=3 {
            crate::sync::sim_tests::local(&mut store, create(n, 100 * n, &format!("m{n}"), &"q".repeat(600)));
        }
        let segs = store.seal_pending_capped(crate::sync::sim_tests::VAULT, 1_000, |_, plain| Ok(plain.to_vec())).unwrap();
        assert_eq!(segs.iter().map(|s| (s.first_seq, s.last_seq)).collect::<Vec<_>>(), [(1, 1), (2, 2), (3, 3)]);
        let decoded: Vec<Segment> = segs.iter().map(|s| serde_json::from_slice(&s.bytes).unwrap()).collect();
        for pair in decoded.windows(2) {
            assert_eq!(pair[1].prev_seg_hash, hex(&segment_hash(&pair[0]).unwrap()));
        }
        assert!(store.seal_pending_capped(crate::sync::sim_tests::VAULT, 1_000, |_, p| Ok(p.to_vec())).unwrap().is_empty());
    }

    // -- D2: scheduler -------------------------------------------------------------

    #[test]
    fn d2_local_change_never_shortens_a_backoff_and_resets_after_success() {
        let mut s = SchedState::default();
        let mut rng = || 0.5;
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut rng), Decision::RunNow);
        assert!(matches!(next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::Retry), &mut rng), Decision::Wait(_)));
        assert_eq!(next_delay(&mut s, SchedEvent::LocalChange, &mut rng), Decision::AtLeast(DEBOUNCE));
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut rng), Decision::RunNow);
        assert_eq!(next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::Success), &mut rng), Decision::Wait(PERIOD));
        assert_eq!(next_delay(&mut s, SchedEvent::LocalChange, &mut rng), Decision::Wait(DEBOUNCE));
    }

    #[test]
    fn d2_debounce_is_capped_at_max_wait_from_the_first_change() {
        let first = Duration::from_secs(1_000);
        let at = |now: u64| debounce_deadline(first + Duration::from_secs(now), Some(first), DEBOUNCE, DEBOUNCE_MAX_WAIT);
        assert_eq!(at(0), first + DEBOUNCE);
        assert_eq!(at(20), first + Duration::from_secs(25));
        assert_eq!(at(28), first + DEBOUNCE_MAX_WAIT, "capped");
        assert_eq!(at(400), first + DEBOUNCE_MAX_WAIT);
        assert_eq!(debounce_deadline(first, None, DEBOUNCE, DEBOUNCE_MAX_WAIT), first + DEBOUNCE);
        assert_eq!(DEBOUNCE_MAX_WAIT, Duration::from_secs(30));
    }

    /// Changes every 30 ms keep pushing a 50 ms debounce back, but a cycle
    /// still runs once max-wait (150 ms) after the first change has passed.
    #[tokio::test]
    async fn d2_run_scheduler_runs_by_max_wait_under_a_stream_of_changes() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        let scheduler = tokio::spawn(run_scheduler_with(
            rx,
            move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    CycleOutcome::Success
                }
            },
            || 0.5,
            Duration::from_millis(150),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the first cycle");
        // DEBOUNCE (5 s) never elapses between these changes; only max-wait can fire.
        let started = tokio::time::Instant::now();
        while started.elapsed() < Duration::from_millis(600) {
            tx.send(SchedEvent::LocalChange).unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let n = runs.load(Ordering::SeqCst);
        assert!(n >= 3, "max-wait fired under a change stream: {n} runs");
        tx.send(SchedEvent::Cancel).unwrap();
        scheduler.await.unwrap();
    }

    /// Cancel takes effect at the cycle's current await (here a settle-like
    /// pause that would last an hour): the scheduler returns at once.
    #[tokio::test]
    async fn d2_cancel_drops_a_cycle_parked_in_a_pause() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        let scheduler = tokio::spawn(run_scheduler(
            rx,
            move || {
                let flag = Arc::clone(&flag);
                async move {
                    tokio::time::sleep(Duration::from_secs(3_600)).await;
                    flag.store(true, Ordering::SeqCst);
                    CycleOutcome::Success
                }
            },
            || 0.5,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(SchedEvent::Cancel).unwrap();
        tokio::time::timeout(Duration::from_secs(2), scheduler).await.expect("cancel is prompt").unwrap();
        assert!(!finished.load(Ordering::SeqCst), "the cycle never completed");
    }

    // -- G7 delta decisions (D5a, D5b, D7a, D1a, D1b, D2a, D3a) ---------------

    /// Forwards to a [`View`] and runs `on_put` before every `put_create`
    /// (an app write landing mid-cycle).
    struct OnPut<'a, F: Fn(&str) + Send + Sync> {
        inner: View<'a>,
        on_put: F,
    }

    impl<F: Fn(&str) + Send + Sync> ObjectStore for OnPut<'_, F> {
        fn create_only(&self) -> bool {
            self.inner.create_only()
        }
        fn identity(&self) -> String {
            self.inner.identity()
        }
        async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
            self.inner.list(q).await
        }
        async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
            self.inner.get(key).await
        }
        async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
            (self.on_put)(key);
            self.inner.put_create(key, bytes).await
        }
        async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
            self.inner.put_replace(key, bytes, if_match).await
        }
        async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
            self.inner.put_replace_ref(key, provider_ref, bytes).await
        }
        async fn delete(&self, key: &str) -> Result<(), ProviderError> {
            self.inner.delete(key).await
        }
        async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
            self.inner.get_all(key).await
        }
        async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
            self.inner.delete_ref(provider_ref).await
        }
        async fn quota(&self) -> Result<Quota, ProviderError> {
            self.inner.quota().await
        }
        async fn ensure_container(&self) -> Result<(), ProviderError> {
            self.inner.ensure_container().await
        }
    }

    /// Message ids carried by `device`'s segments on the remote.
    async fn remote_msg_ids(fake: &FakeStore, device: &str) -> BTreeSet<String> {
        remote_ops(fake, VAULT, DEK, device)
            .await
            .into_iter()
            .filter(|o| o.op_type == "msg.add")
            .filter_map(|o| o.payload.get("msg_id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// D5a: an attachment added while this cycle's blobs upload is sealed in
    /// a later cycle or, if sealed now, waits on its own blob: the gate is
    /// the segment's own ops, not timing. No segment carrying `msg-10` is on
    /// the remote before blob 2 is.
    #[tokio::test]
    async fn d5a_attachment_added_mid_cycle_never_publishes_before_its_blob() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        let blob1 = blob_row(&dek, b"PNG-mid-cycle-1");
        let blob2 = blob_row(&dek, b"PNG-mid-cycle-2");
        conv_with_attachment(&a, 1, &blob1);
        let fired = AtomicBool::new(false);
        let store = &a.store;
        let hook = |key: &str| {
            if key == blob_key(&blob1.blob_id) && !fired.swap(true, Ordering::SeqCst) {
                attach_into(store, 10, &blob2);
            }
        };
        let p = Bound::new(OnPut { inner: View { inner: &fake, hides: &hides }, on_put: hook }, binding_fp(&fake.identity()));
        let now = real_now;
        let mut ctx = CycleCtx {
            provider: &p,
            secrets: &a.secrets,
            store,
            clock: &a.clock,
            pause: &NoPause,
            now: &now,
            stats: &a.stats,
            engine: &mut a.engine,
            vault: VAULT,
            key_epoch: EPOCH,
            install_fp: None,
            device_label: "d",
            app_version: "v",
            checkpoint_dir: None,
            fence: None,
        };
        assert_ok(&Engine::cycle(&mut ctx).await);
        assert!(fired.load(Ordering::SeqCst), "the attachment landed mid-cycle");
        let after_first = remote_msg_ids(&fake, "dev-a").await;
        assert_eq!(after_first, BTreeSet::from(["msg-1".to_owned()]), "sealed before the blob step: only msg-1 went out");
        assert!(fake.copies(&blob_key(&blob2.blob_id)).is_empty());

        // Next cycle: blob 2 before the segment that carries msg-10.
        let writes = Writes::default();
        assert_ok(&a.cycle(&counting(&fake, &writes)).await);
        let order = lock(&writes.1).clone();
        let blob_at = order.iter().position(|k| *k == blob_key(&blob2.blob_id)).expect("blob 2 put");
        let seg_at = order.iter().position(|k| k.contains("/log/")).expect("segment put");
        assert!(blob_at < seg_at, "blob 2 before the segment carrying msg-10: {order:?}");
        assert!(remote_msg_ids(&fake, "dev-a").await.contains("msg-10"));
        assert_eq!(fake.copies(&blob_key(&blob2.blob_id)).len(), 1);
        assert_eq!(lock(&a.store).blob_state(VAULT, &blob2.blob_id).unwrap().as_deref(), Some(BLOB_PUBLISHED));
    }

    /// D5a: the gate reads each sealed segment's ops. A segment whose blob
    /// failed closed (still pending) is never put; once that blob row is
    /// settled the same sealed segment goes out unchanged.
    #[tokio::test]
    async fn d5a_sealed_segment_waits_for_the_blob_its_ops_reference() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let blob = blob_row(&dek, b"PNG-gate");
        fake.seed(&blob_key(&blob.blob_id), b"not this blob");
        conv_with_attachment(&a, 1, &blob);
        let report = a.cycle(&p).await;
        assert!(report.notices.contains(&EngineNotice::BlobRejected), "{report:?}");
        let sealed = lock(&a.store).segment_states(VAULT, "dev-a").unwrap();
        assert_eq!(sealed, vec![(1, 2, UploadState::Never)], "sealed first, not put");
        assert_eq!(
            lock(&a.store).segment_blob_ids(VAULT, "dev-a", 1, 2).unwrap(),
            BTreeSet::from([blob.blob_id.clone()]),
            "derived from the segment's own ops"
        );
        assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 2)).is_empty());

        // The blob is settled another way (here: the other content removed
        // and the blob re-uploaded): the waiting segment goes out as sealed.
        let bytes = lock(&a.store).sealed_bytes(VAULT, "dev-a", 1).unwrap().unwrap().bytes;
        fake.delete(&blob_key(&blob.blob_id)).await.unwrap();
        assert_ok(&a.cycle(&p).await);
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 2)), vec![bytes]);
        assert_eq!(lock(&a.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some(BLOB_PUBLISHED));
    }

    /// D5b: an attachment whose sealed object would reach the object cap is
    /// marked `skipped_oversize`, raises one notice, and blocks nothing: its
    /// own segment and later ops publish; a reader sees it evicted.
    #[tokio::test]
    async fn d5b_oversize_blob_is_skipped_once_and_blocks_no_upload() {
        let dek = SecretKey::from_bytes(DEK);
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let blob = blob_row(&dek, &vec![7u8; MAX_OBJECT_BYTES]);
        conv_with_attachment(&a, 1, &blob);
        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(report.notices.iter().filter(|n| **n == EngineNotice::BlobEvicted).count(), 1, "{report:?}");
        assert_eq!(lock(&a.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some(BLOB_SKIPPED_OVERSIZE));
        assert!(fake.copies(&blob_key(&blob.blob_id)).is_empty(), "nothing sent");
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 2)).len(), 1, "its segment is not blocked");

        a.local(create(3, 300, M, "later"));
        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert!(!report.notices.contains(&EngineNotice::BlobEvicted), "the notice is raised once: {report:?}");
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 3, 3)).len(), 1, "later ops publish");

        let mut b = Dev::new("dev-b");
        let report = b.cycle(&p).await;
        assert_ok(&report);
        assert_eq!(lock(&b.store).blob_state(VAULT, &blob.blob_id).unwrap().as_deref(), Some(BLOB_EVICTED));
    }

    /// Counts settle pauses.
    #[derive(Default)]
    struct CountPause(AtomicUsize);

    impl Pause for CountPause {
        async fn pause(&self, _: Duration) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// D7a: back-to-back manifest misses are not a wipe: the miss counts only
    /// after a second read separated by the settle pause. A transient miss
    /// therefore deletes nothing and changes no local state; a confirmed one
    /// stops the cycle and still deletes nothing.
    #[tokio::test]
    async fn d7a_manifest_miss_needs_a_paused_reread_and_never_deletes() {
        let fake = remote(true);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&bound(&fake, &hides)).await);

        // Three misses: check_vaults' two back-to-back reads and the gate's
        // first; the read after the pause sees the manifest.
        hides.manifest_misses.store(3, Ordering::SeqCst);
        a.local(create(2, 200, "memo-2", "y"));
        let pause = CountPause::default();
        let report = a.cycle_with_pause(&bound(&fake, &hides), &pause).await;
        assert_eq!(report.status, CycleStatus::Ok, "a transient miss is no wipe: {report:?}");
        assert_eq!(pause.0.load(Ordering::SeqCst), 1, "the confirmation read waits the settle pause");
        assert_eq!(hides.manifest_misses.load(Ordering::SeqCst), 0);
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).len(), 1);

        // A real wipe (another device): check_vaults' back-to-back misses
        // are confirmed only by the paused re-read; then the cycle stops,
        // puts nothing, deletes nothing, and leaves local state alone.
        let manifest = vault::manifest_key(VAULT).unwrap();
        a.local(create(3, 300, "memo-3", "z"));
        let put = AtomicBool::new(false);
        let hook = |_: &str| put.store(true, Ordering::SeqCst);
        let p = Bound::new(OnPut { inner: View { inner: &fake, hides: &hides }, on_put: hook }, binding_fp(&fake.identity()));
        fake.delete(&manifest).await.unwrap();
        let pause = CountPause::default();
        let report = a.cycle_with_pause(&p, &pause).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
        assert_eq!(pause.0.load(Ordering::SeqCst), 1, "confirmed only after the paused re-read");
        assert!(!put.load(Ordering::SeqCst), "no put after a confirmed miss");
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).len(), 1, "published objects are never deleted");
        assert!(fake.copies(&manifest).is_empty(), "the manifest is not recreated");
        assert_eq!(lock(&a.store).op_state(VAULT, &oid(2)).unwrap().unwrap().2, UploadState::Published);
        assert_eq!(lock(&a.store).op_state(VAULT, &oid(3)).unwrap().unwrap().2, UploadState::Never, "untouched");
    }

    /// D1a: on Drive the record update targets the observed file id (no new
    /// copy); a removal landing between the read and the update is a 404 →
    /// NotFound, the record is never recreated, and the D1 settle guard turns
    /// the read-back record's two misses into DeviceRemoved.
    #[tokio::test]
    async fn d1a_drive_record_update_hits_the_observed_ref_and_never_recreates() {
        let fake = remote(false);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&p).await);
        let first_ref = fake.get_all(&rec_key("dev-a")).await.unwrap()[0].provider_ref.clone();

        a.local(create(2, 200, "memo-2", "y"));
        assert_ok(&a.cycle(&p).await);
        let copies = fake.get_all(&rec_key("dev-a")).await.unwrap();
        assert_eq!(copies.len(), 1, "updated in place, no second copy");
        assert_eq!(copies[0].provider_ref, first_ref, "the observed file id");

        hides.vanish_on_replace_ref.store(true, Ordering::SeqCst);
        a.local(create(3, 300, "memo-3", "z"));
        let report = a.cycle(&p).await;
        assert!(!hides.vanish_on_replace_ref.load(Ordering::SeqCst), "the update by ref ran");
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
        assert!(fake.copies(&rec_key("dev-a")).is_empty(), "a removed record is never recreated");
        let report = a.cycle(&p).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
        assert!(fake.copies(&rec_key("dev-a")).is_empty());
    }

    /// D1b: `rec_verified` needs the bytes read back to hash to the last put.
    /// A lagging read of the previous version (authentic, but older) proves
    /// nothing; the exact bytes of the last put do.
    #[tokio::test]
    async fn d1b_record_is_verified_only_by_the_bytes_of_the_last_put() {
        let fake = remote(true);
        let hides = Hides::default();
        let p = bound(&fake, &hides);
        let mut a = Dev::new("dev-a");
        let verified = |a: &Dev| lock(&a.store).meta_get(&rec_verified_key(VAULT, "dev-a")).unwrap();
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&p).await);
        let v1 = fake.copies(&rec_key("dev-a"))[0].clone();
        a.local(create(2, 200, "memo-2", "y"));
        assert_ok(&a.cycle(&p).await);
        let v2 = fake.copies(&rec_key("dev-a"))[0].clone();
        assert_ne!(v1, v2);
        assert_eq!(verified(&a).as_deref(), Some("0"), "a fresh put is unverified");

        // A lagging read serves v1: authentic, but not what was last put.
        lock(&hides.stale).insert(rec_key("dev-a"), v1);
        assert_ok(&a.cycle(&p).await);
        assert_eq!(verified(&a).as_deref(), Some("0"), "an older version does not verify the last put");

        // The exact bytes of the last put are read back: verified.
        hides.clear();
        assert_ok(&a.cycle(&p).await);
        assert_eq!(fake.copies(&rec_key("dev-a")), vec![v2], "a quiet cycle rewrites nothing");
        assert_eq!(verified(&a).as_deref(), Some("1"));
    }

    /// D2a: a change while backing off moves the timer to
    /// `max(backoff, min(now + DEBOUNCE, first + max-wait))`.
    #[test]
    fn d2a_change_while_backing_off_keeps_the_backoff_and_caps_the_debounce() {
        let t0 = Duration::from_secs(1_000);
        let s = Duration::from_secs;
        let at = |now: u64, backoff: Option<u64>| {
            backoff_change_deadline(t0 + s(now), backoff.map(|b| t0 + s(b)), t0, DEBOUNCE, DEBOUNCE_MAX_WAIT)
        };
        // A short backoff: the debounce slides, capped at max-wait.
        assert_eq!(at(0, Some(3)), t0 + s(5));
        assert_eq!(at(20, Some(3)), t0 + s(25));
        assert_eq!(at(28, Some(3)), t0 + DEBOUNCE_MAX_WAIT, "edits never starve sync past max-wait");
        assert_eq!(at(400, Some(3)), t0 + DEBOUNCE_MAX_WAIT);
        // A long backoff / Retry-After is never shortened.
        assert_eq!(at(1, Some(60)), t0 + s(60));
        assert_eq!(at(40, Some(60)), t0 + s(60));
        // Not backing off: the plain capped debounce.
        assert_eq!(at(28, None), t0 + DEBOUNCE_MAX_WAIT);
    }

    /// D3a: several own manifest copies raise ManifestCopies (the count only)
    /// once they persisted across two consecutive cycles; one copy resets the
    /// streak; the engine deletes no copy.
    #[tokio::test]
    async fn d3a_manifest_copies_notice_after_two_consecutive_cycles() {
        let fake = remote(false);
        let hides = Hides::default();
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&bound(&fake, &hides)).await);
        let manifest = vault::manifest_key(VAULT).unwrap();
        let bytes = fake.copies(&manifest)[0].clone();
        let has = |r: &CycleReport| r.notices.iter().any(|n| matches!(n, EngineNotice::ManifestCopies { .. }));

        fake.seed(&manifest, &bytes);
        let writes = Writes::default();
        let first = a.cycle(&counting(&fake, &writes)).await;
        assert_ok(&first);
        assert!(!has(&first), "one cycle is not persistence: {first:?}");
        let second = a.cycle(&counting(&fake, &writes)).await;
        assert!(second.notices.contains(&EngineNotice::ManifestCopies { count: 2 }), "{second:?}");
        assert_eq!(writes.0.load(Ordering::SeqCst), 0, "no delete, no write");
        assert_eq!(fake.copies(&manifest).len(), 2);

        // Back to one copy: the streak resets.
        let extra = fake.get_all(&manifest).await.unwrap()[1].provider_ref.clone();
        fake.delete_ref(&extra).await.unwrap();
        assert!(!has(&a.cycle(&bound(&fake, &hides)).await));
        fake.seed(&manifest, &bytes);
        assert!(!has(&a.cycle(&bound(&fake, &hides)).await), "the streak restarts");
        assert!(has(&a.cycle(&bound(&fake, &hides)).await));
    }
}
