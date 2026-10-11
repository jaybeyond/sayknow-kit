//! Vault lifecycle (§4.2.2, §4.6.3, §4.7.1): discovery before creation,
//! creation with RK re-entry, post-hoc two-vault detection, join J1–J7, RK
//! reissue, device removal, and remote wipe.
//!
//! Network steps are `async` over an [`ObjectStore`] and never take a
//! [`Store`]; store steps are synchronous. The command layer therefore never
//! holds the store lock across a network `.await`.
//!
//! Decisions (G7 vault):
//! - Discovery lists manifests with `ListQuery::Props { kind: Manifest }` on
//!   every backend: Drive answers it from appProperties; key-addressed stores
//!   map it to the `v1/` prefix plus an exact layout-key filter
//!   (`provider::keyed_listing`), which is the manifest prefix rule.
//!   Tries run at 0/5/15 s; a result counts only when two consecutive tries
//!   are identical. Identical and empty is "none", identical and non-empty is
//!   "found" (the union of everything seen, so a lagging listing cannot hide
//!   a vault). Anything else — an error, the 30 s cap, three tries without
//!   agreement — is `retry`, which forbids creation exactly like "found".
//! - Creation is gated on a fresh `discovery_state = none` (at most
//!   [`DISCOVERY_RETRY_MS`] old). The DEK goes to the keychain only after the
//!   RK re-entry unlocks the new manifest, and before the manifest upload, so
//!   a crash after the upload never loses the only DEK copy.
//! - Join J2 refuses a stream that does not start at seq 1 or has a gap, and
//!   any apply outcome other than `Applied`/`Duplicate` (held, quarantined,
//!   deferred, future version): the join stops with V1 untouched. V2 rows
//!   already applied stay (vault-scoped) and are reused by the next try.
//! - J4 compares `(kind, id, normalized state)`. Normalization drops op ids
//!   (`head_op_id`, message `op_id`) and the merge-only `conflict_of`, and
//!   rewrites V1 attachment blob ids under V2's `K_name`, so "same content"
//!   means byte-identical attachments. A V2 tombstone under the same id is
//!   "different content" (copy), never a silent resurrection of the V2 id.
//!   Copy ids are `uuidv5(join-copy ns, target ‖ kind ‖ id ‖ n)`, the first
//!   `n` whose slot is free (or already holds identical content → skip).
//! - J5 keeps message ids for a republished conversation (V2 does not have
//!   the conversation, so no msg id can collide) and maps them to
//!   `uuidv5(copy_id, old msg id)` for a copy, whose original stays in V2.
//!   An attachment whose bytes are not stored locally (evicted, or the entity
//!   is `evicted_local`) is republished as metadata only (`evicted: true`).
//! - J7: the identity row, active-vault switch, J5 ops and blobs commit in
//!   one transaction ([`Store::commit_vault_switch`]); the commit is refused
//!   unless every planned entity is live in the resulting projection.
//! - RK reissue on a create-only store has no way to stage a second manifest
//!   under the same name, and adding an overwrite path is forbidden. The new
//!   manifest is persisted locally first (`meta.rk_reissue_manifest`), then
//!   the verified old one is deleted and the new one created and verified;
//!   a crash in between resumes from the persisted copy. Devices that look in
//!   that window see a missing manifest and pause; they never recreate it.
//!   On Drive the new copy is created and verified before old copies go.
//! - Wipe deletes only keys inside `v1/<vault>/` (Drive: the
//!   `Props { vault_id }` query, re-checked against the key), relists until
//!   empty, and never touches the DEK before `wipe_state = done`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::crypto::{derive_subkey, hex, hmac_sha256, Purpose, SecretKey};
use super::envelope::{is_id, ObjectAddress};
use super::identity::{save_identity, DeviceIdentity};
use super::merge::{ConvState, EntityKey, EntityState, Message, MemoState};
use super::ops::{canonical_json, HlcClock, Kind, OpBody, FORMAT_V};
use super::provider::{parse_layout_key, ListQuery, ObjectStore, ProviderError, RemoteKind};
use super::publish::{Pause, RECHECK_DELAYS};
use super::recovery::{
    self, create_vault, unlock, verify_manifest, Manifest, RecoveryError, RecoveryKey, INITIAL_KEY_EPOCH,
};
use super::rotation::UploadState;
use super::secrets::{delete_secret, load_vault_key, store_vault_key, SecretError, SecretStore, SyncSecret};
use super::store::{BlobRow, Store, StoreError};

/// Discovery tries at these offsets from the first one (§4.2.2).
pub(crate) const DISCOVERY_OFFSETS: [Duration; 3] =
    [Duration::ZERO, Duration::from_secs(5), Duration::from_secs(15)];
/// Hard cap on one discovery pass.
pub(crate) const DISCOVERY_CAP: Duration = Duration::from_secs(30);
/// `retry` waits this long; a `none` older than this no longer allows a create.
pub(crate) const DISCOVERY_RETRY_MS: u64 = 5 * 60 * 1000;
/// The typed confirmation the user enters before a cloud wipe (§4.7.1).
pub(crate) const WIPE_CONFIRMATION: &str = "삭제";
/// How many list-and-delete rounds a wipe makes before it reports incomplete
/// (the confirming final sweep counts as one).
const WIPE_SWEEPS: usize = 6;
/// The pause before the wipe's final sweep (D7): long enough for a peer
/// cycle that passed its manifest gate to finish its put batch.
pub(crate) const WIPE_FINAL_SWEEP_DELAY: Duration = RECHECK_DELAYS[1];

const META_DISCOVERY_STATE: &str = "discovery_state";
const META_DISCOVERY_AT: &str = "discovery_checked_at";
const META_DISCOVERY_RETRY_AT: &str = "discovery_retry_at";
const META_WIPE_STATE: &str = "wipe_state";
const META_WIPE_VAULT: &str = "wipe_vault";
const META_REISSUE_STATE: &str = "rk_reissue_state";
const META_REISSUE_VAULT: &str = "rk_reissue_vault";
const META_REISSUE_MANIFEST: &str = "rk_reissue_manifest";
const META_REISSUE_BASE: &str = "rk_reissue_base";
const META_JOINED_FROM: &str = "joined_from_vault";

const STATE_NONE: &str = "none";
const STATE_FOUND: &str = "found";
const STATE_RETRY: &str = "retry";
const STATE_STARTED: &str = "started";
const STATE_DONE: &str = "done";
const STATE_PENDING: &str = "pending";

#[derive(Debug)]
pub(crate) enum VaultError {
    Provider(ProviderError),
    Store(StoreError),
    Secret(SecretError),
    Recovery(RecoveryError),
    /// Creation needs a fresh discovery that found no vault.
    DiscoveryRequired,
    /// The RK opened no candidate manifest.
    NoVaultUnlocked,
    /// The RK opened manifests of more than one vault, or copies of one vault
    /// that disagree (D-1: exactly one must succeed).
    Ambiguous,
    /// This device already has an identity in the target vault, or it is
    /// already the active vault.
    AlreadyMember,
    /// J2 could not download and apply the whole target vault.
    JoinIncomplete(String),
    /// The vault changed since the preview; show the fresh one.
    PreviewStale(JoinPreview),
    /// The RK was not re-entered and verified yet.
    NotConfirmed,
    /// Another object holds the manifest name.
    Conflict,
    /// A written or remote manifest did not verify.
    Verification(String),
    /// The wipe confirmation text was not entered exactly.
    ConfirmationMismatch,
    /// This device cannot remove itself; disconnect instead.
    SelfRemoval,
    InvalidId,
    WipeIncomplete(String),
    /// A different vault's wipe or reissue is pending; finish it first.
    Busy,
    NothingPending,
    /// The resulting projection lost a planned entity; nothing committed.
    JoinRejected,
    NoKey,
    /// Another device's reissued manifest replaced the one this reissue
    /// started from (D3): this device's new RK is not confirmed.
    ReissueSuperseded,
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(e) => write!(f, "storage: {e:?}"),
            Self::Store(e) => write!(f, "store: {e}"),
            Self::Secret(e) => write!(f, "keychain: {e}"),
            Self::Recovery(e) => write!(f, "{e}"),
            Self::DiscoveryRequired => f.write_str("look for an existing vault first"),
            Self::NoVaultUnlocked => f.write_str("the recovery key opens no vault here"),
            Self::Ambiguous => f.write_str("the recovery key opens more than one vault"),
            Self::AlreadyMember => f.write_str("this device is already in that vault"),
            Self::JoinIncomplete(why) => write!(f, "vault download incomplete: {why}"),
            Self::PreviewStale(_) => f.write_str("the vault changed; review the preview again"),
            Self::NotConfirmed => f.write_str("re-enter the recovery key first"),
            Self::Conflict => f.write_str("another object holds the vault manifest name"),
            Self::Verification(why) => write!(f, "manifest verification failed: {why}"),
            Self::ConfirmationMismatch => f.write_str("confirmation text does not match"),
            Self::SelfRemoval => f.write_str("this device cannot remove itself"),
            Self::InvalidId => f.write_str("invalid id"),
            Self::WipeIncomplete(why) => write!(f, "cloud wipe incomplete: {why}"),
            Self::Busy => f.write_str("another vault operation is pending"),
            Self::NothingPending => f.write_str("nothing to resume"),
            Self::JoinRejected => f.write_str("join would lose data; nothing changed"),
            Self::NoKey => f.write_str("vault key missing from the keychain"),
            Self::ReissueSuperseded => f.write_str("another device reissued the recovery key first"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<ProviderError> for VaultError {
    fn from(e: ProviderError) -> Self {
        Self::Provider(e)
    }
}

impl From<StoreError> for VaultError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<SecretError> for VaultError {
    fn from(e: SecretError) -> Self {
        Self::Secret(e)
    }
}

impl From<RecoveryError> for VaultError {
    fn from(e: RecoveryError) -> Self {
        Self::Recovery(e)
    }
}

pub(crate) fn manifest_key(vault: &str) -> Option<String> {
    is_id(vault).then(|| format!("v1/{vault}/manifest.json"))
}

fn manifest_bytes(manifest: &Manifest) -> Vec<u8> {
    serde_json::to_vec(manifest).expect("manifest serializes")
}

fn parse_manifest(bytes: &[u8]) -> Option<Manifest> {
    serde_json::from_slice(bytes).ok()
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time in ms (civil-from-days).
pub(crate) fn utc_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------------------------
// Discovery (§4.2.2, AC 21, AC 31)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryReason {
    Error,
    Timeout,
    Inconsistent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Discovery {
    /// Two consecutive identical empty listings: creation is allowed.
    None,
    /// Vault ids to join (sorted). Creation is forbidden.
    Found(Vec<String>),
    /// No trustworthy answer. Creation and uploads are forbidden; `seen` are
    /// vault ids that did show up, offered for joining.
    Retry { reason: RetryReason, seen: Vec<String> },
}

/// Every vault id with a manifest, sorted. Unparseable names are ignored.
async fn list_manifest_ids<P: ObjectStore>(remote: &P) -> Result<BTreeSet<String>, ProviderError> {
    let listed =
        remote.list(ListQuery::Props { vault_id: None, kind: Some(RemoteKind::Manifest), device_id: None }).await?;
    Ok(listed
        .into_iter()
        .filter_map(|object| parse_layout_key(&object.key))
        .filter(|props| props.kind == RemoteKind::Manifest)
        .map(|props| props.vault_id)
        .collect())
}

/// Bounded discovery: at most three listings at 0/5/15 s, capped at 30 s by
/// `now_ms` (an injectable clock), sleeping only through `pause`. Each
/// listing also runs under a real-time timeout of what is left of the cap
/// (D8), so a listing that never answers ends as `Retry(Timeout)` too.
pub(crate) async fn discover<P: ObjectStore>(remote: &P, pause: &impl Pause, now_ms: impl Fn() -> u64) -> Discovery {
    let start = now_ms();
    let started = tokio::time::Instant::now();
    let cap = DISCOVERY_CAP.as_millis() as u64;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut previous: Option<BTreeSet<String>> = None;
    let mut waited = Duration::ZERO;
    let retry = |reason, seen: &BTreeSet<String>| Discovery::Retry { reason, seen: seen.iter().cloned().collect() };
    for offset in DISCOVERY_OFFSETS {
        if offset > waited {
            pause.pause(offset - waited).await;
            waited = offset;
        }
        let used = now_ms().saturating_sub(start);
        if used > cap {
            return retry(RetryReason::Timeout, &seen);
        }
        // Whichever clock is further along bounds this listing.
        let left = Duration::from_millis(cap - used).min(DISCOVERY_CAP.saturating_sub(started.elapsed()));
        let ids = match tokio::time::timeout(left, list_manifest_ids(remote)).await {
            Err(_) => return retry(RetryReason::Timeout, &seen),
            Ok(Ok(ids)) => ids,
            Ok(Err(_)) => return retry(RetryReason::Error, &seen),
        };
        if now_ms().saturating_sub(start) > cap {
            return retry(RetryReason::Timeout, &seen);
        }
        seen.extend(ids.iter().cloned());
        if previous.as_ref() == Some(&ids) {
            return if seen.is_empty() { Discovery::None } else { Discovery::Found(seen.into_iter().collect()) };
        }
        previous = Some(ids);
    }
    retry(RetryReason::Inconsistent, &seen)
}

/// Persists a discovery result: `meta.discovery_state` ∈ none|found|retry,
/// with the retry time 5 min out on `retry`.
pub(crate) fn record_discovery(store: &mut Store, result: &Discovery, now: u64) -> Result<(), StoreError> {
    let state = match result {
        Discovery::None => STATE_NONE,
        Discovery::Found(_) => STATE_FOUND,
        Discovery::Retry { .. } => STATE_RETRY,
    };
    store.meta_set(META_DISCOVERY_STATE, state)?;
    store.meta_set(META_DISCOVERY_AT, &now.to_string())?;
    if matches!(result, Discovery::Retry { .. }) {
        store.meta_set(META_DISCOVERY_RETRY_AT, &(now + DISCOVERY_RETRY_MS).to_string())?;
    }
    Ok(())
}

fn meta_u64(store: &Store, key: &str) -> Result<Option<u64>, StoreError> {
    Ok(store.meta_get(key)?.and_then(|v| v.parse().ok()))
}

/// When a `retry` discovery is due again, or `None` when none is scheduled.
pub(crate) fn discovery_retry_at(store: &Store) -> Result<Option<u64>, StoreError> {
    if store.meta_get(META_DISCOVERY_STATE)?.as_deref() != Some(STATE_RETRY) {
        return Ok(None);
    }
    meta_u64(store, META_DISCOVERY_RETRY_AT)
}

// ---------------------------------------------------------------------------
// Create

/// A vault being created. `recovery_key` is shown once; the caller must have
/// the user re-enter it ([`confirm_create`]) before anything is uploaded.
pub(crate) struct PendingVault {
    vault_id: String,
    recovery_key: RecoveryKey,
    manifest: Manifest,
    binding_fp: String,
    confirmed: bool,
}

impl PendingVault {
    pub(crate) fn vault_id(&self) -> &str {
        &self.vault_id
    }

    /// The RK for the one-time display dialog. Never store or log it.
    pub(crate) fn recovery_key(&self) -> &RecoveryKey {
        &self.recovery_key
    }
}

/// Step 1–3: a new vault id, a new DEK and RK, the manifest. Refused unless
/// the last discovery found nothing and is at most 5 min old.
pub(crate) fn begin_create(store: &Store, binding_fp: &str, now: u64) -> Result<PendingVault, VaultError> {
    let fresh = meta_u64(store, META_DISCOVERY_AT)?.is_some_and(|at| now.saturating_sub(at) <= DISCOVERY_RETRY_MS);
    if store.meta_get(META_DISCOVERY_STATE)?.as_deref() != Some(STATE_NONE) || !fresh {
        return Err(VaultError::DiscoveryRequired);
    }
    let vault_id = uuid::Uuid::new_v4().to_string();
    let created = create_vault(&vault_id, &utc_timestamp(now), binding_fp)?;
    Ok(PendingVault {
        vault_id,
        recovery_key: created.recovery_key,
        manifest: created.manifest,
        binding_fp: binding_fp.to_owned(),
        confirmed: false,
    })
}

/// Step 3: the re-entered RK must unlock the new manifest. Only then is the
/// DEK written to the keychain. A wrong RK changes nothing.
pub(crate) fn confirm_create(
    pending: &mut PendingVault,
    secrets: &impl SecretStore,
    reentered: &str,
) -> Result<(), VaultError> {
    let dek = unlock(&pending.manifest, &pending.vault_id, reentered, &pending.binding_fp)?;
    store_vault_key(secrets, &pending.vault_id, &dek)?;
    pending.confirmed = true;
    Ok(())
}

/// Step 4: the manifest, create-only, then read back and verified with the
/// keychain DEK. An existing object under the name is accepted only when it
/// is byte-identical (a resumed upload).
pub(crate) async fn publish_created<P: ObjectStore>(
    remote: &P,
    secrets: &impl SecretStore,
    pending: &PendingVault,
) -> Result<(), VaultError> {
    if !pending.confirmed {
        return Err(VaultError::NotConfirmed);
    }
    let dek = load_vault_key(secrets, &pending.vault_id)?.ok_or(VaultError::NoKey)?;
    let key = manifest_key(&pending.vault_id).ok_or(VaultError::InvalidId)?;
    let bytes = manifest_bytes(&pending.manifest);
    match remote.put_create(&key, &bytes).await {
        Ok(_) | Err(ProviderError::AlreadyExists) => {}
        Err(e) => return Err(e.into()),
    }
    let stored = remote.get(&key).await?;
    if stored != bytes {
        return Err(VaultError::Conflict);
    }
    let manifest = parse_manifest(&stored).ok_or_else(|| VaultError::Verification("unreadable".to_owned()))?;
    verify_manifest(&manifest, &pending.vault_id, &dek, &pending.binding_fp)
        .map_err(|e| VaultError::Verification(e.to_string()))?;
    Ok(())
}

/// Steps 5–6: activates the published vault. Local data of the previously
/// active vault (if any) becomes the first stream through the J5 path; only
/// after this commit may segments upload.
pub(crate) fn activate_created(
    store: &mut Store,
    secrets: &impl SecretStore,
    pending: &PendingVault,
    install_fp: Option<String>,
    clock: &mut HlcClock,
    now: u64,
) -> Result<JoinCommit, VaultError> {
    if !pending.confirmed {
        return Err(VaultError::NotConfirmed);
    }
    commit_switch(store, secrets, &pending.vault_id, None, install_fp, clock, now)
}

// ---------------------------------------------------------------------------
// Post-hoc detection (every cycle, AC 20)

/// What the engine reads every cycle. It never selects a winner, merges, or
/// deletes: it shows the two-vault banner for `other_vaults` and pauses (never
/// recreates) when `manifest_present` is false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VaultCheck {
    /// False only after two consecutive NotFound reads of the own manifest.
    pub(crate) manifest_present: bool,
    /// Other vault ids with a manifest, sorted.
    pub(crate) other_vaults: Vec<String>,
    /// Copies under the own manifest name. More than one (Drive, after a
    /// reissue or two concurrent ones) is still one vault (D3): a notice,
    /// never a deletion by the engine.
    pub(crate) manifest_copies: usize,
}

pub(crate) async fn check_vaults<P: ObjectStore>(remote: &P, active_vault: &str) -> Result<VaultCheck, ProviderError> {
    let mut ids = list_manifest_ids(remote).await?;
    ids.remove(active_vault);
    let key = manifest_key(active_vault).ok_or_else(|| ProviderError::Permanent("invalid vault id".to_owned()))?;
    let mut manifest_copies = 0;
    for _ in 0..2 {
        match remote.get_all(&key).await {
            Ok(copies) if !copies.is_empty() => {
                manifest_copies = copies.len();
                break;
            }
            Ok(_) | Err(ProviderError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(VaultCheck { manifest_present: manifest_copies > 0, other_vaults: ids.into_iter().collect(), manifest_copies })
}

// ---------------------------------------------------------------------------
// Join J1–J7 (§4.2.2, AC 20, N-3)

/// A manifest copy found under a vault id.
#[derive(Debug, Clone)]
pub(crate) struct JoinCandidate {
    pub(crate) vault_id: String,
    manifest: Manifest,
}

/// Join tries the RK against at most this many copies per vault (D3: Drive
/// may hold several after reissues; a flood of copies cannot stall a join).
pub(crate) const MAX_MANIFEST_COPIES: usize = 8;

/// Every manifest copy of `vault_ids` (Drive may hold several during and
/// after a reissue; they are one vault). Unparseable copies are skipped; they
/// can never unlock. At most [`MAX_MANIFEST_COPIES`] per vault.
pub(crate) async fn fetch_candidates<P: ObjectStore>(
    remote: &P,
    vault_ids: &[String],
) -> Result<Vec<JoinCandidate>, VaultError> {
    let mut out = Vec::new();
    for vault in vault_ids {
        let key = manifest_key(vault).ok_or(VaultError::InvalidId)?;
        let copies = remote.get_all(&key).await?;
        out.extend(
            copies
                .iter()
                .filter_map(|copy| parse_manifest(&copy.bytes))
                .take(MAX_MANIFEST_COPIES)
                .map(|manifest| JoinCandidate { vault_id: vault.clone(), manifest }),
        );
    }
    Ok(out)
}

/// The vault the RK opened (J1). The DEK is in the keychain, inactive.
pub(crate) struct UnlockedVault {
    pub(crate) vault_id: String,
    key_epoch: u32,
}

/// D-1: unlock every candidate with the RK; exactly one vault must open, and
/// its copies (D3: several after a reissue) must agree on the DEK. Returns
/// `(vault, DEK, key_epoch)`. Pure: nothing is stored.
fn open_with_rk(
    candidates: &[JoinCandidate],
    recovery_text: &str,
    binding_fp: &str,
) -> Result<(String, SecretKey, u32), VaultError> {
    RecoveryKey::parse(recovery_text)?;
    let mut opened: BTreeMap<String, (SecretKey, String, u32)> = BTreeMap::new();
    let mut first_error = None;
    for candidate in candidates {
        match unlock(&candidate.manifest, &candidate.vault_id, recovery_text, binding_fp) {
            Ok(dek) => {
                let check = candidate.manifest.key_check.clone();
                let epoch = candidate.manifest.key_epoch;
                if let Some((_, known, _)) = opened.get(&candidate.vault_id) {
                    if *known != check {
                        return Err(VaultError::Ambiguous);
                    }
                } else {
                    opened.insert(candidate.vault_id.clone(), (dek, check, epoch));
                }
            }
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    if opened.len() > 1 {
        return Err(VaultError::Ambiguous);
    }
    let Some((vault_id, (dek, _, key_epoch))) = opened.into_iter().next() else {
        return Err(match first_error {
            Some(e @ (RecoveryError::BindingMismatch | RecoveryError::Tampered | RecoveryError::Unsupported)) => e.into(),
            _ => VaultError::NoVaultUnlocked,
        });
    };
    Ok((vault_id, dek, key_epoch))
}

/// D-1 + J1: the RK must open exactly one vault (see [`open_with_rk`]). Only
/// then is the DEK stored. A wrong RK, or a vault this device is already in,
/// changes nothing.
pub(crate) fn unlock_join(
    store: &Store,
    secrets: &impl SecretStore,
    candidates: &[JoinCandidate],
    recovery_text: &str,
    binding_fp: &str,
) -> Result<UnlockedVault, VaultError> {
    let (vault_id, dek, key_epoch) = open_with_rk(candidates, recovery_text, binding_fp)?;
    if store.active_vault()?.as_deref() == Some(vault_id.as_str()) || store.local_identity(&vault_id)?.is_some() {
        return Err(VaultError::AlreadyMember);
    }
    store_vault_key(secrets, &vault_id, &dek)?;
    Ok(UnlockedVault { vault_id, key_epoch })
}

/// D1 rejoin of the ACTIVE vault after this device was removed: RK re-entry
/// (it must open the active vault's manifest and agree with the keychain
/// DEK), then one rotation to a fresh device id in the same vault_id —
/// keychain identity first, then one transaction (`Store::apply_rotation`).
/// Ops already published stay as they are under the old, now retired id;
/// unpublished own ops are re-sealed under the new stream by the next cycle
/// (never-sent ops keep their ids; attempted or rejected ones are reissued
/// with fresh op ids, since the removed id may never publish again). Nothing
/// remote is written here; the caller fences the engine and resumes it.
pub(crate) fn rejoin(
    store: &mut Store,
    secrets: &impl SecretStore,
    candidates: &[JoinCandidate],
    recovery_text: &str,
    binding_fp: &str,
    install_fp: Option<String>,
) -> Result<JoinCommit, VaultError> {
    let vault = store.active_vault()?.ok_or(VaultError::NothingPending)?;
    let own: Vec<JoinCandidate> = candidates.iter().filter(|c| c.vault_id == vault).cloned().collect();
    let (opened, dek, _) = open_with_rk(&own, recovery_text, binding_fp)?;
    if opened != vault {
        return Err(VaultError::NoVaultUnlocked);
    }
    match load_vault_key(secrets, &vault)? {
        Some(known) if known.as_bytes() != dek.as_bytes() => {
            return Err(VaultError::Verification("the keychain holds another key".to_owned()))
        }
        Some(_) => {}
        None => store_vault_key(secrets, &vault, &dek)?,
    }
    let old = store.local_identity(&vault)?.ok_or(VaultError::NothingPending)?.device_id;
    // The removed id never settles an attempted upload again: those ops are
    // treated as rejected (reissued under the new id, old ids quarantined).
    for (first, _, state) in store.segment_states(&vault, &old)? {
        if state == UploadState::Attempted {
            store.mark_rejected(&vault, &old, first)?;
        }
    }
    let remote = BTreeMap::new();
    let source = store.rotation_input(&vault, remote.clone())?;
    let identity = loop {
        let candidate = DeviceIdentity::new(&vault, install_fp.clone()).ok_or(VaultError::InvalidId)?;
        if candidate.device_id != old {
            break candidate;
        }
    };
    let plan = Store::plan_rotation(&source, &identity.device_id, || uuid::Uuid::now_v7().to_string())?;
    save_identity(secrets, &identity)?;
    let projection_hash = store.apply_rotation(&vault, &plan, &remote, &identity)?;
    if let Some(fp) = &install_fp {
        store.meta_set("install_fp", fp)?;
    }
    Ok(JoinCommit { vault_id: vault, device_id: identity.device_id, ops: plan.rewritten.len(), blobs: 0, projection_hash })
}

/// Every snapshot and segment copy of the target vault (J2 download).
pub(crate) struct TargetDownload {
    vault_id: String,
    download: super::engine::VaultDownload,
}

/// J2 download (D4), through the engine's §4.5 listing: every snapshot and
/// every segment copy of the target (Props, so Drive uses appProperties).
/// With no snapshot, a stream that does not start at seq 1 or has a gap
/// (list lag) fails the join before anything local changes; a compacted
/// vault is read through its snapshots.
pub(crate) async fn fetch_target<P: ObjectStore>(remote: &P, vault: &str) -> Result<TargetDownload, VaultError> {
    if !is_id(vault) {
        return Err(VaultError::InvalidId);
    }
    let download = super::engine::fetch_vault(remote, vault, INITIAL_KEY_EPOCH).await?;
    if download.uncovered_gap() {
        return Err(VaultError::JoinIncomplete("a device stream is not contiguous and no snapshot covers it".to_owned()));
    }
    Ok(TargetDownload { vault_id: vault.to_owned(), download })
}

/// J2 apply (D4): the latest snapshots, then each stream's tail past its
/// cursor, through the engine's catch-up apply path, into the target's
/// vault-scoped rows. The active vault is untouched (entities mirror only
/// the active vault), so any failure leaves V1 exactly as it was. Fails
/// closed (`JoinIncomplete`) on a gap no snapshot covers, a held or
/// quarantined stream, or a newer format. Idempotent: a retry of the same
/// download re-applies nothing new. Returns the objects applied.
pub(crate) fn apply_target(
    store: &mut Store,
    secrets: &impl SecretStore,
    unlocked: &UnlockedVault,
    download: &TargetDownload,
) -> Result<usize, VaultError> {
    let vault = unlocked.vault_id.as_str();
    if download.vault_id != vault {
        return Err(VaultError::InvalidId);
    }
    if store.active_vault()?.as_deref() == Some(vault) || store.local_identity(vault)?.is_some() {
        return Err(VaultError::AlreadyMember);
    }
    let dek = load_vault_key(secrets, vault)?.ok_or(VaultError::NoKey)?;
    let tally = super::engine::apply_vault_download(store, &dek, vault, unlocked.key_epoch, None, &download.download)?;
    let reason = if tally.future_version > 0 {
        Some("written by a newer version")
    } else if tally.quarantined > 0 {
        Some("an object failed verification")
    } else if tally.held > 0 {
        Some("a stream broke")
    } else if tally.gaps > 0 {
        Some("a device stream has a gap no snapshot covers")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(VaultError::JoinIncomplete(reason.to_owned()));
    }
    // A retry must not read what an earlier try held or quarantined (the
    // joiner has no other V2 rows) as complete.
    let failed: i64 = store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM cursors WHERE vault_id = ?1 AND state = 'held')
                  + (SELECT COUNT(*) FROM quarantine WHERE vault_id = ?1)",
            [vault],
            |r| r.get(0),
        )
        .map_err(StoreError::from)?;
    if failed > 0 {
        return Err(VaultError::JoinIncomplete("a stream broke or failed verification".to_owned()));
    }
    Ok(tally.applied)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JoinAction {
    /// Same id and content already in the target.
    Skip,
    /// Absent from the target: republished under the same id.
    Republish,
    /// Same id, different content (or a target tombstone): a new id.
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct JoinItem {
    pub(crate) kind: Kind,
    pub(crate) source_id: String,
    pub(crate) target_id: String,
    pub(crate) action: JoinAction,
    /// Content evicted locally: republished as metadata only.
    pub(crate) evicted: bool,
}

/// J4: what the commit will do, shown to the user before anything changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JoinPreview {
    pub(crate) source_vault: Option<String>,
    pub(crate) target_vault: String,
    pub(crate) items: Vec<JoinItem>,
    /// V1 tombstones (never republished).
    pub(crate) tombstones: usize,
    pub(crate) fingerprint: String,
}

/// The content-free part of the preview the webview may receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct JoinSummary {
    pub(crate) skip: usize,
    pub(crate) republish: usize,
    pub(crate) copy: usize,
    pub(crate) evicted: usize,
    pub(crate) tombstones: usize,
}

impl JoinPreview {
    pub(crate) fn summary(&self) -> JoinSummary {
        let count = |action| self.items.iter().filter(|i| i.action == action).count();
        JoinSummary {
            skip: count(JoinAction::Skip),
            republish: count(JoinAction::Republish),
            copy: count(JoinAction::Copy),
            evicted: self.items.iter().filter(|i| i.evicted && i.action != JoinAction::Skip).count(),
            tombstones: self.tombstones,
        }
    }
}

/// The source side of a switch: the active vault's entities, their locally
/// stored attachment bytes, and the locally evicted entities.
struct Source {
    vault: Option<String>,
    entities: BTreeMap<EntityKey, EntityState>,
    blobs: BTreeMap<String, (String, Vec<u8>)>,
    evicted: BTreeSet<(String, String)>,
}

fn load_source(store: &Store) -> Result<Source, StoreError> {
    let Some(vault) = store.active_vault()? else {
        return Ok(Source { vault: None, entities: BTreeMap::new(), blobs: BTreeMap::new(), evicted: BTreeSet::new() });
    };
    let entities = store.projection(&vault)?.entities;
    let mut blobs = BTreeMap::new();
    for state in entities.values() {
        if let EntityState::Conv(conv) = state {
            for attachment in conv.messages.values().flat_map(|m| &m.attachments) {
                if let Some(id) = attachment.get("blob_id").and_then(Value::as_str) {
                    if !blobs.contains_key(id) {
                        if let Some(found) = store.blob_bytes(id)? {
                            blobs.insert(id.to_owned(), found);
                        }
                    }
                }
            }
        }
    }
    Ok(Source { vault: Some(vault), entities, blobs, evicted: store.evicted_entities()? })
}

/// A source attachment as it will exist in the target: the bytes renamed
/// under the target's `K_name`, or metadata only when the bytes are gone.
fn map_attachment(
    attachment: &Value,
    evicted: bool,
    source_blobs: &BTreeMap<String, (String, Vec<u8>)>,
    name_key: &SecretKey,
    out: &mut BTreeMap<String, BlobRow>,
) -> Value {
    let Some(obj) = attachment.as_object() else { return attachment.clone() };
    let Some(old) = obj.get("blob_id").and_then(Value::as_str) else { return attachment.clone() };
    let mut mapped: Map<String, Value> = obj.clone();
    match source_blobs.get(old).filter(|_| !evicted) {
        Some((mime, bytes)) => {
            let blob_id = hex(&hmac_sha256(name_key, bytes));
            mapped.insert("blob_id".to_owned(), json!(blob_id));
            out.entry(blob_id.clone()).or_insert_with(|| BlobRow {
                blob_id,
                mime: mime.clone(),
                bytes: bytes.clone(),
                sha256: Sha256::digest(bytes).into(),
            });
        }
        None => {
            mapped.remove("blob_id");
            mapped.insert("evicted".to_owned(), json!(true));
        }
    }
    Value::Object(mapped)
}

/// `(kind, id, state)` content for the J4 comparison: no op ids, no
/// merge-only fields. `attach` maps each attachment into target terms.
fn normalized(state: &EntityState, attach: &mut dyn FnMut(&Value) -> Value) -> Option<Value> {
    Some(match state {
        EntityState::Deleted => return None,
        EntityState::Memo(MemoState { text, pinned, note, .. }) => json!({"memo": {"text": text, "pinned": pinned, "note": note}}),
        EntityState::Conv(ConvState { title, active_leaf, messages }) => {
            let messages: Map<String, Value> = messages
                .iter()
                .map(|(id, m)| {
                    let attachments: Vec<Value> = m.attachments.iter().map(&mut *attach).collect();
                    let value = json!({
                        "parent_id": m.parent_id, "role": m.role, "content": m.content,
                        "model": m.model, "ts": m.ts, "attachments": attachments,
                    });
                    (id.clone(), value)
                })
                .collect();
            json!({"conv": {"title": title, "active_leaf": active_leaf, "messages": messages}})
        }
        EntityState::Tr(tr) => json!({"tr": {"data": tr.data, "pinned": tr.pinned}}),
        EntityState::Policy(policy) => json!({"policy": {"data": policy.data}}),
    })
}

fn join_namespace() -> uuid::Uuid {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"sayknow/v1/join-copy")
}

fn copy_id(target: &str, kind: Kind, source_id: &str, n: u32) -> String {
    uuid::Uuid::new_v5(&join_namespace(), format!("{target}\n{}\n{source_id}\n{n}", kind.as_str()).as_bytes())
        .to_string()
}

fn copy_msg_id(copy: &str, msg_id: &str) -> String {
    uuid::Uuid::new_v5(&join_namespace(), format!("msg\n{copy}\n{msg_id}").as_bytes()).to_string()
}

fn plan(
    source: &Source,
    target_vault: &str,
    target: &BTreeMap<EntityKey, EntityState>,
    name_key: &SecretKey,
) -> JoinPreview {
    let mut items = Vec::new();
    let mut tombstones = 0;
    let mut reserved: BTreeSet<EntityKey> = BTreeSet::new();
    let mut scratch = BTreeMap::new();
    let target_norm = |key: &EntityKey| target.get(key).map(|s| normalized(s, &mut |a: &Value| a.clone()));
    for ((kind, id), state) in &source.entities {
        let evicted = source.evicted.contains(&(kind.as_str().to_owned(), id.clone()));
        let mut attach = |a: &Value| map_attachment(a, evicted, &source.blobs, name_key, &mut scratch);
        let Some(mine) = normalized(state, &mut attach) else {
            tombstones += 1;
            continue;
        };
        let (action, target_id) = match target_norm(&(*kind, id.clone())) {
            None => (JoinAction::Republish, id.clone()),
            Some(Some(theirs)) if theirs == mine => (JoinAction::Skip, id.clone()),
            Some(_) => {
                let mut n = 0;
                loop {
                    let candidate = copy_id(target_vault, *kind, id, n);
                    let key = (*kind, candidate.clone());
                    if !reserved.contains(&key) {
                        match target_norm(&key) {
                            None => break (JoinAction::Copy, candidate),
                            Some(Some(theirs)) if theirs == mine => break (JoinAction::Skip, candidate),
                            Some(_) => {}
                        }
                    }
                    n += 1;
                }
            }
        };
        reserved.insert((*kind, target_id.clone()));
        items.push(JoinItem { kind: *kind, source_id: id.clone(), target_id, action, evicted });
    }
    items.sort();
    let fingerprint_input = json!({
        "source": source.vault,
        "target": target_vault,
        "tombstones": tombstones,
        "items": items.iter().map(|i| json!([i.kind.as_str(), i.source_id, i.target_id, i.action, i.evicted])).collect::<Vec<_>>(),
    });
    let fingerprint = hex(&Sha256::digest(canonical_json(&fingerprint_input).expect("plan is canonical")));
    JoinPreview { source_vault: source.vault.clone(), target_vault: target_vault.to_owned(), items, tombstones, fingerprint }
}

fn fresh_op(clock: &mut HlcClock, now: u64, kind: Kind, entity: &str, op_type: &str, payload: Value) -> OpBody {
    OpBody {
        v: FORMAT_V,
        op_id: uuid::Uuid::now_v7().hyphenated().to_string(),
        origin_device: "unset".to_owned(),
        hlc: clock.tick(now),
        ctx: BTreeMap::new(),
        obs: Vec::new(),
        kind,
        entity_id: entity.to_owned(),
        op_type: op_type.to_owned(),
        payload,
    }
}

/// Parents before children; siblings by (ts, id). Anything left by a cycle
/// follows in (ts, id) order.
fn message_order(messages: &BTreeMap<String, Message>) -> Vec<&str> {
    let mut left: Vec<(&String, &Message)> = messages.iter().collect();
    left.sort_by(|(ia, a), (ib, b)| (a.ts, ia).cmp(&(b.ts, ib)));
    let mut done: BTreeSet<&str> = BTreeSet::new();
    let mut order = Vec::with_capacity(left.len());
    loop {
        let before = order.len();
        left.retain(|(id, m)| {
            let ready = m.parent_id.as_deref().is_none_or(|p| done.contains(p) || !messages.contains_key(p));
            if ready {
                done.insert(id.as_str());
                order.push(id.as_str());
            }
            !ready
        });
        if left.is_empty() || order.len() == before {
            break;
        }
    }
    order.extend(left.iter().map(|(id, _)| id.as_str()));
    order
}

/// J5: the ops recreating one source entity's current state under
/// `item.target_id`, with fresh op ids (UUIDv7). Ctx and dots are assigned by
/// the store on the new identity.
#[allow(clippy::too_many_arguments)]
fn emit(
    item: &JoinItem,
    state: &EntityState,
    source: &Source,
    name_key: &SecretKey,
    clock: &mut HlcClock,
    now: u64,
    ops: &mut Vec<OpBody>,
    blobs: &mut BTreeMap<String, BlobRow>,
) {
    let target = item.target_id.as_str();
    let mut push = |kind, op_type: &str, payload: Value| ops.push(fresh_op(clock, now, kind, target, op_type, payload));
    match state {
        EntityState::Deleted => {}
        EntityState::Memo(memo) => {
            push(Kind::Memo, "create", json!({"text": memo.text, "pinned": memo.pinned}));
            if !memo.note.is_empty() {
                push(Kind::Memo, "note", json!({"note": memo.note}));
            }
        }
        EntityState::Conv(conv) => {
            let copy = item.action == JoinAction::Copy;
            let msg_id = |id: &str| if copy { copy_msg_id(target, id) } else { id.to_owned() };
            push(Kind::Conv, "create", json!({"title": conv.title}));
            for id in message_order(&conv.messages) {
                let m = &conv.messages[id];
                let attachments: Vec<Value> = m
                    .attachments
                    .iter()
                    .map(|a| map_attachment(a, item.evicted, &source.blobs, name_key, blobs))
                    .collect();
                let mut payload = Map::new();
                payload.insert("conv_id".to_owned(), json!(target));
                payload.insert("msg_id".to_owned(), json!(msg_id(id)));
                if let Some(parent) = &m.parent_id {
                    payload.insert("parent_id".to_owned(), json!(msg_id(parent)));
                }
                payload.insert("role".to_owned(), json!(m.role));
                payload.insert("content".to_owned(), json!(m.content));
                if let Some(model) = &m.model {
                    payload.insert("model".to_owned(), json!(model));
                }
                payload.insert("attachments".to_owned(), Value::Array(attachments));
                payload.insert("ts".to_owned(), json!(m.ts));
                push(Kind::Conv, "msg.add", Value::Object(payload));
            }
            if let Some(leaf) = &conv.active_leaf {
                push(Kind::Conv, "set_active", json!({"leaf": msg_id(leaf)}));
            }
        }
        EntityState::Tr(tr) => {
            push(Kind::Tr, "tr.add", tr.data.clone());
            let added_pinned = tr.data.get("pinned").and_then(Value::as_bool).unwrap_or(false);
            if added_pinned != tr.pinned {
                push(Kind::Tr, "tr.pin", json!({"pinned": tr.pinned}));
            }
        }
        EntityState::Policy(policy) => push(Kind::Policy, "policy.retention", policy.data.clone()),
    }
}

fn name_key(secrets: &impl SecretStore, vault: &str) -> Result<SecretKey, VaultError> {
    let dek = load_vault_key(secrets, vault)?.ok_or(VaultError::NoKey)?;
    Ok(derive_subkey(&dek, vault, INITIAL_KEY_EPOCH, Purpose::BlobName))
}

/// J4: the preview against the target rows J2 applied. Read-only.
pub(crate) fn preview_join(
    store: &Store,
    secrets: &impl SecretStore,
    unlocked: &UnlockedVault,
) -> Result<JoinPreview, VaultError> {
    let source = load_source(store)?;
    let target = store.projection(&unlocked.vault_id)?.entities;
    Ok(plan(&source, &unlocked.vault_id, &target, &name_key(secrets, &unlocked.vault_id)?))
}

/// What a committed switch did (content-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JoinCommit {
    pub(crate) vault_id: String,
    pub(crate) device_id: String,
    pub(crate) ops: usize,
    pub(crate) blobs: usize,
    pub(crate) projection_hash: [u8; 32],
}

/// J3–J7. The plan is recomputed; it must still match the previewed
/// fingerprint, else [`VaultError::PreviewStale`] carries the fresh preview.
pub(crate) fn commit_join(
    store: &mut Store,
    secrets: &impl SecretStore,
    unlocked: &UnlockedVault,
    preview: &JoinPreview,
    install_fp: Option<String>,
    clock: &mut HlcClock,
    now: u64,
) -> Result<JoinCommit, VaultError> {
    if preview.target_vault != unlocked.vault_id {
        return Err(VaultError::InvalidId);
    }
    commit_switch(store, secrets, &unlocked.vault_id, Some(&preview.fingerprint), install_fp, clock, now)
}

#[cfg(test)]
thread_local! {
    /// Test-only crash seam (ADV-3): the next J7 validator rejects, so the
    /// transaction rolls back as if the process died before COMMIT.
    pub(crate) static CRASH_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn crash_before_commit() -> bool {
    CRASH_BEFORE_COMMIT.with(|c| c.replace(false))
}

#[cfg(not(test))]
fn crash_before_commit() -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
fn commit_switch(
    store: &mut Store,
    secrets: &impl SecretStore,
    target_vault: &str,
    expected_fingerprint: Option<&str>,
    install_fp: Option<String>,
    clock: &mut HlcClock,
    now: u64,
) -> Result<JoinCommit, VaultError> {
    if store.active_vault()?.as_deref() == Some(target_vault) || store.local_identity(target_vault)?.is_some() {
        return Err(VaultError::AlreadyMember);
    }
    let name_key = name_key(secrets, target_vault)?;
    let source = load_source(store)?;
    let target = store.projection(target_vault)?.entities;
    let preview = plan(&source, target_vault, &target, &name_key);
    if expected_fingerprint.is_some_and(|expected| expected != preview.fingerprint) {
        return Err(VaultError::PreviewStale(preview));
    }

    // J3: a new identity in the target (keychain first; a crash before J7
    // leaves an unpublished keychain id that the next try replaces).
    let source_device = match &source.vault {
        Some(vault) => store.local_identity(vault)?.map(|local| local.device_id),
        None => None,
    };
    let identity = loop {
        let candidate = DeviceIdentity::new(target_vault, install_fp.clone()).ok_or(VaultError::InvalidId)?;
        if source_device.as_deref() != Some(candidate.device_id.as_str()) {
            break candidate;
        }
    };
    save_identity(secrets, &identity)?;

    // J5 (J6: only current live state of the source; no source op ids, dots,
    // cursors, tombstones, forks, retired devices, or remote index).
    let mut ops = Vec::new();
    let mut blobs = BTreeMap::new();
    for item in preview.items.iter().filter(|i| i.action != JoinAction::Skip) {
        if let Some(state) = source.entities.get(&(item.kind, item.source_id.clone())) {
            emit(item, state, &source, &name_key, clock, now, &mut ops, &mut blobs);
        }
    }
    let mut meta = Vec::new();
    if let Some(from) = &source.vault {
        meta.push((META_JOINED_FROM.to_owned(), from.clone()));
    }
    let blob_count = blobs.len();
    let expected: Vec<EntityKey> = preview.items.iter().map(|i| (i.kind, i.target_id.clone())).collect();
    // J7: one transaction.
    let crash = crash_before_commit();
    let stored = store.commit_vault_switch(&identity, ops, blobs.into_values().collect(), meta, |projection| {
        !crash
            && expected
                .iter()
                .all(|key| projection.entities.get(key).is_some_and(|s| !matches!(s, EntityState::Deleted)))
    })?;
    let Some(stored) = stored else {
        return Err(VaultError::JoinRejected);
    };
    let projection_hash = match stored.last() {
        Some(op) => op.projection_hash,
        None => store.projection(target_vault)?.hash,
    };
    Ok(JoinCommit {
        vault_id: target_vault.to_owned(),
        device_id: identity.device_id,
        ops: stored.len(),
        blobs: blob_count,
        projection_hash,
    })
}

// ---------------------------------------------------------------------------
// RK reissue (§4.6.3, AC 30)

/// A reissue awaiting RK re-entry. Nothing is persisted or uploaded yet.
pub(crate) struct PendingReissue {
    vault_id: String,
    recovery_key: RecoveryKey,
    manifest: Manifest,
    binding_fp: String,
    /// The verified copies the rewrap started from: provider_ref → SHA-256
    /// hex of their bytes. Only these may ever be replaced or deleted (D3).
    base: BTreeMap<String, String>,
}

impl PendingReissue {
    /// The new RK for the one-time display dialog. Never store or log it.
    pub(crate) fn recovery_key(&self) -> &RecoveryKey {
        &self.recovery_key
    }
}

/// Status for the UI: a reissue only blocks new joins with the old RK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ReissueStatus {
    /// Always true: existing devices and holders of an old manifest copy
    /// keep access (v1 has no cryptographic revocation; G9 wording).
    pub(crate) not_revocation: bool,
}

/// A confirmed, unfinished reissue as persisted: the new manifest and the
/// copies it replaces. Content-free in `Debug` (the manifest wraps the DEK).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ReissueRecord {
    new_manifest: Vec<u8>,
    base: BTreeMap<String, String>,
}

impl fmt::Debug for ReissueRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReissueRecord").field("bytes", &self.new_manifest.len()).field("base", &self.base.len()).finish()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Reads and verifies the current manifest copies with the keychain DEK,
/// then rewraps the same DEK under a new RK. Every verified copy observed
/// here is the reissue's base.
pub(crate) async fn begin_reissue<P: ObjectStore>(
    remote: &P,
    secrets: &impl SecretStore,
    vault: &str,
    binding_fp: &str,
) -> Result<PendingReissue, VaultError> {
    let dek = load_vault_key(secrets, vault)?.ok_or(VaultError::NoKey)?;
    let key = manifest_key(vault).ok_or(VaultError::InvalidId)?;
    let mut base = BTreeMap::new();
    let mut current = None;
    for copy in remote.get_all(&key).await? {
        let Some(manifest) = parse_manifest(&copy.bytes) else { continue };
        if verify_manifest(&manifest, vault, &dek, binding_fp).is_ok() {
            base.insert(copy.provider_ref.clone(), sha256_hex(&copy.bytes));
            current.get_or_insert(manifest);
        }
    }
    let current = current.ok_or_else(|| VaultError::Verification("no verified manifest".to_owned()))?;
    let next = recovery::rewrap(&current, &dek, binding_fp)?;
    Ok(PendingReissue {
        vault_id: vault.to_owned(),
        recovery_key: next.recovery_key,
        manifest: next.manifest,
        binding_fp: binding_fp.to_owned(),
        base,
    })
}

/// The re-entered new RK must unlock the new manifest; then the manifest and
/// its base are persisted (`rk_reissue_state = pending`) so a crash resumes it.
pub(crate) fn confirm_reissue(store: &mut Store, pending: &PendingReissue, reentered: &str) -> Result<(), VaultError> {
    unlock(&pending.manifest, &pending.vault_id, reentered, &pending.binding_fp)?;
    if store.meta_get(META_REISSUE_STATE)?.as_deref() == Some(STATE_PENDING)
        && store.meta_get(META_REISSUE_VAULT)?.as_deref() != Some(pending.vault_id.as_str())
    {
        return Err(VaultError::Busy);
    }
    let json = String::from_utf8(manifest_bytes(&pending.manifest)).expect("manifest json is utf-8");
    let base = serde_json::to_string(&pending.base).expect("base serializes");
    store.meta_set(META_REISSUE_MANIFEST, &json)?;
    store.meta_set(META_REISSUE_BASE, &base)?;
    store.meta_set(META_REISSUE_VAULT, &pending.vault_id)?;
    store.meta_set(META_REISSUE_STATE, STATE_PENDING)?;
    Ok(())
}

/// `(vault, record)` of a confirmed, unfinished reissue.
pub(crate) fn reissue_pending(store: &Store) -> Result<Option<(String, ReissueRecord)>, StoreError> {
    if store.meta_get(META_REISSUE_STATE)?.as_deref() != Some(STATE_PENDING) {
        return Ok(None);
    }
    match (store.meta_get(META_REISSUE_VAULT)?, store.meta_get(META_REISSUE_MANIFEST)?) {
        (Some(vault), Some(json)) => {
            // A missing or unreadable base replaces nothing (never a clobber).
            let base = store.meta_get(META_REISSUE_BASE)?.and_then(|b| serde_json::from_str(&b).ok()).unwrap_or_default();
            Ok(Some((vault, ReissueRecord { new_manifest: json.into_bytes(), base })))
        }
        _ => Ok(None),
    }
}

/// How often a create-only swap re-reads after a 412 whose cause is still
/// our base (a lagging listing etag) before it reports a transient failure.
const REISSUE_SWAP_TRIES: usize = 3;

/// What the manifest name holds right now, judged against a reissue.
enum ManifestState {
    /// Our new manifest is in place.
    Done,
    /// A copy of the base is still there (keyed stores: the only copy).
    Base,
    /// Another device's verified manifest replaced the base (D3: abort).
    Superseded,
    /// An object that does not verify under this vault's DEK.
    Foreign,
    /// No manifest at all (a wipe?): never recreated by a reissue.
    Missing,
}

fn judge_manifest(
    copies: &[crate::sync::publish::RemoteObject],
    record: &ReissueRecord,
    ours: &dyn Fn(&[u8]) -> bool,
) -> ManifestState {
    if copies.iter().any(|c| c.bytes == record.new_manifest) {
        return ManifestState::Done;
    }
    if copies.iter().any(|c| record.base.get(&c.provider_ref).is_some_and(|sha| *sha == sha256_hex(&c.bytes))) {
        return ManifestState::Base;
    }
    if copies.is_empty() {
        ManifestState::Missing
    } else if copies.iter().any(|c| ours(&c.bytes)) {
        ManifestState::Superseded
    } else {
        ManifestState::Foreign
    }
}

/// Puts the new manifest in place without ever leaving the name empty (D3).
/// Keyed stores: `put_replace` with If-Match on the etag listed right before
/// the base bytes were re-read (the one sanctioned manifest replace: same
/// vault_id, same DEK, verified). Drive: create the new copy, verify it, then
/// `delete_ref` only base copies observed by [`begin_reissue`]. A different
/// verified manifest (a concurrent reissue) is [`VaultError::ReissueSuperseded`]
/// and is never touched; an unverified object is [`VaultError::Conflict`].
/// Resumable: a landed new manifest only finishes the base cleanup.
pub(crate) async fn publish_reissue<P: ObjectStore>(
    remote: &P,
    secrets: &impl SecretStore,
    vault: &str,
    record: &ReissueRecord,
    binding_fp: &str,
) -> Result<ReissueStatus, VaultError> {
    let dek = load_vault_key(secrets, vault)?.ok_or(VaultError::NoKey)?;
    let key = manifest_key(vault).ok_or(VaultError::InvalidId)?;
    let new_manifest =
        parse_manifest(&record.new_manifest).ok_or_else(|| VaultError::Verification("pending".to_owned()))?;
    verify_manifest(&new_manifest, vault, &dek, binding_fp).map_err(|e| VaultError::Verification(e.to_string()))?;
    let ours = |bytes: &[u8]| parse_manifest(bytes).is_some_and(|m| verify_manifest(&m, vault, &dek, binding_fp).is_ok());
    let refuse = |state: ManifestState| match state {
        ManifestState::Superseded => VaultError::ReissueSuperseded,
        ManifestState::Missing => VaultError::Verification("the vault manifest is missing".to_owned()),
        _ => VaultError::Conflict,
    };

    if remote.create_only() {
        for _ in 0..REISSUE_SWAP_TRIES {
            // Etag first, bytes second: a swap between the two shows up as
            // non-base bytes; one after the read fails the If-Match.
            let etag = remote
                .list(ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Manifest), device_id: None })
                .await?
                .into_iter()
                .find(|o| o.key == key)
                .and_then(|o| o.etag);
            let copies = remote.get_all(&key).await?;
            match judge_manifest(&copies, record, &ours) {
                ManifestState::Done => return Ok(ReissueStatus { not_revocation: true }),
                ManifestState::Base => {}
                other => return Err(refuse(other)),
            }
            let Some(etag) = etag else { continue };
            match remote.put_replace(&key, &record.new_manifest, Some(&etag)).await {
                Ok(_) => {}
                Err(ProviderError::PreconditionFailed) => continue,
                Err(e) => return Err(e.into()),
            }
            return match judge_manifest(&remote.get_all(&key).await?, record, &ours) {
                ManifestState::Done => Ok(ReissueStatus { not_revocation: true }),
                other => Err(refuse(other)),
            };
        }
        return Err(ProviderError::Transient("manifest etag kept changing".to_owned()).into());
    }

    let copies = remote.get_all(&key).await?;
    match judge_manifest(&copies, record, &ours) {
        ManifestState::Done => {}
        ManifestState::Base => {
            remote.put_create_unchecked(&key, &record.new_manifest).await?;
        }
        other => return Err(refuse(other)),
    }
    // Verify the new copy is in place before any base copy goes.
    let copies = remote.get_all(&key).await?;
    if !copies.iter().any(|c| c.bytes == record.new_manifest) {
        return Err(VaultError::Conflict);
    }
    for old in &copies {
        if record.base.get(&old.provider_ref).is_some_and(|sha| *sha == sha256_hex(&old.bytes)) {
            remote.delete_ref(&old.provider_ref).await?;
        }
    }
    Ok(ReissueStatus { not_revocation: true })
}

pub(crate) fn finish_reissue(store: &mut Store) -> Result<(), StoreError> {
    store.meta_set(META_REISSUE_STATE, STATE_DONE)
}

/// Drops a pending reissue that another device's manifest superseded
/// ([`VaultError::ReissueSuperseded`]): this device's new RK is never
/// confirmed and nothing remote is touched.
pub(crate) fn abandon_reissue(store: &mut Store) -> Result<(), VaultError> {
    if store.meta_get(META_REISSUE_STATE)?.as_deref() != Some(STATE_PENDING) {
        return Err(VaultError::NothingPending);
    }
    store.meta_set(META_REISSUE_STATE, STATE_NONE)?;
    store.meta_set(META_REISSUE_MANIFEST, "")?;
    store.meta_set(META_REISSUE_BASE, "")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Device removal (§4.6.3)

/// Deletes another device's record. That device then stops uploading and
/// must rejoin (engine). Its streams stay until compaction or a wipe.
pub(crate) async fn remove_device<P: ObjectStore>(
    remote: &P,
    vault: &str,
    own_device: &str,
    device: &str,
) -> Result<(), VaultError> {
    if !is_id(device) {
        return Err(VaultError::InvalidId);
    }
    if device == own_device {
        return Err(VaultError::SelfRemoval);
    }
    let key = ObjectAddress::device_record(vault, INITIAL_KEY_EPOCH, device).remote_key().ok_or(VaultError::InvalidId)?;
    remote.delete(&key).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Remote wipe (§4.7.1, AC 26/27)

/// Step 1–2: the typed confirmation, then `wipe_state = started` for `vault`.
pub(crate) fn begin_wipe(store: &mut Store, vault: &str, confirmation: &str) -> Result<(), VaultError> {
    if confirmation != WIPE_CONFIRMATION {
        return Err(VaultError::ConfirmationMismatch);
    }
    if !is_id(vault) {
        return Err(VaultError::InvalidId);
    }
    if let Some(other) = wipe_pending(store)? {
        if other != vault {
            return Err(VaultError::Busy);
        }
    }
    store.meta_set(META_WIPE_VAULT, vault)?;
    store.meta_set(META_WIPE_STATE, STATE_STARTED)?;
    Ok(())
}

/// The vault whose wipe started and is not done (resume after a crash).
pub(crate) fn wipe_pending(store: &Store) -> Result<Option<String>, StoreError> {
    if store.meta_get(META_WIPE_STATE)?.as_deref() != Some(STATE_STARTED) {
        return Ok(None);
    }
    store.meta_get(META_WIPE_VAULT)
}

/// The engine never syncs while a wipe runs, or into a vault it wiped.
pub(crate) fn wipe_in_progress(store: &Store) -> Result<bool, StoreError> {
    match store.meta_get(META_WIPE_STATE)?.as_deref() {
        Some(STATE_STARTED) => Ok(true),
        Some(STATE_DONE) => Ok(store.meta_get(META_WIPE_VAULT)? == store.active_vault()?),
        _ => Ok(false),
    }
}

/// Steps 3–5, idempotent: delete the manifest, read it as missing twice,
/// then delete every object inside `v1/<vault>/` until a relist is empty.
/// An empty relist is confirmed by one final sweep after
/// [`WIPE_FINAL_SWEEP_DELAY`] (D7): a peer whose cycle passed its manifest
/// gate before the delete can still land one put; that object is removed
/// here. Residual risk: a peer offline mid-put for longer than the pause.
/// Returns how many objects were deleted.
pub(crate) async fn run_wipe<P: ObjectStore>(remote: &P, pause: &impl Pause, vault: &str) -> Result<usize, VaultError> {
    let key = manifest_key(vault).ok_or(VaultError::InvalidId)?;
    let prefix = format!("v1/{vault}/");
    for copy in remote.get_all(&key).await? {
        remote.delete_ref(&copy.provider_ref).await?;
    }
    remote.delete(&key).await?;
    for attempt in 0..2 {
        if attempt > 0 {
            pause.pause(RECHECK_DELAYS[0]).await;
        }
        match remote.get(&key).await {
            Err(ProviderError::NotFound) => {}
            Ok(_) => return Err(VaultError::WipeIncomplete("manifest still readable".to_owned())),
            Err(e) => return Err(e.into()),
        }
    }
    let mut deleted = 0;
    let mut confirming = false;
    for _ in 0..WIPE_SWEEPS {
        let refs = wipe_listing(remote, vault, &prefix).await?;
        if refs.is_empty() {
            if confirming {
                return Ok(deleted);
            }
            confirming = true;
            pause.pause(WIPE_FINAL_SWEEP_DELAY).await;
            continue;
        }
        confirming = false;
        for provider_ref in refs {
            remote.delete_ref(&provider_ref).await?;
            deleted += 1;
        }
    }
    Err(VaultError::WipeIncomplete("objects keep appearing".to_owned()))
}

/// Every copy inside `v1/<vault>/`: the Drive Props query plus, where the
/// store supports it, the raw prefix (strays outside the layout).
async fn wipe_listing<P: ObjectStore>(remote: &P, vault: &str, prefix: &str) -> Result<BTreeSet<String>, VaultError> {
    let mut listed = remote.list(ListQuery::Props { vault_id: Some(vault), kind: None, device_id: None }).await?;
    match remote.list(ListQuery::Prefix(prefix)).await {
        Ok(more) => listed.extend(more),
        Err(ProviderError::Unsupported(_)) => {}
        Err(e) => return Err(e.into()),
    }
    let mut refs: BTreeSet<String> = BTreeSet::new();
    for object in listed {
        if !object.key.starts_with(prefix) {
            return Err(VaultError::WipeIncomplete("listing returned a key outside the vault".to_owned()));
        }
        refs.insert(object.provider_ref);
    }
    Ok(refs)
}

/// Steps 6–7: `wipe_state = done`, then the local DEK is deleted. Safe to
/// call again after a crash between the two.
pub(crate) fn finish_wipe(store: &mut Store, secrets: &impl SecretStore, vault: &str) -> Result<(), VaultError> {
    if store.meta_get(META_WIPE_VAULT)?.as_deref() != Some(vault) {
        return Err(VaultError::NothingPending);
    }
    store.meta_set(META_WIPE_STATE, STATE_DONE)?;
    delete_secret(secrets, SyncSecret::VaultKey { vault_id: vault })?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Mutex;

    use super::*;
    use crate::sync::identity::genesis_chain_head;
    use crate::sync::provider::binding_fp;
    use crate::sync::provider::fake::{FakeStore, Op};
    use crate::sync::secrets::MemoryStore;
    use crate::sync::store::{open_segment, ApplyOutcome, CursorState, PendingSegment};

    const NOW: u64 = 1_700_000_000_000;
    const V1: &str = "vault-one";
    const M_SAME: &str = "10000000-0000-4000-8000-000000000001";
    const M_CONFLICT: &str = "10000000-0000-4000-8000-000000000002";
    const M_MINE: &str = "10000000-0000-4000-8000-000000000003";
    const M_DEAD: &str = "10000000-0000-4000-8000-000000000004";
    const M_C: &str = "10000000-0000-4000-8000-000000000005";
    const CONV: &str = "20000000-0000-4000-8000-000000000001";
    const TR: &str = "30000000-0000-4000-8000-000000000001";
    const IMAGE: &[u8] = b"\x89PNG fake image bytes";

    #[derive(Default)]
    struct NoPause(Mutex<Vec<Duration>>);

    impl Pause for NoPause {
        async fn pause(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    impl NoPause {
        fn taken(&self) -> Vec<Duration> {
            self.0.lock().unwrap().clone()
        }
    }

    fn binding() -> String {
        binding_fp("fake|default")
    }

    fn body(kind: Kind, entity: &str, op_type: &str, payload: Value) -> OpBody {
        fresh_op(&mut HlcClock::default(), NOW, kind, entity, op_type, payload)
    }

    fn memo(entity: &str, text: &str) -> OpBody {
        body(Kind::Memo, entity, "create", json!({"text": text, "pinned": false}))
    }

    fn keys(remote: &FakeStore, prefix: &str) -> Vec<String> {
        let all = if remote.create_only() {
            futures_lite_block(remote.list(ListQuery::Prefix("v1/")))
        } else {
            futures_lite_block(remote.list(ListQuery::Props { vault_id: None, kind: None, device_id: None }))
        };
        let mut out: Vec<String> = all.unwrap().into_iter().map(|o| o.key).filter(|k| k.starts_with(prefix)).collect();
        out.sort();
        out
    }

    /// The fake completes every future on its first poll.
    fn futures_lite_block<F: std::future::Future>(future: F) -> F::Output {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(out) => out,
            std::task::Poll::Pending => panic!("fake store future pending"),
        }
    }

    fn seg_key(secrets: &MemoryStore, vault: &str) -> SecretKey {
        let dek = load_vault_key(secrets, vault).unwrap().unwrap();
        derive_subkey(&dek, vault, INITIAL_KEY_EPOCH, Purpose::Segment)
    }

    fn seal(store: &mut Store, secrets: &MemoryStore, vault: &str) -> PendingSegment {
        let key = seg_key(secrets, vault);
        store
            .seal_pending(vault, |segment, plain| {
                let address =
                    ObjectAddress::segment(vault, INITIAL_KEY_EPOCH, &segment.device_id, segment.first_seq, segment.last_seq);
                crate::sync::envelope::seal(&key, &address, plain)
            })
            .unwrap()
            .expect("something to seal")
    }

    fn segment_key(vault: &str, segment: &PendingSegment) -> String {
        ObjectAddress::segment(vault, INITIAL_KEY_EPOCH, &segment.device_id, segment.first_seq, segment.last_seq)
            .remote_key()
            .unwrap()
    }

    async fn upload(remote: &FakeStore, vault: &str, segment: &PendingSegment) {
        remote.put_create(&segment_key(vault, segment), &segment.bytes).await.unwrap();
    }

    /// Discovery that found nothing, recorded at `NOW`.
    async fn discovered_none(remote: &FakeStore, store: &mut Store) {
        let result = discover(remote, &NoPause::default(), || NOW).await;
        assert_eq!(result, Discovery::None);
        record_discovery(store, &result, NOW).unwrap();
    }

    struct Created {
        store: Store,
        secrets: MemoryStore,
        vault: String,
        rk: String,
    }

    /// Device A creates a vault on `remote` through the whole create flow.
    async fn create_on(remote: &FakeStore) -> Created {
        let mut store = Store::open_in_memory().unwrap();
        let secrets = MemoryStore::default();
        discovered_none(remote, &mut store).await;
        let mut pending = begin_create(&store, &binding(), NOW).unwrap();
        let rk = pending.recovery_key().to_display().to_string();
        confirm_create(&mut pending, &secrets, &rk).unwrap();
        publish_created(remote, &secrets, &pending).await.unwrap();
        activate_created(&mut store, &secrets, &pending, None, &mut HlcClock::default(), NOW).unwrap();
        let vault = pending.vault_id().to_owned();
        Created { store, secrets, vault, rk }
    }

    fn v1_name_key() -> SecretKey {
        derive_subkey(&SecretKey::from_bytes([3; 32]), V1, INITIAL_KEY_EPOCH, Purpose::BlobName)
    }

    fn v1_seg_key() -> SecretKey {
        derive_subkey(&SecretKey::from_bytes([3; 32]), V1, INITIAL_KEY_EPOCH, Purpose::Segment)
    }

    fn v1_device(device: &str) -> Store {
        let mut store = Store::open_in_memory().unwrap();
        let identity = DeviceIdentity { v: 1, vault_id: V1.to_owned(), device_id: device.to_owned(), install_fp: None, generation: 0 };
        store.init_identity(&identity).unwrap();
        store.set_active_vault(V1).unwrap();
        store
    }

    /// Device B on V1: memos that match / conflict with / are absent from V2,
    /// a tombstone, a conversation with an image, a pinned translation, and
    /// a memo from remote V1 device C (so B holds a V1 cursor for C).
    fn device_b() -> Store {
        let mut b = v1_device("dev-b1");
        for op in [
            memo(M_SAME, "same everywhere"),
            memo(M_CONFLICT, "B's version"),
            memo(M_MINE, "only on B"),
            memo(M_DEAD, "deleted on B"),
        ] {
            b.insert_local_op(V1, op, UploadState::Never).unwrap();
        }
        b.insert_local_op(V1, body(Kind::Memo, M_DEAD, "delete", json!({})), UploadState::Never).unwrap();
        let blob_id = hex(&hmac_sha256(&v1_name_key(), IMAGE));
        let blob = BlobRow { blob_id: blob_id.clone(), mime: "image/png".to_owned(), bytes: IMAGE.to_vec(), sha256: Sha256::digest(IMAGE).into() };
        let msg = |id: &str, parent: Option<&str>, attachments: Value, ts: u64| {
            let mut p = json!({"conv_id": CONV, "msg_id": id, "role": "user", "content": format!("text {id}"), "attachments": attachments, "ts": ts});
            if let Some(parent) = parent {
                p["parent_id"] = json!(parent);
            }
            body(Kind::Conv, CONV, "msg.add", p)
        };
        let ops = vec![
            body(Kind::Conv, CONV, "create", json!({"title": "chat"})),
            msg("m-1", None, json!([{"blob_id": blob_id, "mime": "image/png", "width": 1, "height": 1}]), 1),
            msg("m-2", Some("m-1"), json!([]), 2),
            body(Kind::Tr, TR, "tr.add", json!({"source": "가", "target": "a", "pinned": true})),
        ];
        b.insert_local_batch(V1, ops, vec![blob], Vec::new()).unwrap();

        let mut c = v1_device("dev-c1");
        c.insert_local_op(V1, memo(M_C, "from C"), UploadState::Never).unwrap();
        let key = v1_seg_key();
        let sealed = c
            .seal_pending(V1, |s, plain| {
                crate::sync::envelope::seal(&key, &ObjectAddress::segment(V1, INITIAL_KEY_EPOCH, &s.device_id, s.first_seq, s.last_seq), plain)
            })
            .unwrap()
            .unwrap();
        let address = ObjectAddress::segment(V1, INITIAL_KEY_EPOCH, "dev-c1", 1, sealed.last_seq);
        assert!(matches!(b.apply_sealed(V1, &key, &address, &sealed.bytes).unwrap(), ApplyOutcome::Applied { .. }));
        b
    }

    /// Every V1-scoped row, plus the global tables, as text.
    fn v1_dump(store: &Store) -> String {
        let conn = store.conn();
        let mut out = String::new();
        let mut dump = |sql: &str| {
            let mut stmt = conn.prepare(sql).unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}"))).collect::<Result<Vec<_>, _>>()
                })
                .unwrap();
            for row in rows {
                out.push_str(&format!("{sql}: {}\n", row.unwrap().join("|")));
            }
        };
        for table in ["meta", "entities", "msg_tree", "blobs"] {
            dump(&format!("SELECT * FROM {table} ORDER BY 1, 2"));
        }
        for table in [
            "vault_identity", "ops", "op_dots", "forks", "memo_lineage", "vault_blob_map", "cursors", "remote_index",
            "retired_devices", "quarantine",
        ] {
            dump(&format!("SELECT * FROM {table} WHERE vault_id = '{V1}' ORDER BY 1, 2, 3"));
        }
        out
    }

    fn v1_op_ids(store: &Store) -> Vec<String> {
        let mut stmt = store.conn().prepare("SELECT op_id FROM ops WHERE vault_id = ?1").unwrap();
        stmt.query_map([V1], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    fn live(store: &Store, vault: &str, kind: Kind, id: &str) -> Option<EntityState> {
        store.projection(vault).unwrap().entities.get(&(kind, id.to_owned())).cloned().filter(|s| *s != EntityState::Deleted)
    }

    struct Joined {
        a: Created,
        b: Store,
        b_secrets: MemoryStore,
        remote: FakeStore,
    }

    /// A creates V2 holding M_SAME (same content as B) and M_CONFLICT (other
    /// content) and uploads it; B (on V1) unlocks with A's RK and runs J2.
    async fn joining(create_only: bool) -> (Joined, UnlockedVault) {
        let remote = FakeStore::new(create_only);
        let mut a = create_on(&remote).await;
        a.store.insert_local_op(&a.vault, memo(M_SAME, "same everywhere"), UploadState::Never).unwrap();
        let first = seal(&mut a.store, &a.secrets, &a.vault);
        upload(&remote, &a.vault, &first).await;
        a.store.insert_local_op(&a.vault, memo(M_CONFLICT, "A's version"), UploadState::Never).unwrap();
        let second = seal(&mut a.store, &a.secrets, &a.vault);
        upload(&remote, &a.vault, &second).await;

        let b = device_b();
        let b_secrets = MemoryStore::default();
        let found = discover(&remote, &NoPause::default(), || NOW).await;
        assert_eq!(found, Discovery::Found(vec![a.vault.clone()]));
        let Discovery::Found(ids) = found else { unreachable!() };
        let candidates = fetch_candidates(&remote, &ids).await.unwrap();
        let unlocked = unlock_join(&b, &b_secrets, &candidates, &a.rk, &binding()).unwrap();
        assert_eq!(unlocked.vault_id, a.vault);
        (Joined { a, b, b_secrets, remote }, unlocked)
    }

    #[tokio::test]
    async fn discovery_twice_empty_is_none_and_allows_create() {
        let remote = FakeStore::new(true);
        let pause = NoPause::default();
        assert_eq!(discover(&remote, &pause, || NOW).await, Discovery::None);
        assert_eq!(pause.taken(), [Duration::from_secs(5)], "second try at 5 s");
        let mut store = Store::open_in_memory().unwrap();
        assert!(matches!(begin_create(&store, &binding(), NOW), Err(VaultError::DiscoveryRequired)));
        record_discovery(&mut store, &Discovery::None, NOW).unwrap();
        assert!(begin_create(&store, &binding(), NOW).is_ok());
        assert!(
            matches!(begin_create(&store, &binding(), NOW + DISCOVERY_RETRY_MS + 1), Err(VaultError::DiscoveryRequired)),
            "a stale `none` no longer allows a create"
        );
        assert_eq!(discovery_retry_at(&store).unwrap(), None);
    }

    #[tokio::test]
    async fn discovery_found_forbids_create_and_returns_candidates() {
        let remote = FakeStore::new(false);
        remote.seed("v1/vault-x/manifest.json", b"{}");
        remote.seed("v1/vault-x/log/dev-1/000000000001-000000000001.seg", b"s");
        let result = discover(&remote, &NoPause::default(), || NOW).await;
        assert_eq!(result, Discovery::Found(vec!["vault-x".to_owned()]));
        let mut store = Store::open_in_memory().unwrap();
        record_discovery(&mut store, &result, NOW).unwrap();
        assert!(matches!(begin_create(&store, &binding(), NOW), Err(VaultError::DiscoveryRequired)));
    }

    #[tokio::test]
    async fn discovery_error_is_retry_with_zero_creates_and_uploads() {
        let remote = FakeStore::new(true);
        remote.fail_next(Op::List, ProviderError::Transient("down".to_owned()));
        let result = discover(&remote, &NoPause::default(), || NOW).await;
        assert_eq!(result, Discovery::Retry { reason: RetryReason::Error, seen: Vec::new() });
        let mut store = Store::open_in_memory().unwrap();
        record_discovery(&mut store, &result, NOW).unwrap();
        assert_eq!(store.meta_get("discovery_state").unwrap().as_deref(), Some("retry"));
        assert_eq!(discovery_retry_at(&store).unwrap(), Some(NOW + 5 * 60 * 1000));
        assert!(matches!(begin_create(&store, &binding(), NOW), Err(VaultError::DiscoveryRequired)));
        assert_eq!(remote.requests(), 1, "only the failed listing reached the store");
        assert!(keys(&remote, "v1/").is_empty());
    }

    /// ADV-5: a `retry` result of any kind persists `retry` with the 5 min
    /// retry time, refuses creation, and the pass wrote nothing remote.
    fn assert_retry_forbids_everything(result: &Discovery, remote: &FakeStore, writes_before: Vec<String>) {
        let mut store = Store::open_in_memory().unwrap();
        record_discovery(&mut store, result, NOW).unwrap();
        assert_eq!(store.meta_get("discovery_state").unwrap().as_deref(), Some("retry"));
        assert_eq!(discovery_retry_at(&store).unwrap(), Some(NOW + DISCOVERY_RETRY_MS));
        assert!(matches!(begin_create(&store, &binding(), NOW), Err(VaultError::DiscoveryRequired)));
        assert!(store.active_vault().unwrap().is_none());
        assert_eq!(keys(remote, "v1/"), writes_before, "discovery writes nothing remote");
    }

    #[tokio::test]
    async fn discovery_over_30_seconds_is_retry() {
        let remote = FakeStore::new(true);
        let t = Cell::new(NOW);
        let clock = || {
            t.set(t.get() + 16_000);
            t.get()
        };
        let result = discover(&remote, &NoPause::default(), clock).await;
        assert_eq!(result, Discovery::Retry { reason: RetryReason::Timeout, seen: Vec::new() });
        assert_retry_forbids_everything(&result, &remote, Vec::new());
    }

    /// D8: the second listing never answers. The injected clock has used
    /// all but 300 ms of the cap by then, so the per-call timeout cuts it at
    /// the remaining cap (not a fixed 30 s), keeping what the first saw.
    #[tokio::test]
    async fn discovery_hung_listing_is_cut_at_the_remaining_cap() {
        struct HangsSecond(std::sync::atomic::AtomicUsize);
        impl ObjectStore for HangsSecond {
            fn create_only(&self) -> bool {
                true
            }
            fn identity(&self) -> String {
                "fake|default".to_owned()
            }
            async fn list(&self, _: ListQuery<'_>) -> Result<Vec<crate::sync::provider::ListedObject>, ProviderError> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let key = manifest_key("vault-x").unwrap();
                    return Ok(vec![crate::sync::provider::ListedObject {
                        key: key.clone(),
                        provider_ref: key,
                        size: None,
                        etag: None,
                    }]);
                }
                std::future::pending().await
            }
            async fn get(&self, _: &str) -> Result<Vec<u8>, ProviderError> {
                Err(ProviderError::NotFound)
            }
            async fn put_create(&self, _: &str, _: &[u8]) -> Result<crate::sync::provider::ListedObject, ProviderError> {
                panic!("discovery never writes")
            }
            async fn put_replace(
                &self,
                _: &str,
                _: &[u8],
                _: Option<&str>,
            ) -> Result<crate::sync::provider::ListedObject, ProviderError> {
                panic!("discovery never writes")
            }
            async fn delete(&self, _: &str) -> Result<(), ProviderError> {
                panic!("discovery never deletes")
            }
            async fn quota(&self) -> Result<crate::sync::provider::Quota, ProviderError> {
                Err(ProviderError::Unsupported("quota".to_owned()))
            }
            async fn ensure_container(&self) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        let cap = DISCOVERY_CAP.as_millis() as u64;
        let t = Cell::new(NOW);
        let pause = NoPause::default();
        let store = HangsSecond(Default::default());
        let started = std::time::Instant::now();
        let clock = || {
            // The 5 s pause "took" all but 300 ms of the cap.
            if !pause.taken().is_empty() {
                t.set(NOW + cap - 300);
            }
            t.get()
        };
        let result = discover(&store, &pause, clock).await;
        let took = started.elapsed();
        assert_eq!(result, Discovery::Retry { reason: RetryReason::Timeout, seen: vec!["vault-x".to_owned()] });
        assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(5), "cut at the remaining cap: {took:?}");
        let mut state = Store::open_in_memory().unwrap();
        record_discovery(&mut state, &result, NOW).unwrap();
        assert!(matches!(begin_create(&state, &binding(), NOW), Err(VaultError::DiscoveryRequired)));
    }

    /// The manifest shows up for one listing only (list lag both ways).
    struct FlipPause<'a>(&'a FakeStore, Mutex<Vec<Duration>>);

    impl Pause for FlipPause<'_> {
        async fn pause(&self, duration: Duration) {
            let first = {
                let mut taken = self.1.lock().unwrap();
                taken.push(duration);
                taken.len() == 1
            };
            if first {
                self.0.seed("v1/vault-x/manifest.json", b"{}");
            } else {
                self.0.delete("v1/vault-x/manifest.json").await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn discovery_inconsistent_three_tries_is_retry_at_0_5_15() {
        let remote = FakeStore::new(true);
        let pause = FlipPause(&remote, Mutex::default());
        let result = discover(&remote, &pause, || NOW).await;
        assert_eq!(result, Discovery::Retry { reason: RetryReason::Inconsistent, seen: vec!["vault-x".to_owned()] });
        assert_eq!(*pause.1.lock().unwrap(), [Duration::from_secs(5), Duration::from_secs(10)]);
        assert_eq!(remote.requests(), 4, "three listings and the pause's own delete; no put");
        assert_retry_forbids_everything(&result, &remote, Vec::new());
    }

    #[tokio::test]
    async fn create_requires_rk_reentry_before_any_keychain_or_remote_change() {
        let remote = FakeStore::new(true);
        let mut store = Store::open_in_memory().unwrap();
        let secrets = MemoryStore::default();
        discovered_none(&remote, &mut store).await;
        let before = remote.requests();
        let mut pending = begin_create(&store, &binding(), NOW).unwrap();
        let other = RecoveryKey::generate().unwrap().to_display().to_string();
        assert!(matches!(confirm_create(&mut pending, &secrets, &other), Err(VaultError::Recovery(RecoveryError::WrongKey))));
        assert!(confirm_create(&mut pending, &secrets, "not a key").is_err());
        assert!(secrets.accounts().is_empty(), "wrong RK: keychain unchanged");
        assert!(matches!(publish_created(&remote, &secrets, &pending).await, Err(VaultError::NotConfirmed)));
        assert!(matches!(
            activate_created(&mut store, &secrets, &pending, None, &mut HlcClock::default(), NOW),
            Err(VaultError::NotConfirmed)
        ));
        assert_eq!(remote.requests(), before, "nothing uploaded");
        assert_eq!(store.active_vault().unwrap(), None);
    }

    #[tokio::test]
    async fn create_publishes_verified_manifest_then_activates() {
        let remote = FakeStore::new(true);
        let a = create_on(&remote).await;
        assert_eq!(keys(&remote, "v1/"), [format!("v1/{}/manifest.json", a.vault)]);
        assert_eq!(a.store.active_vault().unwrap().as_deref(), Some(a.vault.as_str()));
        let local = a.store.local_identity(&a.vault).unwrap().unwrap();
        assert_eq!(local.sealed_seq, 0);
        assert_eq!(local.chain_head, genesis_chain_head(&a.vault, &local.device_id));
        let check = check_vaults(&remote, &a.vault).await.unwrap();
        assert_eq!(check, VaultCheck { manifest_present: true, other_vaults: Vec::new(), manifest_copies: 1 });
        assert!(a.secrets.accounts().contains(&format!("sync:vault:{}", a.vault)));
        assert!(a.secrets.accounts().contains(&format!("sync:device:{}", a.vault)));
    }

    #[tokio::test]
    async fn create_turns_existing_local_data_into_the_first_stream() {
        let remote = FakeStore::new(true);
        let mut b = device_b();
        let secrets = MemoryStore::default();
        discovered_none(&remote, &mut b).await;
        let mut pending = begin_create(&b, &binding(), NOW).unwrap();
        let rk = pending.recovery_key().to_display().to_string();
        confirm_create(&mut pending, &secrets, &rk).unwrap();
        publish_created(&remote, &secrets, &pending).await.unwrap();
        let commit = activate_created(&mut b, &secrets, &pending, None, &mut HlcClock::default(), NOW).unwrap();
        let vault = pending.vault_id().to_owned();
        assert_eq!(commit.ops, 9, "5 memos/tr + conversation (create, 2 messages, set_active)");
        for (kind, id) in [(Kind::Memo, M_SAME), (Kind::Memo, M_CONFLICT), (Kind::Memo, M_MINE), (Kind::Memo, M_C), (Kind::Conv, CONV), (Kind::Tr, TR)] {
            assert!(live(&b, &vault, kind, id).is_some(), "{id} carried into the new vault");
        }
        assert!(live(&b, &vault, Kind::Memo, M_DEAD).is_none());
        let segment = seal(&mut b, &secrets, &vault);
        assert_eq!((segment.first_seq, segment.last_seq), (1, 9));
    }

    #[tokio::test]
    async fn post_hoc_detection_reports_other_vault_and_missing_manifest_without_writing() {
        let remote = FakeStore::new(true);
        let a = create_on(&remote).await;
        remote.seed("v1/vault-zz/manifest.json", b"{}");
        let check = check_vaults(&remote, &a.vault).await.unwrap();
        assert_eq!(
            check,
            VaultCheck { manifest_present: true, other_vaults: vec!["vault-zz".to_owned()], manifest_copies: 1 }
        );
        assert_eq!(keys(&remote, "v1/vault-zz/").len(), 1, "the other vault is untouched");

        remote.fail_next(Op::Get, ProviderError::Transient("down".to_owned()));
        assert!(check_vaults(&remote, &a.vault).await.is_err(), "an error is never 'missing'");

        remote.delete(&manifest_key(&a.vault).unwrap()).await.unwrap();
        let before = remote.requests();
        let check = check_vaults(&remote, &a.vault).await.unwrap();
        assert!(!check.manifest_present);
        assert_eq!(remote.requests(), before + 3, "one list and two reads, no write");
        assert!(keys(&remote, &format!("v1/{}/", a.vault)).is_empty());
    }

    #[tokio::test]
    async fn join_n3_new_identity_genesis_no_v1_leak_all_live_kept() {
        let (mut j, unlocked) = joining(true).await;
        let v2 = j.a.vault.clone();
        let tx = fetch_target(&j.remote, &v2).await.unwrap();
        assert_eq!(apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap(), 2);
        assert_eq!(j.b.active_vault().unwrap().as_deref(), Some(V1), "J2 leaves V1 active");

        let preview = preview_join(&j.b, &j.b_secrets, &unlocked).unwrap();
        let summary = preview.summary();
        assert_eq!(summary, JoinSummary { skip: 1, republish: 4, copy: 1, evicted: 0, tombstones: 1 });
        let exposed = serde_json::to_string(&summary).unwrap();
        for id in [M_SAME, M_CONFLICT, M_MINE, V1, v2.as_str()] {
            assert!(!exposed.contains(id), "the UI summary carries no ids");
        }
        let copy = preview.items.iter().find(|i| i.source_id == M_CONFLICT).unwrap();
        assert_eq!(copy.action, JoinAction::Copy);
        assert_ne!(copy.target_id, M_CONFLICT);
        assert_eq!(preview, preview_join(&j.b, &j.b_secrets, &unlocked).unwrap(), "deterministic preview");

        let v1_device_ids = ["dev-b1", "dev-c1"];
        let v1_ops = v1_op_ids(&j.b);
        assert_eq!(v1_ops.len(), 10, "B: 4 creates, 1 delete, 4 conversation/tr ops; C: 1");
        let mut clock = HlcClock::default();
        let commit = commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut clock, NOW).unwrap();
        assert_eq!(commit.ops, 8, "M_MINE, M_C, the copy, conversation x4, tr.add");
        assert_eq!(commit.blobs, 1);
        assert_eq!(j.b.active_vault().unwrap().as_deref(), Some(v2.as_str()));
        assert!(!v1_device_ids.contains(&commit.device_id.as_str()), "V2 device id differs from V1");
        assert_eq!(j.b.meta_get("joined_from_vault").unwrap().as_deref(), Some(V1));

        // V2 stream: seq 1, genesis chain, ctx = V2 frontier.
        let segment = seal(&mut j.b, &j.b_secrets, &v2);
        assert_eq!((segment.first_seq, segment.last_seq), (1, 8));
        let opened = open_segment(&seg_key(&j.b_secrets, &v2), &ObjectAddress::segment(&v2, 1, &commit.device_id, 1, 8), &segment.bytes).unwrap();
        assert_eq!(opened.prev_seg_hash, hex(&genesis_chain_head(&v2, &commit.device_id)));
        let a_device = j.a.store.local_identity(&v2).unwrap().unwrap().device_id;
        for op in &opened.ops {
            assert_eq!(op.body.ctx.get(&a_device), Some(&2), "ctx carries the V2 frontier");
            assert!(op.body.ctx.keys().all(|d| !v1_device_ids.contains(&d.as_str())));
        }
        upload(&j.remote, &v2, &segment).await;

        // A V2 reader applies it without hold.
        let outcome = j.a.store.apply_sealed(&v2, &seg_key(&j.a.secrets, &v2), &ObjectAddress::segment(&v2, 1, &commit.device_id, 1, 8), &segment.bytes).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Applied { .. }), "{outcome:?}");
        let cursor = j.a.store.cursor(&v2, &commit.device_id).unwrap().unwrap();
        assert_eq!(cursor.state, CursorState::Ok);
        assert_eq!(j.a.store.projection(&v2).unwrap().hash, j.b.projection(&v2).unwrap().hash, "converged");

        // Byte scan of every V2 object (raw and decrypted).
        let old_blob = hex(&hmac_sha256(&v1_name_key(), IMAGE));
        let mut forbidden: Vec<String> = v1_ops.clone();
        forbidden.extend(v1_device_ids.iter().map(|d| d.to_string()));
        forbidden.extend([M_DEAD.to_owned(), V1.to_owned(), old_blob]);
        let objects = keys(&j.remote, &format!("v1/{v2}/"));
        assert_eq!(objects.len(), 4, "manifest + 2 A segments + 1 B segment");
        for key in &objects {
            for bytes in j.remote.copies(key) {
                let mut texts = vec![bytes.clone()];
                if let Some(address) = ObjectAddress::from_remote_key(key, 1) {
                    texts.push(open_segment(&seg_key(&j.b_secrets, &v2), &address, &bytes).unwrap().encode().unwrap());
                }
                for text in &texts {
                    for needle in &forbidden {
                        assert!(!contains(text, needle), "{key} leaks V1 value {needle}");
                    }
                }
            }
        }
        // ...and of B's own V2 rows.
        for table in ["ops", "op_dots", "cursors", "forks", "retired_devices", "remote_index"] {
            let rows: Vec<String> = {
                let mut stmt = j.b.conn().prepare(&format!("SELECT * FROM {table} WHERE vault_id = ?1")).unwrap();
                let n = stmt.column_count();
                stmt.query_map([&v2], |r| (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}"))).collect::<Result<Vec<_>, _>>().map(|v| v.join("|")))
                    .unwrap().collect::<Result<_, _>>().unwrap()
            };
            for row in rows {
                for needle in &forbidden {
                    assert!(!row.contains(needle.as_str()), "{table} V2 row leaks {needle}");
                }
            }
        }

        // Every V1 live entity is in V2 (the conflict as a copy); the V1
        // tombstone target does not reappear.
        let text = |state: Option<EntityState>| match state {
            Some(EntityState::Memo(m)) => m.text,
            other => panic!("not a live memo: {other:?}"),
        };
        assert_eq!(text(live(&j.b, &v2, Kind::Memo, M_SAME)), "same everywhere");
        assert_eq!(text(live(&j.b, &v2, Kind::Memo, M_CONFLICT)), "A's version", "V2 content never overwritten");
        assert_eq!(text(live(&j.b, &v2, Kind::Memo, &copy.target_id)), "B's version");
        assert_eq!(text(live(&j.b, &v2, Kind::Memo, M_MINE)), "only on B");
        assert_eq!(text(live(&j.b, &v2, Kind::Memo, M_C)), "from C");
        assert!(!j.b.projection(&v2).unwrap().entities.contains_key(&(Kind::Memo, M_DEAD.to_owned())));
        let Some(EntityState::Conv(conv)) = live(&j.b, &v2, Kind::Conv, CONV) else { panic!("conversation missing") };
        assert_eq!(conv.messages.len(), 2);
        assert_eq!(conv.messages["m-2"].parent_id.as_deref(), Some("m-1"));
        let v2_name = derive_subkey(&load_vault_key(&j.b_secrets, &v2).unwrap().unwrap(), &v2, 1, Purpose::BlobName);
        let new_blob = hex(&hmac_sha256(&v2_name, IMAGE));
        assert_eq!(conv.messages["m-1"].attachments[0]["blob_id"], json!(new_blob), "blob id recomputed with V2 K_name");
        assert_eq!(j.b.blob_bytes(&new_blob).unwrap().map(|(_, b)| b).as_deref(), Some(IMAGE));
        let state: String = j.b.conn().query_row(
            "SELECT remote_state FROM vault_blob_map WHERE vault_id = ?1 AND blob_id = ?2", [&v2, &new_blob], |r| r.get(0)).unwrap();
        assert_eq!(state, "pending", "re-uploaded by the engine");
        let Some(EntityState::Tr(tr)) = live(&j.b, &v2, Kind::Tr, TR) else { panic!("translation missing") };
        assert!(tr.pinned);
        // V1 stays readable and unchanged locally.
        assert!(live(&j.b, V1, Kind::Memo, M_MINE).is_some());
    }

    #[tokio::test]
    async fn join_on_drive_uses_props_listing() {
        let (mut j, unlocked) = joining(false).await;
        let tx = fetch_target(&j.remote, &unlocked.vault_id).await.unwrap();
        apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap();
        let preview = preview_join(&j.b, &j.b_secrets, &unlocked).unwrap();
        commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).unwrap();
        assert_eq!(j.b.active_vault().unwrap(), Some(unlocked.vault_id.clone()));
    }

    /// A compacts V2 (§4.5): a snapshot of segments 1–2 replaces them.
    async fn compact_a(j: &mut Joined) -> String {
        let v2 = j.a.vault.clone();
        let snapshot = crate::sync::compaction::build_snapshot(&j.a.store, &v2).unwrap();
        let plain = snapshot.encode().unwrap();
        let device = j.a.store.local_identity(&v2).unwrap().unwrap().device_id;
        let address =
            ObjectAddress::snapshot(&v2, INITIAL_KEY_EPOCH, &device, &crate::sync::engine::snapshot_id(&plain));
        let dek = load_vault_key(&j.a.secrets, &v2).unwrap().unwrap();
        let sealed = crate::sync::envelope::seal(&derive_subkey(&dek, &v2, INITIAL_KEY_EPOCH, Purpose::Snapshot), &address, &plain)
            .unwrap();
        let key = address.remote_key().unwrap();
        j.remote.put_create(&key, &sealed).await.unwrap();
        for seq in [1, 2] {
            j.remote.delete(&ObjectAddress::segment(&v2, 1, &device, seq, seq).remote_key().unwrap()).await.unwrap();
        }
        assert!(keys(&j.remote, &format!("v1/{v2}/log/")).is_empty(), "precondition: every segment compacted");
        key
    }

    /// D4: a compacted V2 that kept writing (stream continues at seq 3) is
    /// joined through its snapshot plus the tail, with exactly the preview of
    /// the uncompacted vault; nothing is applied twice on a retry.
    #[tokio::test]
    async fn join_a_compacted_vault_through_its_snapshot_and_tail() {
        for create_only in [true, false] {
            let (mut j, unlocked) = joining(create_only).await;
            let v2 = j.a.vault.clone();
            compact_a(&mut j).await;
            const M_LATE: &str = "10000000-0000-4000-8000-0000000000b1";
            j.a.store.insert_local_op(&v2, memo(M_LATE, "after compaction"), UploadState::Never).unwrap();
            let third = seal(&mut j.a.store, &j.a.secrets, &v2);
            assert_eq!((third.first_seq, third.last_seq), (3, 3));
            upload(&j.remote, &v2, &third).await;

            let before = v1_dump(&j.b);
            let tx = fetch_target(&j.remote, &v2).await.unwrap();
            assert_eq!(apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap(), 2, "snapshot + segment 3");
            let v2_state = j.b.projection(&v2).unwrap().hash;
            // A retry of the same download (J2 is idempotent) changes nothing:
            // the segment is a duplicate, the snapshot re-applies the same set.
            assert!(apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap() <= 1, "segment 3 not re-applied");
            assert_eq!(j.b.projection(&v2).unwrap().hash, v2_state);
            assert_eq!(v1_dump(&j.b), before, "J2 leaves V1 untouched");
            let preview = preview_join(&j.b, &j.b_secrets, &unlocked).unwrap();
            assert_eq!(preview.summary(), JoinSummary { skip: 1, republish: 4, copy: 1, evicted: 0, tombstones: 1 });
            let copy = preview.items.iter().find(|i| i.source_id == M_CONFLICT).unwrap();
            assert_eq!(copy.action, JoinAction::Copy, "V2 holds other content under the id");
            commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).unwrap();
            let text = |id: &str| match live(&j.b, &v2, Kind::Memo, id) {
                Some(EntityState::Memo(m)) => m.text,
                other => panic!("{id}: {other:?}"),
            };
            assert_eq!(text(M_CONFLICT), "A's version", "create_only={create_only}");
            assert_eq!(text(M_LATE), "after compaction");
            assert_eq!(text(&copy.target_id), "B's version");
        }
    }

    /// D4 fail-closed: a stream that does not start at 1 with no snapshot,
    /// or a snapshot that does not verify, fails J2 with V1 unchanged.
    #[tokio::test]
    async fn join_a_compacted_vault_fails_closed_without_a_valid_snapshot() {
        let (mut j, unlocked) = joining(true).await;
        let v2 = j.a.vault.clone();
        let snap = compact_a(&mut j).await;
        j.a.store.insert_local_op(&v2, memo("10000000-0000-4000-8000-0000000000b2", "late"), UploadState::Never).unwrap();
        let third = seal(&mut j.a.store, &j.a.secrets, &v2);
        upload(&j.remote, &v2, &third).await;
        let before = v1_dump(&j.b);

        // A tampered snapshot: quarantined, the join refused, and a retry
        // of the same download refused too.
        j.remote.delete(&snap).await.unwrap();
        j.remote.seed(&snap, b"not a snapshot");
        let tx = fetch_target(&j.remote, &v2).await.unwrap();
        for _ in 0..2 {
            assert!(matches!(apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx), Err(VaultError::JoinIncomplete(_))));
        }
        assert!(
            j.b.quarantine_rows(&v2).unwrap().iter().any(|(k, _)| *k == snap),
            "the tampered snapshot is quarantined"
        );
        assert_eq!(v1_dump(&j.b), before);
        assert_eq!(j.b.active_vault().unwrap().as_deref(), Some(V1));
        assert!(j.b.local_identity(&v2).unwrap().is_none());

        // No snapshot at all: the stream starts at 3 and nothing covers 1–2.
        j.remote.delete(&snap).await.unwrap();
        assert!(matches!(fetch_target(&j.remote, &v2).await, Err(VaultError::JoinIncomplete(_))));
        assert_eq!(v1_dump(&j.b), before);
    }

    /// D5 / J5: the joined V1 image is re-uploaded under its V2 BlobName id
    /// (sealed with V2 K_blob); no V1 blob id reaches V2; a second upload
    /// dedupes; another V2 device downloads the same bytes.
    #[tokio::test]
    async fn join_reuploads_blobs_under_v2_ids() {
        for create_only in [true, false] {
            let (mut j, unlocked) = joining(create_only).await;
            let v2 = unlocked.vault_id.clone();
            let tx = fetch_target(&j.remote, &v2).await.unwrap();
            apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap();
            let preview = preview_join(&j.b, &j.b_secrets, &unlocked).unwrap();
            commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).unwrap();

            let dek = load_vault_key(&j.b_secrets, &v2).unwrap().unwrap();
            let v2_id = hex(&hmac_sha256(&derive_subkey(&dek, &v2, 1, Purpose::BlobName), IMAGE));
            let old_id = hex(&hmac_sha256(&v1_name_key(), IMAGE));
            assert_ne!(v2_id, old_id);
            let pending = j.b.pending_blobs(&v2).unwrap();
            assert_eq!(pending, vec![(v2_id.clone(), IMAGE.to_vec())], "J5 queued the V2 id");
            for (id, bytes) in &pending {
                let up = crate::sync::engine::upload_blob(&j.remote, &dek, &v2, 1, id, bytes).await.unwrap();
                assert_eq!(up, crate::sync::engine::BlobUpload::Uploaded);
                j.b.set_blob_state(&v2, id, crate::sync::store::BLOB_PUBLISHED).unwrap();
            }
            assert!(j.b.pending_blobs(&v2).unwrap().is_empty());
            let blob_keys = keys(&j.remote, &format!("v1/{v2}/blob/"));
            assert_eq!(blob_keys, vec![ObjectAddress::blob(&v2, 1, &v2_id).remote_key().unwrap()]);
            for key in keys(&j.remote, "v1/") {
                assert!(!key.contains(&old_id), "{key} names the V1 blob id");
                for bytes in j.remote.copies(&key) {
                    assert!(!contains(&bytes, &old_id) && !contains(&bytes, "PNG fake"), "{key} leaks");
                }
            }
            // Idempotent: a create-only store dedupes the same content; Drive
            // may hold a second copy, and every copy opens to the same bytes.
            let again = crate::sync::engine::upload_blob(&j.remote, &dek, &v2, 1, &v2_id, IMAGE).await.unwrap();
            if create_only {
                assert_eq!(again, crate::sync::engine::BlobUpload::Deduped);
            }
            let blob_key = ObjectAddress::blob(&v2, 1, &v2_id);
            for copy in j.remote.copies(&blob_key.remote_key().unwrap()) {
                let plain =
                    crate::sync::envelope::open(&derive_subkey(&dek, &v2, 1, Purpose::Blob), &blob_key, &copy).unwrap();
                assert_eq!(plain.as_slice(), IMAGE);
            }
            // A, already in V2, reads it with its own keychain DEK.
            let a_dek = load_vault_key(&j.a.secrets, &v2).unwrap().unwrap();
            let got = crate::sync::engine::download_blob(&j.remote, &a_dek, &v2, 1, &v2_id, 1 << 20).await.unwrap();
            assert_eq!(got.as_deref(), Some(IMAGE), "create_only={create_only}");
        }
    }

    /// D1 rejoin: after a removal, RK re-entry rotates to a fresh id in the
    /// same vault. Published ops stay under the retired id; a never-sent op
    /// moves to the new stream with its id; an attempted one is reissued
    /// under a fresh id (the old id quarantined). The new stream starts at
    /// seq 1 from the genesis head. A wrong RK changes nothing.
    #[tokio::test]
    async fn rejoin_rotates_to_a_fresh_identity_and_keeps_published_ops() {
        let remote = FakeStore::new(true);
        let mut a = create_on(&remote).await;
        let v = a.vault.clone();
        let old = a.store.local_identity(&v).unwrap().unwrap().device_id;
        let published = memo(M_SAME, "published");
        let attempted = memo(M_CONFLICT, "attempted");
        let never = memo(M_MINE, "never sent");
        a.store.insert_local_op(&v, published.clone(), UploadState::Never).unwrap();
        let first = seal(&mut a.store, &a.secrets, &v);
        upload(&remote, &v, &first).await;
        a.store.mark_attempted(&v, &old, 1).unwrap();
        a.store.mark_published(&v, &old, 1).unwrap();
        a.store.insert_local_op(&v, attempted.clone(), UploadState::Never).unwrap();
        seal(&mut a.store, &a.secrets, &v);
        a.store.mark_attempted(&v, &old, 2).unwrap();
        a.store.insert_local_op(&v, never.clone(), UploadState::Never).unwrap();
        let candidates = fetch_candidates(&remote, std::slice::from_ref(&v)).await.unwrap();

        // Wrong RK: nothing changes (store, keychain).
        let dump = a.store.dump_state().unwrap();
        let accounts: Vec<(String, Option<String>)> =
            a.secrets.accounts().into_iter().map(|k| (k.clone(), a.secrets.raw(&k))).collect();
        let wrong = RecoveryKey::generate().unwrap().to_display().to_string();
        assert!(matches!(rejoin(&mut a.store, &a.secrets, &candidates, &wrong, &binding(), None), Err(VaultError::NoVaultUnlocked)));
        assert!(matches!(rejoin(&mut a.store, &a.secrets, &[], &a.rk, &binding(), None), Err(VaultError::NoVaultUnlocked)));
        assert_eq!(a.store.dump_state().unwrap(), dump);
        let after: Vec<(String, Option<String>)> =
            a.secrets.accounts().into_iter().map(|k| (k.clone(), a.secrets.raw(&k))).collect();
        assert_eq!(after, accounts);

        let commit = rejoin(&mut a.store, &a.secrets, &candidates, &a.rk, &binding(), None).unwrap();
        assert_eq!(commit.vault_id, v);
        assert_ne!(commit.device_id, old);
        assert_eq!(a.store.active_vault().unwrap().as_deref(), Some(v.as_str()), "same vault");
        assert_eq!(a.store.local_identity(&v).unwrap().unwrap().device_id, commit.device_id);
        let keychain = crate::sync::identity::load_identity(&a.secrets, &v).unwrap().unwrap();
        assert_eq!(keychain.device_id, commit.device_id, "keychain names the new id");

        let state = |op: &OpBody| a.store.op_state(&v, &op.op_id).unwrap().map(|s| s.2);
        assert_eq!(state(&published), Some(UploadState::Published), "published stays");
        assert_eq!(a.store.dots_of(&v, &published.op_id).unwrap(), vec![crate::sync::ops::Dot { device: old.clone(), seq: 1 }]);
        assert_eq!(state(&never), Some(UploadState::Never));
        assert_eq!(a.store.dots_of(&v, &never.op_id).unwrap()[0].device, commit.device_id, "moved to the new stream");
        assert!(a.store.dots_of(&v, &attempted.op_id).unwrap().is_empty(), "the attempted id never goes out again");
        assert!(a.store.quarantine_rows(&v).unwrap().iter().any(|(k, _)| *k == format!("op/{}", attempted.op_id)));
        let text = |id: &str| match live(&a.store, &v, Kind::Memo, id) {
            Some(EntityState::Memo(m)) => m.text,
            other => panic!("{id}: {other:?}"),
        };
        assert_eq!(text(M_SAME), "published");
        assert_eq!(text(M_CONFLICT), "attempted", "reissued, content kept");
        assert_eq!(text(M_MINE), "never sent");

        // The next seal starts the new stream at seq 1 from genesis.
        let next = seal(&mut a.store, &a.secrets, &v);
        assert_eq!((next.device_id.as_str(), next.first_seq, next.last_seq), (commit.device_id.as_str(), 1, 2));
        let opened = open_segment(&seg_key(&a.secrets, &v), &ObjectAddress::segment(&v, 1, &commit.device_id, 1, 2), &next.bytes).unwrap();
        assert_eq!(opened.prev_seg_hash, hex(&genesis_chain_head(&v, &commit.device_id)));
        assert!(opened.ops.iter().all(|op| op.body.op_id != attempted.op_id && op.body.op_id != published.op_id));
        let retired: i64 = a
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM retired_devices WHERE vault_id = ?1 AND device_id = ?2", [&v, &old], |r| r.get(0))
            .unwrap();
        assert_eq!(retired, 1, "the removed id is retired, read like another device");
    }

    #[tokio::test]
    async fn join_crash_at_j2_download_leaves_v1_unchanged() {
        let (j, unlocked) = joining(true).await;
        let before = v1_dump(&j.b);
        j.remote.fail_next(Op::Get, ProviderError::Transient("cut".to_owned()));
        assert!(fetch_target(&j.remote, &unlocked.vault_id).await.is_err());
        assert_eq!(v1_dump(&j.b), before);
        assert_eq!(j.b.active_vault().unwrap().as_deref(), Some(V1));
    }

    #[tokio::test]
    async fn join_crash_during_j2_apply_leaves_v1_unchanged() {
        let (mut j, unlocked) = joining(true).await;
        let v2 = unlocked.vault_id.clone();
        let device = j.a.store.local_identity(&v2).unwrap().unwrap().device_id;
        let second = ObjectAddress::segment(&v2, 1, &device, 2, 2).remote_key().unwrap();
        j.remote.delete(&second).await.unwrap();
        j.remote.seed(&second, b"torn write");
        let before = v1_dump(&j.b);
        let tx = fetch_target(&j.remote, &v2).await.unwrap();
        assert!(matches!(apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx), Err(VaultError::JoinIncomplete(_))));
        assert_eq!(v1_dump(&j.b), before, "segment 1 landed in V2 rows only");
        assert_eq!(j.b.active_vault().unwrap().as_deref(), Some(V1));
        assert!(j.b.local_identity(&v2).unwrap().is_none());
    }

    #[tokio::test]
    async fn join_gap_in_a_stream_fails_before_any_apply() {
        let (j, unlocked) = joining(true).await;
        let device = j.a.store.local_identity(&unlocked.vault_id).unwrap().unwrap().device_id;
        j.remote.delete(&ObjectAddress::segment(&unlocked.vault_id, 1, &device, 1, 1).remote_key().unwrap()).await.unwrap();
        assert!(matches!(fetch_target(&j.remote, &unlocked.vault_id).await, Err(VaultError::JoinIncomplete(_))));
    }

    #[tokio::test]
    async fn join_crash_before_j7_leaves_v1_unchanged_and_retry_succeeds() {
        let (mut j, unlocked) = joining(true).await;
        let v2 = unlocked.vault_id.clone();
        let tx = fetch_target(&j.remote, &v2).await.unwrap();
        apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap();
        let preview = preview_join(&j.b, &j.b_secrets, &unlocked).unwrap();
        let before = v1_dump(&j.b);
        CRASH_BEFORE_COMMIT.with(|c| c.set(true));
        let crashed = commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW);
        assert!(matches!(crashed, Err(VaultError::JoinRejected)));
        assert_eq!(v1_dump(&j.b), before);
        assert_eq!(j.b.active_vault().unwrap().as_deref(), Some(V1));
        assert!(j.b.local_identity(&v2).unwrap().is_none());
        let commit = commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).unwrap();
        assert_eq!(j.b.local_identity(&v2).unwrap().unwrap().device_id, commit.device_id);
        let keychain = crate::sync::identity::load_identity(&j.b_secrets, &v2).unwrap().unwrap();
        assert_eq!(keychain.device_id, commit.device_id, "the retry replaced the unpublished keychain id");
    }

    #[tokio::test]
    async fn join_refuses_a_stale_preview() {
        let (mut j, unlocked) = joining(true).await;
        let tx = fetch_target(&j.remote, &unlocked.vault_id).await.unwrap();
        apply_target(&mut j.b, &j.b_secrets, &unlocked, &tx).unwrap();
        let preview = preview_join(&j.b, &j.b_secrets, &unlocked).unwrap();
        j.b.insert_local_op(V1, memo("10000000-0000-4000-8000-0000000000ff", "late"), UploadState::Never).unwrap();
        let before = v1_dump(&j.b);
        let stale = commit_join(&mut j.b, &j.b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW);
        let Err(VaultError::PreviewStale(fresh)) = stale else { panic!("expected a stale preview") };
        assert_eq!(fresh.summary().republish, preview.summary().republish + 1);
        assert_eq!(v1_dump(&j.b), before);
    }

    #[tokio::test]
    async fn join_with_wrong_rk_or_as_member_changes_nothing() {
        let (j, _) = joining(true).await;
        let fresh = Store::open_in_memory().unwrap();
        let secrets = MemoryStore::default();
        let candidates = fetch_candidates(&j.remote, std::slice::from_ref(&j.a.vault)).await.unwrap();
        let wrong = RecoveryKey::generate().unwrap().to_display().to_string();
        assert!(matches!(unlock_join(&fresh, &secrets, &candidates, &wrong, &binding()), Err(VaultError::NoVaultUnlocked)));
        assert!(unlock_join(&fresh, &secrets, &candidates, "garbage", &binding()).is_err());
        assert!(secrets.accounts().is_empty());
        let accounts = j.a.secrets.accounts();
        assert!(matches!(unlock_join(&j.a.store, &j.a.secrets, &candidates, &j.a.rk, &binding()), Err(VaultError::AlreadyMember)));
        assert_eq!(j.a.secrets.accounts(), accounts);
    }

    async fn reissued(remote: &FakeStore) -> (Created, String, ReissueStatus) {
        let mut a = create_on(remote).await;
        let pending = begin_reissue(remote, &a.secrets, &a.vault, &binding()).await.unwrap();
        let new_rk = pending.recovery_key().to_display().to_string();
        assert!(confirm_reissue(&mut a.store, &pending, &a.rk).is_err(), "the old RK does not confirm");
        assert_eq!(reissue_pending(&a.store).unwrap(), None);
        confirm_reissue(&mut a.store, &pending, &new_rk).unwrap();
        let (vault, bytes) = reissue_pending(&a.store).unwrap().unwrap();
        let status = publish_reissue(remote, &a.secrets, &vault, &bytes, &binding()).await.unwrap();
        finish_reissue(&mut a.store).unwrap();
        assert_eq!(reissue_pending(&a.store).unwrap(), None);
        (a, new_rk, status)
    }

    async fn assert_reissue_effect(remote: &FakeStore, a: &Created, new_rk: &str, status: ReissueStatus) {
        assert!(status.not_revocation);
        let key = manifest_key(&a.vault).unwrap();
        assert_eq!(remote.copies(&key).len(), 1, "exactly the new manifest remains");
        let candidates = fetch_candidates(remote, std::slice::from_ref(&a.vault)).await.unwrap();
        let joiner = Store::open_in_memory().unwrap();
        let secrets = MemoryStore::default();
        assert!(matches!(unlock_join(&joiner, &secrets, &candidates, &a.rk, &binding()), Err(VaultError::NoVaultUnlocked)));
        assert!(secrets.accounts().is_empty());
        unlock_join(&joiner, &secrets, &candidates, new_rk, &binding()).unwrap();
        // The DEK is unchanged: existing devices continue.
        let old_dek = load_vault_key(&a.secrets, &a.vault).unwrap().unwrap();
        assert!(verify_manifest(&candidates[0].manifest, &a.vault, &old_dek, &binding()).is_ok());
        assert_eq!(load_vault_key(&secrets, &a.vault).unwrap().unwrap(), old_dek);
        assert!(check_vaults(remote, &a.vault).await.unwrap().manifest_present);
    }

    #[tokio::test]
    async fn rk_reissue_on_create_only_store() {
        let remote = FakeStore::new(true);
        let (a, new_rk, status) = reissued(&remote).await;
        assert_reissue_effect(&remote, &a, &new_rk, status).await;
    }

    #[tokio::test]
    async fn rk_reissue_on_drive_creates_new_before_deleting_old() {
        let remote = FakeStore::new(false);
        let (a, new_rk, status) = reissued(&remote).await;
        assert_reissue_effect(&remote, &a, &new_rk, status).await;
    }

    /// A FakeStore that fails its `crash_at`-th call (1-based) — before the
    /// call reaches the store, or after it landed (lost response) — and
    /// asserts after every call that the manifest name is never empty (D3).
    struct Crashy<'a> {
        inner: &'a FakeStore,
        manifest: String,
        crash_at: usize,
        landed: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Crashy<'_> {
        fn before(&self) -> bool {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            n == self.crash_at && !self.landed
        }

        fn after<T>(&self, result: Result<T, ProviderError>) -> Result<T, ProviderError> {
            assert!(!self.inner.copies(&self.manifest).is_empty(), "a manifest-less window (call {})", self.calls());
            let n = self.calls();
            if n == self.crash_at && self.landed {
                return Err(ProviderError::Transient("crash after the call landed".to_owned()));
            }
            result
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn crash() -> ProviderError {
            ProviderError::Transient("crash before the call".to_owned())
        }
    }

    impl ObjectStore for Crashy<'_> {
        fn create_only(&self) -> bool {
            self.inner.create_only()
        }
        fn identity(&self) -> String {
            self.inner.identity()
        }
        async fn list(&self, q: ListQuery<'_>) -> Result<Vec<crate::sync::provider::ListedObject>, ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.list(q).await;
            self.after(r)
        }
        async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.get(key).await;
            self.after(r)
        }
        async fn get_all(&self, key: &str) -> Result<Vec<crate::sync::publish::RemoteObject>, ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.get_all(key).await;
            self.after(r)
        }
        async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<crate::sync::provider::ListedObject, ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.put_create(key, bytes).await;
            self.after(r)
        }
        async fn put_create_unchecked(
            &self,
            key: &str,
            bytes: &[u8],
        ) -> Result<crate::sync::provider::ListedObject, ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.put_create_unchecked(key, bytes).await;
            self.after(r)
        }
        async fn put_replace(
            &self,
            key: &str,
            bytes: &[u8],
            if_match: Option<&str>,
        ) -> Result<crate::sync::provider::ListedObject, ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.put_replace(key, bytes, if_match).await;
            self.after(r)
        }
        async fn delete(&self, key: &str) -> Result<(), ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.delete(key).await;
            self.after(r)
        }
        async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
            if self.before() {
                return Err(Self::crash());
            }
            let r = self.inner.delete_ref(provider_ref).await;
            self.after(r)
        }
        async fn quota(&self) -> Result<crate::sync::provider::Quota, ProviderError> {
            self.inner.quota().await
        }
        async fn ensure_container(&self) -> Result<(), ProviderError> {
            self.inner.ensure_container().await
        }
    }

    /// D3: a crash (before or after) at every remote call of the publish
    /// never leaves the vault without a manifest, and a resume finishes
    /// with exactly the new manifest (create-only: If-Match swap; Drive:
    /// create, verify, delete the observed old copy).
    #[tokio::test]
    async fn rk_reissue_has_no_manifest_less_window_at_any_crash_point() {
        for create_only in [true, false] {
            for crash_at in 1..=4 {
                for landed in [false, true] {
                    let remote = FakeStore::new(create_only);
                    let mut a = create_on(&remote).await;
                    let pending = begin_reissue(&remote, &a.secrets, &a.vault, &binding()).await.unwrap();
                    let new_rk = pending.recovery_key().to_display().to_string();
                    confirm_reissue(&mut a.store, &pending, &new_rk).unwrap();
                    let (vault, record) = reissue_pending(&a.store).unwrap().unwrap();
                    let key = manifest_key(&a.vault).unwrap();
                    let crashy = Crashy {
                        inner: &remote,
                        manifest: key.clone(),
                        crash_at,
                        landed,
                        calls: Default::default(),
                    };
                    let case = format!("create_only={create_only} crash_at={crash_at} landed={landed}");
                    assert!(publish_reissue(&crashy, &a.secrets, &vault, &record, &binding()).await.is_err(), "{case}");
                    assert!(!remote.copies(&key).is_empty(), "{case}");
                    // The pending reissue resumes it (boot or the dialog).
                    let (vault, record) = reissue_pending(&a.store).unwrap().expect("still pending after the crash");
                    let resume = Crashy { inner: &remote, manifest: key, crash_at: 0, landed, calls: Default::default() };
                    let status = publish_reissue(&resume, &a.secrets, &vault, &record, &binding()).await.unwrap();
                    finish_reissue(&mut a.store).unwrap();
                    assert_reissue_effect(&remote, &a, &new_rk, status).await;
                }
            }
        }
    }

    /// D3 / RT-B7: two devices reissue from the same manifest. On a keyed
    /// store the second swap fails its If-Match / sees a foreign-but-valid
    /// manifest: `ReissueSuperseded`, nothing written, and the first RK —
    /// the one the user confirmed — keeps opening the vault.
    #[tokio::test]
    async fn concurrent_reissue_supersedes_and_never_clobbers_a_confirmed_rk() {
        for create_only in [true, false] {
            let remote = FakeStore::new(create_only);
            let mut a = create_on(&remote).await;
            let b_secrets = MemoryStore::default();
            for account in a.secrets.accounts() {
                b_secrets.put_raw(&account, &a.secrets.raw(&account).unwrap());
            }
            let mut b_store = Store::open_in_memory().unwrap();
            let pending_a = begin_reissue(&remote, &a.secrets, &a.vault, &binding()).await.unwrap();
            let pending_b = begin_reissue(&remote, &b_secrets, &a.vault, &binding()).await.unwrap();
            let rk_a = pending_a.recovery_key().to_display().to_string();
            let rk_b = pending_b.recovery_key().to_display().to_string();
            confirm_reissue(&mut a.store, &pending_a, &rk_a).unwrap();
            confirm_reissue(&mut b_store, &pending_b, &rk_b).unwrap();
            let (vault, record_a) = reissue_pending(&a.store).unwrap().unwrap();
            let (_, record_b) = reissue_pending(&b_store).unwrap().unwrap();
            publish_reissue(&remote, &a.secrets, &vault, &record_a, &binding()).await.unwrap();
            finish_reissue(&mut a.store).unwrap();
            let key = manifest_key(&vault).unwrap();
            let after_a = remote.copies(&key);
            let watch = Crashy { inner: &remote, manifest: key.clone(), crash_at: 0, landed: false, calls: Default::default() };
            assert!(matches!(
                publish_reissue(&watch, &b_secrets, &vault, &record_b, &binding()).await,
                Err(VaultError::ReissueSuperseded)
            ));
            assert_eq!(remote.copies(&key), after_a, "create_only={create_only}: B wrote and deleted nothing");
            abandon_reissue(&mut b_store).unwrap();
            assert_eq!(reissue_pending(&b_store).unwrap(), None);
            assert!(matches!(abandon_reissue(&mut b_store), Err(VaultError::NothingPending)));
            let candidates = fetch_candidates(&remote, std::slice::from_ref(&vault)).await.unwrap();
            let joiner = Store::open_in_memory().unwrap();
            assert!(unlock_join(&joiner, &MemoryStore::default(), &candidates, &rk_a, &binding()).is_ok());
            assert!(unlock_join(&joiner, &MemoryStore::default(), &candidates, &rk_b, &binding()).is_err());
        }
    }

    /// D3 on Drive: B's create landed but its delete of the old copy did
    /// not, then A reissued from that old copy too. Both new copies stay
    /// (neither deletes the other); readers see one vault with a copies
    /// notice, each confirmed RK joins, the old RK no longer does.
    #[tokio::test]
    async fn drive_dual_manifest_copies_are_one_vault_and_each_rk_joins() {
        let remote = FakeStore::new(false);
        let mut a = create_on(&remote).await;
        let b_secrets = MemoryStore::default();
        for account in a.secrets.accounts() {
            b_secrets.put_raw(&account, &a.secrets.raw(&account).unwrap());
        }
        let mut b_store = Store::open_in_memory().unwrap();
        let pending_a = begin_reissue(&remote, &a.secrets, &a.vault, &binding()).await.unwrap();
        let pending_b = begin_reissue(&remote, &b_secrets, &a.vault, &binding()).await.unwrap();
        let rk_a = pending_a.recovery_key().to_display().to_string();
        let rk_b = pending_b.recovery_key().to_display().to_string();
        confirm_reissue(&mut a.store, &pending_a, &rk_a).unwrap();
        confirm_reissue(&mut b_store, &pending_b, &rk_b).unwrap();
        let (vault, record_a) = reissue_pending(&a.store).unwrap().unwrap();
        let (_, record_b) = reissue_pending(&b_store).unwrap().unwrap();
        let key = manifest_key(&vault).unwrap();
        // B: get_all, create, get_all, then the delete of the old copy fails.
        let b_crash = Crashy { inner: &remote, manifest: key.clone(), crash_at: 4, landed: false, calls: Default::default() };
        assert!(publish_reissue(&b_crash, &b_secrets, &vault, &record_b, &binding()).await.is_err());
        assert_eq!(remote.copies(&key).len(), 2, "old + B");
        publish_reissue(&remote, &a.secrets, &vault, &record_a, &binding()).await.unwrap();
        finish_reissue(&mut a.store).unwrap();
        // B resumes: its copy is in place; the old copy is already gone.
        publish_reissue(&remote, &b_secrets, &vault, &record_b, &binding()).await.unwrap();
        finish_reissue(&mut b_store).unwrap();
        assert_eq!(remote.copies(&key).len(), 2, "A's and B's copies, never one deleting the other");

        let check = check_vaults(&remote, &vault).await.unwrap();
        assert_eq!(check, VaultCheck { manifest_present: true, other_vaults: Vec::new(), manifest_copies: 2 });
        assert_eq!(discover(&remote, &NoPause::default(), || NOW).await, Discovery::Found(vec![vault.clone()]));
        let candidates = fetch_candidates(&remote, std::slice::from_ref(&vault)).await.unwrap();
        assert_eq!(candidates.len(), 2);
        for rk in [&rk_a, &rk_b] {
            let joiner = Store::open_in_memory().unwrap();
            let secrets = MemoryStore::default();
            let unlocked = unlock_join(&joiner, &secrets, &candidates, rk, &binding()).unwrap();
            assert_eq!(unlocked.vault_id, vault);
            assert_eq!(
                load_vault_key(&secrets, &vault).unwrap().unwrap(),
                load_vault_key(&a.secrets, &vault).unwrap().unwrap()
            );
        }
        let joiner = Store::open_in_memory().unwrap();
        let secrets = MemoryStore::default();
        assert!(matches!(
            unlock_join(&joiner, &secrets, &candidates, &a.rk, &binding()),
            Err(VaultError::NoVaultUnlocked)
        ));
        assert!(secrets.accounts().is_empty());
    }

    #[tokio::test]
    async fn rk_reissue_never_deletes_an_unverified_object() {
        let remote = FakeStore::new(true);
        let mut a = create_on(&remote).await;
        let pending = begin_reissue(&remote, &a.secrets, &a.vault, &binding()).await.unwrap();
        let new_rk = pending.recovery_key().to_display().to_string();
        confirm_reissue(&mut a.store, &pending, &new_rk).unwrap();
        let key = manifest_key(&a.vault).unwrap();
        remote.delete(&key).await.unwrap();
        remote.seed(&key, b"someone else's manifest");
        let (vault, bytes) = reissue_pending(&a.store).unwrap().unwrap();
        assert!(matches!(publish_reissue(&remote, &a.secrets, &vault, &bytes, &binding()).await, Err(VaultError::Conflict)));
        assert_eq!(remote.copies(&key), vec![b"someone else's manifest".to_vec()]);
    }

    #[tokio::test]
    async fn removing_a_device_deletes_only_its_record() {
        let remote = FakeStore::new(true);
        let rec = |d: &str| ObjectAddress::device_record("vault-a", 1, d).remote_key().unwrap();
        remote.seed(&rec("dev-1"), b"r1");
        remote.seed(&rec("dev-2"), b"r2");
        remote.seed("v1/vault-a/log/dev-2/000000000001-000000000001.seg", b"s");
        remove_device(&remote, "vault-a", "dev-1", "dev-2").await.unwrap();
        assert_eq!(keys(&remote, "v1/"), ["v1/vault-a/devices/dev-1.rec", "v1/vault-a/log/dev-2/000000000001-000000000001.seg"]);
        assert!(matches!(remove_device(&remote, "vault-a", "dev-1", "dev-1").await, Err(VaultError::SelfRemoval)));
        assert!(matches!(remove_device(&remote, "vault-a", "dev-1", "../x").await, Err(VaultError::InvalidId)));
        assert_eq!(keys(&remote, "v1/").len(), 2);
    }

    /// A vault with segments, plus an unrelated vault and a stray file.
    async fn wipe_fixture(create_only: bool) -> (FakeStore, Created, Vec<String>) {
        let remote = FakeStore::new(create_only);
        let mut a = create_on(&remote).await;
        a.store.insert_local_op(&a.vault, memo(M_MINE, "x"), UploadState::Never).unwrap();
        let segment = seal(&mut a.store, &a.secrets, &a.vault);
        upload(&remote, &a.vault, &segment).await;
        remote.seed(&format!("v1/{}/blob/ab/abcdef.blob", a.vault), b"b");
        if create_only {
            remote.seed(&format!("v1/{}/stray.tmp", a.vault), b"t");
        }
        let others = vec!["v1/vault-other/log/dev-9/000000000001-000000000001.seg".to_owned(), "v1/vault-other/manifest.json".to_owned()];
        for key in &others {
            remote.seed(key, b"other");
        }
        (remote, a, others)
    }

    #[tokio::test]
    async fn wipe_needs_typed_confirmation() {
        let (remote, mut a, _) = wipe_fixture(true).await;
        let before = remote.requests();
        assert!(matches!(begin_wipe(&mut a.store, &a.vault, "delete"), Err(VaultError::ConfirmationMismatch)));
        assert_eq!(a.store.meta_get("wipe_state").unwrap(), None);
        assert!(!wipe_in_progress(&a.store).unwrap());
        assert_eq!(remote.requests(), before);
    }

    async fn wipe_only_the_vault_prefix(create_only: bool) {
        let (remote, mut a, others) = wipe_fixture(create_only).await;
        begin_wipe(&mut a.store, &a.vault, WIPE_CONFIRMATION).unwrap();
        assert!(wipe_in_progress(&a.store).unwrap());
        assert_eq!(wipe_pending(&a.store).unwrap().as_deref(), Some(a.vault.as_str()));
        let pause = NoPause::default();
        let deleted = run_wipe(&remote, &pause, &a.vault).await.unwrap();
        assert_eq!(deleted, if create_only { 3 } else { 2 });
        assert_eq!(pause.taken(), [RECHECK_DELAYS[0], WIPE_FINAL_SWEEP_DELAY], "final sweep after a pause");
        assert_eq!(keys(&remote, "v1/"), others, "only the vault prefix is deleted");
        assert!(load_vault_key(&a.secrets, &a.vault).unwrap().is_some(), "DEK kept until done");
        finish_wipe(&mut a.store, &a.secrets, &a.vault).unwrap();
        assert_eq!(a.store.meta_get("wipe_state").unwrap().as_deref(), Some("done"));
        assert!(load_vault_key(&a.secrets, &a.vault).unwrap().is_none(), "local DEK deleted");
        assert!(wipe_in_progress(&a.store).unwrap(), "the engine never syncs the wiped vault");
        finish_wipe(&mut a.store, &a.secrets, &a.vault).unwrap();
    }

    #[tokio::test]
    async fn wipe_deletes_only_the_vault_prefix() {
        wipe_only_the_vault_prefix(true).await;
    }

    #[tokio::test]
    async fn wipe_on_drive_deletes_only_the_vault_props() {
        wipe_only_the_vault_prefix(false).await;
    }

    /// D7: a peer cycle that passed its manifest gate before the wipe lands
    /// a segment and its record during the wiper's final pause; the final
    /// sweep removes them, then confirms empty after another pause.
    #[tokio::test]
    async fn wipe_final_sweep_removes_a_late_peer_put() {
        struct LatePeer<'a> {
            remote: &'a FakeStore,
            vault: String,
            taken: Mutex<Vec<Duration>>,
        }
        impl Pause for LatePeer<'_> {
            async fn pause(&self, d: Duration) {
                let mut taken = self.taken.lock().unwrap();
                taken.push(d);
                if d == WIPE_FINAL_SWEEP_DELAY && taken.iter().filter(|t| **t == WIPE_FINAL_SWEEP_DELAY).count() == 1 {
                    let seg = ObjectAddress::segment(&self.vault, 1, "dev-late", 7, 7).remote_key().unwrap();
                    let rec = ObjectAddress::device_record(&self.vault, 1, "dev-late").remote_key().unwrap();
                    self.remote.seed(&seg, b"late segment");
                    self.remote.seed(&rec, b"late record");
                }
            }
        }
        for create_only in [true, false] {
            let (remote, mut a, others) = wipe_fixture(create_only).await;
            begin_wipe(&mut a.store, &a.vault, WIPE_CONFIRMATION).unwrap();
            let pause = LatePeer { remote: &remote, vault: a.vault.clone(), taken: Mutex::default() };
            let deleted = run_wipe(&remote, &pause, &a.vault).await.unwrap();
            assert_eq!(deleted, (if create_only { 3 } else { 2 }) + 2, "the late segment and record too");
            assert_eq!(keys(&remote, "v1/"), others, "create_only={create_only}: nothing of the vault survives");
            assert_eq!(
                *pause.taken.lock().unwrap(),
                [RECHECK_DELAYS[0], WIPE_FINAL_SWEEP_DELAY, WIPE_FINAL_SWEEP_DELAY],
                "a non-empty final sweep is confirmed by another pause"
            );
        }
    }

    /// D7: objects that keep appearing are reported, never read as done.
    #[tokio::test]
    async fn wipe_that_never_settles_is_incomplete() {
        struct Flood<'a>(&'a FakeStore, String, std::sync::atomic::AtomicU64);
        impl Pause for Flood<'_> {
            async fn pause(&self, _: Duration) {
                let n = self.2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let seg = ObjectAddress::segment(&self.1, 1, "dev-late", n, n).remote_key().unwrap();
                self.0.seed(&seg, b"late segment");
            }
        }
        let (remote, mut a, _) = wipe_fixture(true).await;
        begin_wipe(&mut a.store, &a.vault, WIPE_CONFIRMATION).unwrap();
        let flood = Flood(&remote, a.vault.clone(), std::sync::atomic::AtomicU64::new(0));
        assert!(matches!(run_wipe(&remote, &flood, &a.vault).await, Err(VaultError::WipeIncomplete(_))));
        assert_eq!(wipe_pending(&a.store).unwrap().as_deref(), Some(a.vault.as_str()), "still resumable");
    }

    #[tokio::test]
    async fn wipe_resumes_after_a_crash_and_other_devices_recreate_nothing() {
        let (remote, mut a, others) = wipe_fixture(true).await;
        begin_wipe(&mut a.store, &a.vault, WIPE_CONFIRMATION).unwrap();
        remote.fail_next(Op::List, ProviderError::Transient("crash".to_owned()));
        assert!(run_wipe(&remote, &NoPause::default(), &a.vault).await.is_err());
        assert!(remote.copies(&manifest_key(&a.vault).unwrap()).is_empty(), "manifest deleted first");
        assert!(!keys(&remote, &format!("v1/{}/log/", a.vault)).is_empty(), "segments left by the crash");

        // Another device of the vault sees the manifest gone: pause, no writes.
        let creates_before = keys(&remote, "v1/");
        let check = check_vaults(&remote, &a.vault).await.unwrap();
        assert!(!check.manifest_present);
        assert_eq!(keys(&remote, "v1/"), creates_before, "the other device recreates nothing");

        let vault = wipe_pending(&a.store).unwrap().expect("resumable");
        run_wipe(&remote, &NoPause::default(), &vault).await.unwrap();
        finish_wipe(&mut a.store, &a.secrets, &vault).unwrap();
        assert_eq!(keys(&remote, "v1/"), others);
        assert!(!check_vaults(&remote, &a.vault).await.unwrap().manifest_present);
        assert_eq!(keys(&remote, "v1/"), others);
        assert_eq!(wipe_pending(&a.store).unwrap(), None);
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(1_700_000_000_999), "2023-11-14T22:13:20Z");
    }
}
