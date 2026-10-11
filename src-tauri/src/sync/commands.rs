//! Tauri commands and the managed [`SyncRuntime`] (G7 design "sync/commands.rs
//! + lib.rs").
//!
//! Every command is a thin wrapper over an `AppHandle`-free method or helper;
//! the app side is reached only through the [`Host`] trait (clipboard handle,
//! save, emit, spawn), so the logic is unit-tested without a Tauri app.
//!
//! Decisions (recorded for review):
//! - **Flag.** The app keeps Rust-side settings as JSON files in
//!   `app_data_dir` (`clipboard.json`, `agent-usage-cache.json`, ...); the
//!   webview keeps its own in localStorage. The sync flag must be readable
//!   before the webview exists and only through sync commands, so it is
//!   `app_data_dir/sync-settings.json` (`{"enabled": bool, "local_only":
//!   bool}`), written atomically. Missing, unreadable, or malformed → off.
//!   With the flag off, boot touches nothing under `sync/` and builds no
//!   provider (AC 15): [`boot_plan`] is empty.
//! - **Disable at runtime** stops the scheduler, drops the provider, and
//!   uninstalls the sink, and records `local_only = true` (same atomic
//!   write as the flag) because memo commands are local-only from then on.
//!   The next native cutover reconciles the live memos into the store
//!   (`changed(None, live)`, which only edits what differs and recreates a
//!   tombstoned memo under a fresh id) before mirroring, so an edit made
//!   while off is never overwritten. A memo deleted while off reappears
//!   (no delete is inferred from absence: no data loss).
//! - **Locks.** clipboard sink slot → clipboard state → store → clock, all
//!   `std::sync::Mutex`, none held across an `.await`. The engine is moved
//!   out of its mutex for a cycle and put back. Memo commands hold the sink
//!   slot for their whole run, so a runtime cutover is one critical section
//!   (AD-A2) that no command can interleave with.
//! - **Connect.** Only one account is connected at a time; a different
//!   binding needs a disconnect first (D11). WebDAV is probed
//!   (`probe_conditional_create`) on connect and again on every session
//!   restore before the store reaches the engine (ADV-5). R2 and WebDAV
//!   secrets live only in the keychain (`SyncSecret::Credential`); the
//!   database keeps only the non-secret identity (base URL, user name,
//!   account id, bucket, OAuth subject). OAuth client ids come from the
//!   build environment (`SAYKNOW_SYNC_{GDRIVE,ONEDRIVE}_CLIENT_ID`,
//!   optional `..._CLIENT_SECRET`); unset → `not_configured`, fail closed.
//! - **Disconnect** without a wipe forgets the connection and the keychain
//!   credential only; local data and the vault key stay (an account switch
//!   republishes them, D11). With a wipe: the exact `"삭제"` first, then
//!   `vault::begin_wipe` → `run_wipe` → `finish_wipe`.
//! - **Boot finalize decision** ([`finalize_input`]): `Unreadable` → no
//!   finalize this boot; `Corrupt`, or `Missing` with a recorded clipboard →
//!   `None`; `Loaded`, or `Missing` without one → `Some(live)`. An unreadable
//!   migration manifest is treated like `Unreadable` (fail closed).
//! - **What the webview gets:** fixed codes and labels, counts, the account
//!   label (`user@host`, the bucket, or the provider plus a fingerprint
//!   prefix), and the RK only from create / reissue (D9b). Vault ids appear
//!   only as [`VaultTag`]; discovery returns a count.
//! - `apply_memo_projection` is not used: the mirror must be computed and
//!   applied under one clipboard lock (a projection read outside it could
//!   revert a memo command that ran in between), so [`clipboard::mirror_in`]
//!   does both.
//! - Device removal takes the device id from the webview; listing devices is
//!   G9 (the status carries none yet).
//! - **G8 chat.** `sync_chat_load` reads the native projection (`not_native`
//!   before the cutover). `sync:chat-changed`
//!   (payload `{}`) fires after a cycle that changed the projection or a
//!   changed `sync_queue_apply`, and only when a digest of the conversation
//!   and translation entities differs from the last load or signal (memo-only
//!   changes stay silent). The status adds `migration_state` (= `migration`),
//!   `queue_flushed_qseq` (the E3 watermark), `ack_qseq`, and
//!   `chat_src_fallbacks`. `sync_queue_apply` answers `applied`, `duplicate`
//!   (same qseq, same op hash) or `stale_qseq{watermark}` (E3).
//! - **G8 E1, native stays native.** A device whose existing `sync.db`
//!   reached `native` keeps `sync_chat_load`, `sync_queue_apply` (with its
//!   `sync:chat-changed`) and the full status while the flag is off; the
//!   flag stops only the network (scheduler, provider, discovery). Any
//!   other flag-off device gets `disabled`, and its `sync.db`, if any, is only
//!   read through a read-only peek: never created, opened, or migrated.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};
use zeroize::Zeroizing;

use super::crypto::{derive_subkey, Purpose, SecretKey};
use super::engine::{self, CycleCtx, CycleOutcome, CycleReport, CycleStatus, EngineNotice, SchedEvent, StopReason};
use super::memo_bridge::{self, MemoSink, StoreMemoSink};
use super::merge::{project, Projection};
use super::migration::{self, Finalize, MigrationDir, MigrationError, MigrationState, QueuedOp};
use super::oauth::{self, OAuthClientConfig, OAuthTokens, ProviderAuth};
use super::ops::{HlcClock, Kind};
use super::provider::gdrive::GDriveStore;
use super::provider::onedrive::OneDriveStore;
use super::provider::r2::{R2Config, R2Store};
use super::provider::transport::{HttpTransport, ReqwestTransport};
use super::provider::webdav::{WebDavConfig, WebDavStore};
use super::provider::{binding_fp, Bound, ListQuery, ObjectStore, ProviderError};
use super::publish::Pause;
use super::recovery::{RecoveryError, INITIAL_KEY_EPOCH};
use super::secrets::{delete_secret, load_vault_key, read_secret, write_secret, CloudProvider, KeyringStore, SecretError, SecretStore, SyncSecret};
use super::store::{now_ms, Store, StoreError};
use super::telemetry::{export_diagnostics, log_event, FailureClass, Field, StatsSnapshot, SyncStats, VaultTag};
use super::vault::{self, Discovery, JoinPreview, JoinSummary, PendingReissue, PendingVault, UnlockedVault, VaultError};
use crate::clipboard::{self, ClipEntry, ClipboardHandle, EntryKind, LoadOutcome};
use crate::oauth_callback::{cancel_native, start_native_listener, OAuthCallbackState, OAuthError};

const SETTINGS_FILE: &str = "sync-settings.json";
const SYNC_DIR: &str = "sync";
const STORE_FILE: &str = "sync.db";
const MIGRATION_DIR: &str = "sync-migration";
const CHECKPOINT_DIR: &str = "checkpoints";
const META_CONNECTION: &str = "connection";
/// The binding fingerprint the active vault was created / joined under. A
/// connection to another account leaves the vault inactive (`needs_vault`,
/// A8) until the user creates or joins one there.
const META_VAULT_BINDING: &str = "sync_vault_binding";
/// G9 U5: the `sync_dismiss_notice` argument that dismisses `restored`.
const RESTORED_NOTICE: &str = "restored";
/// G9 U5: `<prefix><vault_id>` holds the hex digests of the restored-after-
/// delete notices the user dismissed (JSON array; local only, never ids).
const META_RESTORED_DISMISSED: &str = "restored_dismissed:";
/// G9 U5: the dismissed set is bounded.
const RESTORED_DISMISSED_CAP: usize = 4096;
/// G9 U4: other devices' records are re-read at most this often, unless a
/// switch, disconnect or removal forces it.
const DEVICES_REFRESH_MS: u64 = 10 * 60 * 1000;
/// G9 U4: the length of a device id's display form.
const DEVICE_SHORT_LEN: usize = 8;
/// The native OAuth listener gives up after this long (§4.8).
const OAUTH_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Emitted (payload `{}`) when the conversations or translations of the
/// native projection changed (G8).
pub(crate) const CHAT_CHANGED_EVENT: &str = "sync:chat-changed";

#[cfg(target_os = "macos")]
const DEVICE_LABEL: &str = "SayKnow Kit (macOS)";
#[cfg(target_os = "windows")]
const DEVICE_LABEL: &str = "SayKnow Kit (Windows)";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DEVICE_LABEL: &str = "SayKnow Kit";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Flag (see the module doc)

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct SyncSettings {
    pub(crate) enabled: bool,
    /// Memo commands ran without a sink since the last native cutover.
    pub(crate) local_only: bool,
}

pub(crate) fn settings_path(dir: &Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

/// Fail closed (D9): sync is on only for a JSON object whose `enabled` is the
/// boolean `true`. Anything else (unreadable, not JSON, not an object, a
/// wrong type) is off; `local_only` is read the same way.
pub(crate) fn load_settings(dir: Option<&Path>) -> SyncSettings {
    let Some(dir) = dir else { return SyncSettings::default() };
    let Ok(raw) = std::fs::read(settings_path(dir)) else { return SyncSettings::default() };
    let object = match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Object(object)) => object,
        _ => {
            log_event("settings_malformed", &[]);
            return SyncSettings::default();
        }
    };
    let flag = |name: &str| object.get(name) == Some(&serde_json::Value::Bool(true));
    SyncSettings { enabled: flag("enabled"), local_only: flag("local_only") }
}

pub(crate) fn save_settings(dir: &Path, settings: SyncSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_vec(&settings).map_err(std::io::Error::other)?;
    clipboard::write_atomic(&settings_path(dir), &json)
}

// ---------------------------------------------------------------------------
// Errors the webview may see: a fixed code and a message free of secrets,
// paths, ids, and payloads.

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CommandError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl CommandError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn disabled() -> Self {
        Self::new("disabled", "sync is turned off")
    }

    /// E1a: the local `sync.db` could not be peeked, opened or loaded, so it
    /// is unknown whether this device is native. Never `disabled` and never
    /// the status defaults: the webview keeps its current mode and retries.
    fn store_error(class: &'static str) -> Self {
        log_event("command_error", &[("kind", Field::Label("store_error")), ("class", Field::Label(class))]);
        Self::new("store_error", "local sync database could not be read")
    }

    fn not_connected() -> Self {
        Self::new("not_connected", "connect a storage account first")
    }

    fn not_joined() -> Self {
        Self::new("not_joined", "create or join a vault first")
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The error class of a store failure, for content-free logs (A6): the
/// `Display` text can carry op ids, device ids, and sequence numbers.
fn store_error_kind(e: &StoreError) -> &'static str {
    match e {
        StoreError::Sql(_) => "sql",
        StoreError::Op(_) => "op",
        StoreError::Rotation(_) => "rotation",
        StoreError::Corrupt(_) => "corrupt",
        StoreError::NoIdentity => "no_identity",
        StoreError::Invalid(_) => "invalid",
        StoreError::Io(_) => "io",
    }
}

impl From<StoreError> for CommandError {
    fn from(e: StoreError) -> Self {
        log_event("command_error", &[("kind", Field::Label("store")), ("class", Field::Label(store_error_kind(&e)))]);
        Self::new("store", "local sync database error")
    }
}

impl From<SecretError> for CommandError {
    fn from(e: SecretError) -> Self {
        match e {
            SecretError::Backend(_) => Self::new("keychain", "keychain unavailable"),
            SecretError::InvalidName => Self::new("keychain", "invalid keychain entry name"),
            SecretError::Corrupt => Self::new("keychain", "keychain entry is corrupt"),
        }
    }
}

impl From<ProviderError> for CommandError {
    fn from(e: ProviderError) -> Self {
        let code = match &e {
            ProviderError::Auth { reconsent: true } => "reconsent",
            ProviderError::Auth { reconsent: false } => "permission_denied",
            ProviderError::RateLimited { .. } => "rate_limited",
            ProviderError::QuotaExceeded => "quota",
            ProviderError::NotFound => "configuration",
            ProviderError::AlreadyExists | ProviderError::PreconditionFailed => "conflict",
            ProviderError::Unsupported(_) => "unsupported",
            ProviderError::BindingMismatch => "binding_mismatch",
            ProviderError::Transient(_) => "transient",
            ProviderError::Permanent(_) => "permanent",
        };
        // Provider strings never carry a credential or an authenticated URL.
        let message = match e {
            ProviderError::Unsupported(text) | ProviderError::Transient(text) | ProviderError::Permanent(text) => text,
            other => format!("{other:?}"),
        };
        Self::new(code, message)
    }
}

impl From<RecoveryError> for CommandError {
    fn from(e: RecoveryError) -> Self {
        let code = match e {
            RecoveryError::Format | RecoveryError::Checksum => "rk_format",
            RecoveryError::WrongKey => "rk_wrong",
            RecoveryError::BindingMismatch => "binding_mismatch",
            RecoveryError::Tampered | RecoveryError::VaultMismatch => "manifest_invalid",
            RecoveryError::Unsupported => "unsupported",
            RecoveryError::InvalidInput | RecoveryError::Crypto(_) => "internal",
        };
        Self::new(code, e.to_string())
    }
}

impl From<VaultError> for CommandError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::Provider(p) => p.into(),
            VaultError::Store(s) => s.into(),
            VaultError::Secret(s) => s.into(),
            VaultError::Recovery(r) => r.into(),
            other => {
                let code = match &other {
                    VaultError::DiscoveryRequired => "discovery_required",
                    VaultError::NoVaultUnlocked => "rk_wrong",
                    VaultError::Ambiguous => "ambiguous",
                    VaultError::AlreadyMember => "already_member",
                    VaultError::JoinIncomplete(_) => "join_incomplete",
                    VaultError::PreviewStale(_) => "preview_stale",
                    VaultError::NotConfirmed => "not_confirmed",
                    VaultError::Conflict => "conflict",
                    VaultError::Verification(_) => "manifest_invalid",
                    VaultError::ConfirmationMismatch => "confirmation_mismatch",
                    VaultError::SelfRemoval => "self_removal",
                    VaultError::InvalidId => "invalid_id",
                    VaultError::WipeIncomplete(_) => "wipe_incomplete",
                    VaultError::Busy => "busy",
                    VaultError::NothingPending => "nothing_pending",
                    VaultError::JoinRejected => "join_rejected",
                    VaultError::NoKey => "no_key",
                    VaultError::ReissueSuperseded => "reissue_superseded",
                    VaultError::Provider(_) | VaultError::Store(_) | VaultError::Secret(_) | VaultError::Recovery(_) => {
                        unreachable!("handled above")
                    }
                };
                Self::new(code, other.to_string())
            }
        }
    }
}

impl From<MigrationError> for CommandError {
    fn from(e: MigrationError) -> Self {
        match e {
            MigrationError::Store(s) => s.into(),
            MigrationError::Chunk(why) => Self::new("migration_chunk", why),
            MigrationError::WrongState(state) => Self::new("migration_state", state.as_str()),
            other => {
                let class = match other {
                    MigrationError::Io(_) => "io",
                    MigrationError::Json(_) => "json",
                    MigrationError::UnknownState(_) => "unknown_state",
                    MigrationError::Corrupt(_) => "corrupt",
                    _ => "other",
                };
                log_event("command_error", &[("kind", Field::Label("migration")), ("class", Field::Label(class))]);
                Self::new("migration", "migration backup could not be processed")
            }
        }
    }
}

impl From<OAuthError> for CommandError {
    fn from(e: OAuthError) -> Self {
        let code = match e {
            OAuthError::Cancelled => "cancelled",
            OAuthError::Timeout => "timeout",
            OAuthError::Busy => "busy",
            OAuthError::EmptyState | OAuthError::Bind(_) => "oauth",
        };
        Self::new(code, e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Connections

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    Gdrive,
    Onedrive,
    Webdav,
    R2,
}

impl ProviderKind {
    fn cloud(self) -> CloudProvider {
        match self {
            Self::Gdrive => CloudProvider::GoogleDrive,
            Self::Onedrive => CloudProvider::OneDrive,
            Self::Webdav => CloudProvider::WebDav,
            Self::R2 => CloudProvider::R2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Gdrive => "gdrive",
            Self::Onedrive => "onedrive",
            Self::Webdav => "webdav",
            Self::R2 => "r2",
        }
    }
}

/// The non-secret half of a connection (in `meta.connection`). The secret
/// half lives only in the keychain under `Credential { provider, fp }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConnectMeta {
    pub(crate) binding_fp: String,
    pub(crate) params: ConnectParams,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub(crate) enum ConnectParams {
    Gdrive { sub: String },
    Onedrive { tenant_id: String, object_id: String },
    Webdav { base_url: String, username: String },
    R2 { account_id: String, bucket: String },
}

impl ConnectMeta {
    pub(crate) fn kind(&self) -> ProviderKind {
        match self.params {
            ConnectParams::Gdrive { .. } => ProviderKind::Gdrive,
            ConnectParams::Onedrive { .. } => ProviderKind::Onedrive,
            ConnectParams::Webdav { .. } => ProviderKind::Webdav,
            ConnectParams::R2 { .. } => ProviderKind::R2,
        }
    }

    /// What the UI shows (`{provider, account_label}`).
    pub(crate) fn account_label(&self) -> String {
        match &self.params {
            ConnectParams::Webdav { base_url, username } => {
                let host = url::Url::parse(base_url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default();
                format!("{username}@{host}")
            }
            ConnectParams::R2 { bucket, .. } => bucket.clone(),
            ConnectParams::Gdrive { .. } | ConnectParams::Onedrive { .. } => {
                format!("{} {}", self.kind().label(), &self.binding_fp[..self.binding_fp.len().min(8)])
            }
        }
    }

    fn secret(&self) -> SyncSecret<'_> {
        SyncSecret::Credential { provider: self.kind().cloud(), binding_fp: &self.binding_fp }
    }
}

pub(crate) fn load_connection(store: &Store) -> Result<Option<ConnectMeta>, StoreError> {
    Ok(store.meta_get(META_CONNECTION)?.filter(|v| !v.is_empty()).and_then(|v| serde_json::from_str(&v).ok()))
}

fn save_connection(store: &mut Store, meta: Option<&ConnectMeta>) -> Result<(), StoreError> {
    let text = match meta {
        Some(meta) => serde_json::to_string(meta).map_err(|e| StoreError::Invalid(e.to_string()))?,
        None => String::new(),
    };
    store.meta_set(META_CONNECTION, &text)
}

/// Webview input for WebDAV / R2. Secrets print `<redacted>`.
#[derive(Deserialize)]
pub(crate) struct WebDavParams {
    pub(crate) base_url: String,
    pub(crate) username: String,
    pub(crate) password: String,
}

#[derive(Deserialize)]
pub(crate) struct R2Params {
    pub(crate) account_id: String,
    pub(crate) bucket: String,
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
}

/// The R2 keychain value.
#[derive(Serialize, Deserialize)]
struct R2Secret {
    access_key_id: String,
    secret_access_key: String,
}

type Tokens = OAuthTokens<KeyringStore, ReqwestTransport>;

/// The bound adapter the engine runs against.
pub(crate) enum ActiveProvider {
    Drive(Bound<GDriveStore<ReqwestTransport, Tokens>>),
    OneDrive(Bound<OneDriveStore<ReqwestTransport, Tokens>>),
    WebDav(Bound<WebDavStore<ReqwestTransport>>),
    R2(Bound<R2Store<ReqwestTransport>>),
}

macro_rules! with_provider {
    ($provider:expr, $p:ident => $body:expr) => {
        match $provider {
            ActiveProvider::Drive($p) => $body,
            ActiveProvider::OneDrive($p) => $body,
            ActiveProvider::WebDav($p) => $body,
            ActiveProvider::R2($p) => $body,
        }
    };
}

fn transport() -> Result<ReqwestTransport, ProviderError> {
    ReqwestTransport::new().map_err(ProviderError::from)
}

/// The OAuth registration from the build environment; unset fails closed.
fn oauth_client(kind: ProviderKind) -> Result<ProviderAuth, CommandError> {
    let (id, secret) = match kind {
        ProviderKind::Gdrive => (option_env!("SAYKNOW_SYNC_GDRIVE_CLIENT_ID"), option_env!("SAYKNOW_SYNC_GDRIVE_CLIENT_SECRET")),
        ProviderKind::Onedrive => {
            (option_env!("SAYKNOW_SYNC_ONEDRIVE_CLIENT_ID"), option_env!("SAYKNOW_SYNC_ONEDRIVE_CLIENT_SECRET"))
        }
        ProviderKind::Webdav | ProviderKind::R2 => return Err(CommandError::new("unsupported", "provider does not use OAuth")),
    };
    let Some(client_id) = id.filter(|v| !v.is_empty()) else {
        return Err(CommandError::new("not_configured", "this build has no OAuth client for the provider"));
    };
    let client = OAuthClientConfig { client_id: client_id.to_owned(), client_secret: secret.filter(|v| !v.is_empty()).map(str::to_owned) };
    Ok(match kind {
        ProviderKind::Gdrive => ProviderAuth::drive(client),
        _ => ProviderAuth::onedrive(client),
    })
}

/// `identity_from_id_token` text → the non-secret connection half.
pub(crate) fn parse_oauth_identity(identity: &str) -> Result<ConnectMeta, CommandError> {
    let bad = || CommandError::new("oauth", "the sign-in returned an unusable account identity");
    let parts: Vec<&str> = identity.split('|').collect();
    let params = match parts.as_slice() {
        ["gdrive", sub] => ConnectParams::Gdrive { sub: (*sub).to_owned() },
        ["onedrive", tid, oid] => ConnectParams::Onedrive { tenant_id: (*tid).to_owned(), object_id: (*oid).to_owned() },
        _ => return Err(bad()),
    };
    Ok(ConnectMeta { binding_fp: binding_fp(identity), params })
}

/// Rebuilds the adapter of a stored connection: WebDAV / R2 credentials come
/// from `secrets` (the runtime's keychain), OAuth tokens from the keychain.
/// WebDAV is probed again (every session start, ADV-5). Fails on a binding
/// drift.
async fn restore_provider(meta: &ConnectMeta, secrets: &impl SecretStore) -> Result<ActiveProvider, ProviderError> {
    let fp = meta.binding_fp.clone();
    let provider = match &meta.params {
        ConnectParams::Gdrive { sub } => {
            let cfg = oauth_client(ProviderKind::Gdrive).map_err(|e| ProviderError::Unsupported(e.message))?;
            let tokens = OAuthTokens::new(KeyringStore, transport()?, cfg, fp.clone());
            ActiveProvider::Drive(Bound::new(GDriveStore::new(transport()?, tokens, sub.clone())?, fp))
        }
        ConnectParams::Onedrive { tenant_id, object_id } => {
            let cfg = oauth_client(ProviderKind::Onedrive).map_err(|e| ProviderError::Unsupported(e.message))?;
            let tokens = OAuthTokens::new(KeyringStore, transport()?, cfg, fp.clone());
            ActiveProvider::OneDrive(Bound::new(OneDriveStore::new(transport()?, tokens, tenant_id, object_id)?, fp))
        }
        ConnectParams::Webdav { base_url, username } => {
            let password = read_credential(secrets, meta)?;
            let password = String::from_utf8(password.to_vec()).map_err(|_| ProviderError::Auth { reconsent: true })?;
            let store = WebDavStore::new(
                WebDavConfig { base_url: base_url.clone(), username: username.clone(), password },
                transport()?,
            )?;
            store.probe_conditional_create().await?;
            ActiveProvider::WebDav(Bound::new(store, fp))
        }
        ConnectParams::R2 { account_id, bucket } => {
            let raw = read_credential(secrets, meta)?;
            let secret: R2Secret = serde_json::from_slice(&raw).map_err(|_| ProviderError::Auth { reconsent: true })?;
            let config = R2Config {
                account_id: account_id.clone(),
                bucket: bucket.clone(),
                access_key_id: secret.access_key_id,
                secret_access_key: secret.secret_access_key,
            };
            ActiveProvider::R2(Bound::new(R2Store::new(config, transport()?)?, fp))
        }
    };
    let identity = with_provider!(&provider, p => p.identity());
    if binding_fp(&identity) != meta.binding_fp {
        return Err(ProviderError::BindingMismatch);
    }
    Ok(provider)
}

fn read_credential(secrets: &impl SecretStore, meta: &ConnectMeta) -> Result<Zeroizing<Vec<u8>>, ProviderError> {
    match read_secret(secrets, meta.secret()) {
        Ok(Some(bytes)) => Ok(bytes),
        Ok(None) | Err(SecretError::Corrupt) => Err(ProviderError::Auth { reconsent: true }),
        Err(SecretError::Backend(_)) => Err(ProviderError::Transient("keychain unavailable".to_owned())),
        Err(SecretError::InvalidName) => Err(ProviderError::Permanent("invalid credential binding".to_owned())),
    }
}

/// A connect-time check: the container exists and listing works (a missing
/// bucket or folder answers NotFound, which is a configuration error).
pub(crate) async fn connect_check<P: ObjectStore>(store: &P) -> Result<(), ProviderError> {
    store.ensure_container().await?;
    let query = if store.create_only() {
        ListQuery::Prefix("v1/")
    } else {
        ListQuery::Props { vault_id: None, kind: Some(super::provider::RemoteKind::Manifest), device_id: None }
    };
    store.list(query).await.map(|_| ())
}

/// WebDAV connect: the conditional-create probe must pass before anything
/// is stored; then the password goes to the keychain only.
pub(crate) async fn connect_webdav<T: HttpTransport>(
    secrets: &impl SecretStore,
    params: WebDavParams,
    transport: T,
) -> Result<(WebDavStore<T>, ConnectMeta), CommandError> {
    let password = Zeroizing::new(params.password);
    let config = WebDavConfig { base_url: params.base_url, username: params.username.clone(), password: password.to_string() };
    let store = WebDavStore::new(config, transport)?;
    store.probe_conditional_create().await?;
    connect_check(&store).await?;
    let identity = store.identity();
    let base_url = identity.split('|').nth(1).unwrap_or_default().to_owned();
    let meta = ConnectMeta { binding_fp: binding_fp(&identity), params: ConnectParams::Webdav { base_url, username: params.username } };
    write_secret(secrets, meta.secret(), password.as_bytes())?;
    Ok((store, meta))
}

/// R2 connect: the bucket must list; then the key pair goes to the keychain.
pub(crate) async fn connect_r2<T: HttpTransport>(
    secrets: &impl SecretStore,
    params: R2Params,
    transport: T,
) -> Result<(R2Store<T>, ConnectMeta), CommandError> {
    let secret = R2Secret { access_key_id: params.access_key_id, secret_access_key: params.secret_access_key };
    let config = R2Config {
        account_id: params.account_id.clone(),
        bucket: params.bucket.clone(),
        access_key_id: secret.access_key_id.clone(),
        secret_access_key: secret.secret_access_key.clone(),
    };
    let store = R2Store::new(config, transport)?;
    connect_check(&store).await?;
    let meta = ConnectMeta {
        binding_fp: binding_fp(&store.identity()),
        params: ConnectParams::R2 { account_id: params.account_id, bucket: params.bucket },
    };
    let json = Zeroizing::new(serde_json::to_vec(&secret).map_err(|_| CommandError::new("internal", "credential encoding"))?);
    write_secret(secrets, meta.secret(), &json)?;
    Ok((store, meta))
}

// ---------------------------------------------------------------------------
// Command validation helpers (AppHandle-free, unit-tested)

/// A cloud wipe needs exactly `"삭제"` (§4.7.1); anything else changes nothing.
pub(crate) fn check_wipe_confirmation(wipe_cloud: bool, confirm: &str) -> Result<(), CommandError> {
    if wipe_cloud && confirm != vault::WIPE_CONFIRMATION {
        return Err(VaultError::ConfirmationMismatch.into());
    }
    Ok(())
}

/// Create steps 3–5 after the RK display: the re-entered RK must unlock the
/// new manifest (a wrong one changes nothing and may be retried), then the
/// manifest is published create-only and verified. No local state changes.
pub(crate) async fn publish_create<P: ObjectStore, S: SecretStore>(
    remote: &P,
    secrets: &S,
    pending: &mut PendingVault,
    reentered: &str,
) -> Result<(), CommandError> {
    vault::confirm_create(pending, secrets, reentered)?;
    vault::publish_created(remote, secrets, pending).await?;
    Ok(())
}

/// Create steps 3–6 in one call (tests): [`publish_create`], then the
/// activation. The runtime runs the activation inside the vault-switch
/// critical section instead.
#[cfg(test)]
pub(crate) async fn finish_create<P: ObjectStore, S: SecretStore>(
    remote: &P,
    secrets: &S,
    store: &Mutex<Store>,
    clock: &Mutex<HlcClock>,
    pending: &mut PendingVault,
    reentered: &str,
    install_fp: Option<String>,
) -> Result<vault::JoinCommit, CommandError> {
    publish_create(remote, secrets, pending, reentered).await?;
    let mut store = lock(store);
    let mut clock = lock(clock);
    Ok(vault::activate_created(&mut store, secrets, pending, install_fp, &mut clock, now_ms())?)
}

// ---------------------------------------------------------------------------
// Boot

/// One step of the flag-on boot (G7 design "Boot sequence").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootStep {
    OpenStore,
    ResumeMigration,
    NativeCutover,
    StartScheduler,
}

/// The boot decision (AC 15): nothing at all with the flag off. The
/// clipboard load runs before this in either case, exactly as without sync.
pub(crate) fn boot_plan(enabled: bool) -> &'static [BootStep] {
    if enabled {
        &[BootStep::OpenStore, BootStep::ResumeMigration, BootStep::NativeCutover, BootStep::StartScheduler]
    } else {
        &[]
    }
}

/// Which memos `migration::finalize` may use (G4 delta-review rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalizeInput {
    /// `Unreadable` boot: do not finalize this boot.
    NotThisBoot,
    /// `None`: the backup memos stand.
    Backup,
    /// `Some(live)`.
    Live,
}

pub(crate) fn finalize_input(outcome: Option<&LoadOutcome>, clipboard_recorded: Option<bool>) -> FinalizeInput {
    match (outcome, clipboard_recorded) {
        (None | Some(LoadOutcome::Unreadable), _) | (_, None) => FinalizeInput::NotThisBoot,
        (Some(LoadOutcome::Corrupt { .. }), Some(_)) => FinalizeInput::Backup,
        (Some(LoadOutcome::Missing), Some(true)) => FinalizeInput::Backup,
        (Some(LoadOutcome::Missing), Some(false)) | (Some(LoadOutcome::Loaded), Some(_)) => FinalizeInput::Live,
    }
}

/// Whether the migration manifest recorded a clipboard; `None` when it
/// cannot be read (fail closed).
fn clipboard_recorded(dir: &MigrationDir) -> Option<bool> {
    let raw = std::fs::read(dir.manifest()).ok()?;
    let manifest: migration::Manifest = serde_json::from_slice(&raw).ok()?;
    Some(manifest.clipboard_sha256.is_some())
}

/// The finalize critical section (AD-A2) over injected steps: under the
/// sink slot and the clipboard lock, the live memos are snapshotted (per
/// `input`), `finalize` runs, and only on `Native` the mirror is applied,
/// saved, and the sink installed.
pub(crate) fn finalize_cutover(
    clip: &ClipboardHandle,
    input: FinalizeInput,
    finalize: impl FnOnce(Option<&[ClipEntry]>) -> Result<Finalize, CommandError>,
    mirror: impl FnOnce(&[ClipEntry]) -> Result<Vec<ClipEntry>, CommandError>,
    sink: impl FnOnce() -> Arc<dyn MemoSink>,
    save: impl FnOnce(&[ClipEntry], usize),
) -> Result<Finalize, CommandError> {
    if input == FinalizeInput::NotThisBoot {
        return Err(CommandError::new("not_this_boot", "clipboard history was unreadable this session; retried next start"));
    }
    let mut outcome = None;
    clipboard::cutover_in(
        clip,
        |entries| {
            let live: Vec<ClipEntry> = entries.iter().filter(|e| e.kind == EntryKind::Memo).cloned().collect();
            let result = finalize((input == FinalizeInput::Live).then_some(live.as_slice()))?;
            let native = matches!(result, Finalize::Native { .. });
            outcome = Some(result);
            if !native {
                return Ok::<_, CommandError>(None);
            }
            Ok(Some((mirror(entries)?, sink())))
        },
        save,
    )?;
    outcome.ok_or_else(|| CommandError::new("internal", "finalize produced no result"))
}

// ---------------------------------------------------------------------------
// Host: everything app-side the runtime touches.

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub(crate) trait Host: Send + Sync {
    fn clipboard(&self) -> &ClipboardHandle;
    /// The atomic clipboard save (honours the load's save block).
    fn save_clipboard(&self, entries: &[ClipEntry], max_entries: usize);
    /// `clipboard:changed` + `clipboard:cleared` (the webview refetches).
    fn clipboard_changed(&self);
    /// `sync:chat-changed` with a content-free `{}` (G8): the webview
    /// re-runs `sync_chat_load`. Hosts that show no chat ignore it.
    fn chat_changed(&self) {}
    fn spawn(&self, task: BoxFuture) -> Option<tauri::async_runtime::JoinHandle<()>>;
}

pub(crate) struct AppHost {
    pub(crate) app: AppHandle,
    pub(crate) clipboard: Arc<ClipboardHandle>,
}

impl Host for AppHost {
    fn clipboard(&self) -> &ClipboardHandle {
        &self.clipboard
    }

    fn save_clipboard(&self, entries: &[ClipEntry], max_entries: usize) {
        clipboard::save_persisted(&self.app, &self.clipboard, entries, max_entries);
    }

    fn clipboard_changed(&self) {
        let _ = self.app.emit("clipboard:changed", ());
        let _ = self.app.emit("clipboard:cleared", ());
    }

    fn chat_changed(&self) {
        let _ = self.app.emit(CHAT_CHANGED_EVENT, serde_json::json!({}));
    }

    fn spawn(&self, task: BoxFuture) -> Option<tauri::async_runtime::JoinHandle<()>> {
        Some(tauri::async_runtime::spawn(task))
    }
}

struct TokioPause;

impl Pause for TokioPause {
    async fn pause(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

type Events = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<SchedEvent>>>>;

/// The store sink plus a debounce nudge to the scheduler after a commit.
/// It refuses (the command fails, nothing changes) once its vault is no
/// longer the active one: an edit is never committed to a vault that was
/// switched away from (CB-B2). The switch holds the sink slot, as every memo
/// command does, so this check cannot race it.
struct NotifyingSink {
    inner: StoreMemoSink,
    events: Events,
}

impl NotifyingSink {
    fn nudge(&self) {
        if let Some(tx) = lock(&self.events).as_ref() {
            let _ = tx.send(SchedEvent::LocalChange);
        }
    }

    fn current(&self) -> Result<(), String> {
        match lock(&self.inner.store).active_vault() {
            Ok(Some(active)) if active == self.inner.vault => Ok(()),
            Ok(_) => Err("the sync vault was switched; retry".to_owned()),
            Err(e) => Err(format!("local sync database error ({})", store_error_kind(&e))),
        }
    }
}

impl MemoSink for NotifyingSink {
    fn changed(&self, before: Option<&ClipEntry>, after: Option<&ClipEntry>) -> Result<Option<String>, String> {
        self.current()?;
        let out = self.inner.changed(before, after)?;
        self.nudge();
        Ok(out)
    }

    fn delete_all(&self, ids: &[String]) -> Result<(), String> {
        self.current()?;
        self.inner.delete_all(ids)?;
        self.nudge();
        Ok(())
    }
}

struct SchedulerHandle {
    tx: tokio::sync::mpsc::UnboundedSender<SchedEvent>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// Runtime (managed state)

pub(crate) type InstallFp = Box<dyn Fn() -> Option<String> + Send + Sync>;

pub(crate) struct SyncRuntime<S: SecretStore = KeyringStore> {
    /// `app_data_dir`; `None` keeps sync off.
    dir: Option<PathBuf>,
    flag: AtomicBool,
    store: OnceLock<Arc<Mutex<Store>>>,
    clock: Arc<Mutex<HlcClock>>,
    provider: Mutex<Option<Arc<ActiveProvider>>>,
    stats: Arc<SyncStats>,
    engine: Mutex<engine::Engine>,
    /// The vault-switch fence (D6): bumped by every create / join / rejoin
    /// commit. A cycle that started under an older value never puts its
    /// engine back, records, mirrors, or writes another object.
    vault_gen: AtomicU64,
    scheduler: Mutex<Option<SchedulerHandle>>,
    events: Events,
    secrets: S,
    install_fp: InstallFp,
    load_outcome: Mutex<Option<LoadOutcome>>,
    finalize_input: Mutex<FinalizeInput>,
    last_status: Mutex<Option<CycleStatus>>,
    last_cycle_at: Mutex<Option<u64>>,
    notices: Mutex<BTreeSet<EngineNotice>>,
    discovered: Mutex<Vec<String>>,
    pending_create: Mutex<Option<PendingVault>>,
    pending_reissue: Mutex<Option<PendingReissue>>,
    pending_join: Mutex<Option<(UnlockedVault, JoinPreview)>>,
    /// Digest of the conversation and translation entities last loaded or
    /// signalled (G8): `sync:chat-changed` fires only when it moves.
    chat_fp: Mutex<Option<[u8; 32]>>,
    /// `src_fallbacks` of the last `sync_chat_load` (status only).
    chat_src_fallbacks: AtomicU64,
    /// G9 U4: the vault's devices from their verified records; `None` until
    /// read, and again after a switch, disconnect or removal (forces a read).
    devices: Mutex<Option<DevicesCache>>,
    /// G9 U5: restored-after-delete digests of the active vault's projection
    /// and the dismissed ones; `None` until the first cycle computes them.
    restored: Mutex<Option<RestoredCache>>,
}

impl<S: SecretStore + Send + Sync + 'static> SyncRuntime<S> {
    pub(crate) fn new(dir: Option<PathBuf>, secrets: S, install_fp: InstallFp) -> Self {
        let enabled = load_settings(dir.as_deref()).enabled;
        Self {
            dir,
            flag: AtomicBool::new(enabled),
            store: OnceLock::new(),
            clock: Arc::new(Mutex::new(HlcClock::default())),
            provider: Mutex::new(None),
            stats: Arc::new(SyncStats::default()),
            engine: Mutex::new(engine::Engine::default()),
            vault_gen: AtomicU64::new(0),
            scheduler: Mutex::new(None),
            events: Arc::new(Mutex::new(None)),
            secrets,
            install_fp,
            load_outcome: Mutex::new(None),
            finalize_input: Mutex::new(FinalizeInput::NotThisBoot),
            last_status: Mutex::new(None),
            last_cycle_at: Mutex::new(None),
            notices: Mutex::new(BTreeSet::new()),
            discovered: Mutex::new(Vec::new()),
            pending_create: Mutex::new(None),
            pending_reissue: Mutex::new(None),
            pending_join: Mutex::new(None),
            chat_fp: Mutex::new(None),
            chat_src_fallbacks: AtomicU64::new(0),
            devices: Mutex::new(None),
            restored: Mutex::new(None),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn require_enabled(&self) -> Result<(), CommandError> {
        if self.enabled() {
            Ok(())
        } else {
            Err(CommandError::disabled())
        }
    }

    fn data_dir(&self) -> Result<&Path, CommandError> {
        self.dir.as_deref().ok_or_else(|| CommandError::new("unavailable", "no app data directory"))
    }

    fn migration_dir(&self) -> Result<MigrationDir, CommandError> {
        let dir = self.data_dir()?;
        Ok(MigrationDir::new(dir.join(MIGRATION_DIR), dir.join("clipboard.json")))
    }

    /// Opens `sync/sync.db` once. Flag-on paths call this; flag-off paths
    /// reach it only through [`SyncRuntime::local_store`] for a device whose
    /// existing store is already native (E1).
    fn open_store(&self) -> Result<Arc<Mutex<Store>>, CommandError> {
        if let Some(store) = self.store.get() {
            return Ok(store.clone());
        }
        let path = self.data_dir()?.join(SYNC_DIR).join(STORE_FILE);
        let opened = Arc::new(Mutex::new(Store::open(&path)?));
        Ok(self.store.get_or_init(|| opened).clone())
    }

    fn store(&self) -> Result<Arc<Mutex<Store>>, CommandError> {
        self.require_enabled()?;
        self.open_store()
    }

    /// The store for the local chat commands (G8 E1, "native stays native"):
    /// with the flag on, the store; with the flag off, only a store that
    /// already reached `native`, whose SQLite stays the local source of
    /// truth. With the flag off, `disabled` is only a definite answer: no
    /// `sync.db`, or a readable one that is not native (only peeked at, never
    /// created or migrated). A peek, open or load failure is `store_error`
    /// (E1a), never `disabled`.
    fn local_store(&self) -> Result<Arc<Mutex<Store>>, CommandError> {
        if self.enabled() {
            return self.open_store();
        }
        if let Some(store) = self.store.get() {
            return match MigrationState::load(&lock(store)) {
                Ok(MigrationState::Native) => Ok(store.clone()),
                Ok(_) => Err(CommandError::disabled()),
                Err(_) => Err(CommandError::store_error("load")),
            };
        }
        // No app data directory: there is no store at all.
        let Ok(dir) = self.data_dir() else { return Err(CommandError::disabled()) };
        if !self.peek_native(dir)? {
            return Err(CommandError::disabled());
        }
        let store = self.open_store().map_err(|_| CommandError::store_error("open"))?;
        let state = MigrationState::load(&lock(&store));
        match state {
            Ok(MigrationState::Native) => Ok(store),
            Ok(_) => Err(CommandError::disabled()),
            Err(_) => Err(CommandError::store_error("load")),
        }
    }

    /// Whether `sync.db` under `dir` already reached `native`, without
    /// opening it as a store (E1a: a read failure is `store_error`).
    fn peek_native(&self, dir: &Path) -> Result<bool, CommandError> {
        MigrationState::file_is_native(&dir.join(SYNC_DIR).join(STORE_FILE)).map_err(|_| CommandError::store_error("peek"))
    }

    /// The store `status` reports (it never creates one). With the flag off,
    /// only a native store (E1); with the flag on, the open store, or a
    /// native `sync.db` not opened yet. `Ok(None)` is a definite "nothing to
    /// report"; `Err` is a store read failure (E1a), never the defaults.
    fn status_store(&self, enabled: bool) -> Result<Option<Arc<Mutex<Store>>>, CommandError> {
        if !enabled {
            return match self.local_store() {
                Ok(store) => Ok(Some(store)),
                Err(e) if e.code == "disabled" => Ok(None),
                Err(e) => Err(e),
            };
        }
        if let Some(store) = self.store.get() {
            return Ok(Some(store.clone()));
        }
        let Ok(dir) = self.data_dir() else { return Ok(None) };
        if !self.peek_native(dir)? {
            // Not native yet: the boot opens (and migrates) it.
            return Ok(None);
        }
        self.open_store().map(Some).map_err(|_| CommandError::store_error("open"))
    }

    fn active_vault(&self, store: &Mutex<Store>) -> Result<String, CommandError> {
        lock(store).active_vault()?.ok_or_else(CommandError::not_joined)
    }

    fn connection(&self, store: &Mutex<Store>) -> Result<ConnectMeta, CommandError> {
        load_connection(&lock(store))?.ok_or_else(CommandError::not_connected)
    }

    fn name_key(&self, vault: &str) -> Result<SecretKey, CommandError> {
        let dek = load_vault_key(&self.secrets, vault)?.ok_or_else(|| CommandError::from(VaultError::NoKey))?;
        Ok(derive_subkey(&dek, vault, INITIAL_KEY_EPOCH, Purpose::BlobName))
    }

    fn settings(&self) -> SyncSettings {
        load_settings(self.dir.as_deref())
    }

    fn write_settings(&self, settings: SyncSettings) -> Result<(), CommandError> {
        save_settings(self.data_dir()?, settings).map_err(|e| {
            log::warn!("sync settings not saved: {e}");
            CommandError::new("io", "settings could not be saved")
        })
    }

    // -- boot ------------------------------------------------------------

    /// After the clipboard load (whose outcome is recorded either way). With
    /// the flag off this returns at once: no store, no provider, no sink.
    pub(crate) fn boot(self: &Arc<Self>, host: &Arc<dyn Host>, load: LoadOutcome) -> Vec<BootStep> {
        *lock(&self.load_outcome) = Some(load);
        if boot_plan(self.enabled()).is_empty() {
            log_event("boot", &[("enabled", Field::Bool(false))]);
            return Vec::new();
        }
        self.activate(host)
    }

    /// Steps 1–5 of the flag-on boot (also run when sync is turned on).
    /// A failed step stops the rest; the error is logged content-free.
    fn activate(self: &Arc<Self>, host: &Arc<dyn Host>) -> Vec<BootStep> {
        let mut done = Vec::new();
        for &step in boot_plan(true) {
            let result = match step {
                BootStep::OpenStore => self.open_store().map(|_| ()),
                BootStep::ResumeMigration => self.resume_migration().map(|_| ()),
                BootStep::NativeCutover => self.boot_cutover(host).map(|_| ()),
                BootStep::StartScheduler => {
                    self.start_scheduler(host);
                    Ok(())
                }
            };
            if let Err(e) = result {
                log_event("boot_step_failed", &[("code", Field::Label(e.code))]);
                break;
            }
            done.push(step);
        }
        done
    }

    /// Migration resume (G4 amendments): M4/M5 from the backup files when
    /// they are pending, then the finalize decision for this boot. Never
    /// mirrors before `native`.
    fn resume_migration(&self) -> Result<MigrationState, CommandError> {
        let store = self.open_store()?;
        let Some(vault) = lock(&store).active_vault()? else {
            *lock(&self.finalize_input) = FinalizeInput::NotThisBoot;
            return Ok(MigrationState::load(&lock(&store))?);
        };
        let dir = self.migration_dir()?;
        let state = MigrationState::load(&lock(&store))?;
        let state = match state {
            MigrationState::BackedUp | MigrationState::Importing | MigrationState::Imported => {
                self.import_and_verify(&store, &dir, &vault)?
            }
            other => other,
        };
        let input = if state == MigrationState::Verified {
            finalize_input(lock(&self.load_outcome).as_ref(), clipboard_recorded(&dir))
        } else {
            FinalizeInput::NotThisBoot
        };
        *lock(&self.finalize_input) = input;
        Ok(state)
    }

    fn import_and_verify(&self, store: &Mutex<Store>, dir: &MigrationDir, vault: &str) -> Result<MigrationState, CommandError> {
        let name_key = self.name_key(vault)?;
        let mut store = lock(store);
        {
            let mut clock = lock(&self.clock);
            migration::import(&mut store, dir, vault, &name_key, &mut clock, now_ms())?;
        }
        if let migration::Verify::Failed(bad) = migration::verify(&mut store, dir, vault)? {
            log_event("migration_verify_failed", &[("mismatches", Field::Num(bad.len() as u64))]);
        }
        Ok(MigrationState::load(&store)?)
    }

    /// Step 4: when `native`, mirror + save before the sink, under the
    /// clipboard lock; reconciles first when memos ran local-only.
    fn boot_cutover(&self, host: &Arc<dyn Host>) -> Result<bool, CommandError> {
        let settings = self.settings();
        let installed = self.native_cutover(host, settings.local_only)?;
        if installed && settings.local_only {
            self.write_settings(SyncSettings { local_only: false, ..settings })?;
        }
        Ok(installed)
    }

    fn make_sink(&self, store: Arc<Mutex<Store>>, vault: String) -> Arc<dyn MemoSink> {
        Arc::new(NotifyingSink {
            inner: StoreMemoSink { store, vault, clock: self.clock.clone() },
            events: self.events.clone(),
        })
    }

    /// The native cutover: nothing unless `native` with an active vault. The
    /// migration state and the vault are read under the clipboard lock (A14),
    /// so nothing can change them between the check and the install.
    fn native_cutover(&self, host: &Arc<dyn Host>, reconcile: bool) -> Result<bool, CommandError> {
        let store = self.open_store()?;
        let mut cut = None;
        let installed = clipboard::cutover_in(
            host.clipboard(),
            |entries| {
                let vault = {
                    let guard = lock(&store);
                    if MigrationState::load(&guard)? != MigrationState::Native {
                        return Ok(None);
                    }
                    match guard.active_vault()? {
                        Some(vault) => vault,
                        None => return Ok(None),
                    }
                };
                let prepared = self.cutover_prepare(&store, &vault, entries, reconcile)?;
                cut = Some(vault);
                Ok::<_, CommandError>(Some(prepared))
            },
            |entries, max| host.save_clipboard(entries, max),
        )?;
        if let (true, Some(vault)) = (installed, cut) {
            host.clipboard_changed();
            log_event("cutover", &[("vault", Field::Vault(VaultTag::of(&vault))), ("reconciled", Field::Bool(reconcile))]);
        }
        Ok(installed)
    }

    /// Inside the clipboard lock: optionally reconcile the live memos into
    /// `vault` (sync was off), then mirror its projection; returns the new
    /// entries and the sink bound to `vault`.
    fn cutover_prepare(
        &self,
        store: &Arc<Mutex<Store>>,
        vault: &str,
        entries: &[ClipEntry],
        reconcile: bool,
    ) -> Result<(Vec<ClipEntry>, Arc<dyn MemoSink>), CommandError> {
        let sink = self.make_sink(store.clone(), vault.to_owned());
        let mut entries = entries.to_vec();
        if reconcile {
            for entry in entries.iter_mut().filter(|e| e.kind == EntryKind::Memo) {
                if let Some(id) = sink.changed(None, Some(entry)).map_err(|_| CommandError::new("store", "memo reconcile failed"))? {
                    entry.id = id;
                }
            }
        }
        let projection = lock(store).projection(vault)?;
        Ok((memo_bridge::mirror(&entries, &projection, now_ms() as i64), sink))
    }

    /// Remote apply → mirror (after a cycle that changed memos). The sink
    /// slot is held throughout (slot → state → store, like a memo command),
    /// so a disable or a vault switch cannot land between the check and the
    /// mirror.
    fn mirror_now(&self, host: &dyn Host) -> Result<bool, CommandError> {
        let slot = lock(&host.clipboard().memo_sink);
        if slot.is_none() {
            return Ok(false);
        }
        let store = self.open_store()?;
        let vault = self.active_vault(&store)?;
        let changed = clipboard::mirror_in(
            host.clipboard(),
            |entries| {
                let projection = lock(&store).projection(&vault)?;
                Ok::<_, CommandError>(memo_bridge::mirror(entries, &projection, now_ms() as i64))
            },
            |entries, max| host.save_clipboard(entries, max),
        )?;
        drop(slot);
        if changed {
            host.clipboard_changed();
        }
        Ok(changed)
    }

    // -- scheduler -------------------------------------------------------

    /// Whether sync may run: a connection, an active vault, and that vault
    /// created or joined under this connection's account (A8).
    fn has_connection_and_vault(&self) -> bool {
        let Some(store) = self.store.get() else { return false };
        let store = lock(store);
        matches!(store.active_vault(), Ok(Some(_))) && vault_bound(&store)
    }

    fn start_scheduler(self: &Arc<Self>, host: &Arc<dyn Host>) {
        if !self.enabled() || !self.has_connection_and_vault() {
            return;
        }
        let mut slot = lock(&self.scheduler);
        if slot.is_some() {
            return;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *lock(&self.events) = Some(tx.clone());
        let runtime = Arc::clone(self);
        let task_host = Arc::clone(host);
        let task = host.spawn(Box::pin(async move {
            let cycle = || {
                let runtime = Arc::clone(&runtime);
                let host = Arc::clone(&task_host);
                async move { runtime.cycle_once(host.as_ref()).await }
            };
            engine::run_scheduler(rx, cycle, engine::os_jitter).await;
        }));
        *slot = Some(SchedulerHandle { tx, task });
    }

    async fn stop_scheduler(&self) {
        *lock(&self.events) = None;
        let handle = lock(&self.scheduler).take();
        if let Some(handle) = handle {
            let _ = handle.tx.send(SchedEvent::Cancel);
            if let Some(task) = handle.task {
                if task.await.is_err() {
                    log_event("scheduler_join_failed", &[]);
                }
            }
        }
    }

    fn resume(self: &Arc<Self>, host: &Arc<dyn Host>) {
        let sent = lock(&self.scheduler).as_ref().is_some_and(|h| h.tx.send(SchedEvent::Resume).is_ok());
        if !sent {
            self.start_scheduler(host);
        }
    }

    async fn active_provider(&self) -> Result<Arc<ActiveProvider>, CommandError> {
        self.restore().await.map_err(|e| match e {
            Early::Provider(p) => p.into(),
            Early::Command(c) => c,
        })
    }

    /// The cached adapter, or one rebuilt from the stored connection; the
    /// provider error is kept whole for the cycle's status (D10).
    async fn restore(&self) -> Result<Arc<ActiveProvider>, Early> {
        if let Some(provider) = lock(&self.provider).clone() {
            return Ok(provider);
        }
        let store = self.store().map_err(Early::Command)?;
        let meta = self.connection(&store).map_err(Early::Command)?;
        let provider = Arc::new(restore_provider(&meta, &self.secrets).await.map_err(Early::Provider)?);
        *lock(&self.provider) = Some(provider.clone());
        Ok(provider)
    }

    async fn cycle_once(&self, host: &dyn Host) -> CycleOutcome {
        if !self.enabled() {
            return CycleOutcome::Stop;
        }
        let Some(store) = self.store.get().cloned() else { return CycleOutcome::Stop };
        // The fence value and the vault are read together: a switch bumps the
        // generation while it holds the store lock.
        let (gen, vault) = {
            let guard = lock(&store);
            let gen = self.vault_gen.load(Ordering::SeqCst);
            match guard.active_vault() {
                Ok(Some(vault)) if vault_bound(&guard) => (gen, vault),
                Ok(_) => {
                    drop(guard);
                    return self.fail_early(gen, Early::Command(CommandError::not_joined()));
                }
                Err(e) => {
                    drop(guard);
                    return self.fail_early(gen, Early::Command(e.into()));
                }
            }
        };
        let provider = match self.restore().await {
            Ok(provider) => provider,
            Err(e) => return self.fail_early(gen, e),
        };
        let report = with_provider!(provider.as_ref(), p => self.cycle_with(p, &store, &vault, gen).await);
        let Some(report) = report else {
            log_event("cycle_superseded", &[]);
            return CycleOutcome::Stop;
        };
        if !self.record(&report, gen) {
            return CycleOutcome::Stop;
        }
        if report.changed {
            if let Err(e) = self.mirror_now(host) {
                log_event("mirror_failed", &[("code", Field::Label(e.code))]);
            }
            self.notify_chat(host);
        }
        let restored_current = lock(&self.restored).as_ref().is_some_and(|c| c.gen == gen && c.vault == vault);
        if report.changed || !restored_current {
            self.refresh_restored(&store, &vault, gen);
        }
        if report.status == CycleStatus::Ok {
            with_provider!(provider.as_ref(), p => self.refresh_devices(p, &vault, gen).await);
        }
        CycleOutcome::of(&report)
    }

    /// G9 U5: the restored-after-delete digests of `vault` from its
    /// projection; dismissed digests no longer restored are pruned (and the
    /// pruned set persisted). Lock order: restored → store.
    fn refresh_restored(&self, store: &Mutex<Store>, vault: &str, gen: u64) {
        let mut slot = lock(&self.restored);
        let mut guard = lock(store);
        let current = match guard.projection(vault) {
            Ok(projection) => engine::restored_after_delete(&projection.notices),
            Err(_) => {
                log_event("restored_refresh_failed", &[]);
                return;
            }
        };
        let live: BTreeSet<[u8; 32]> = current.iter().map(|(_, digest)| *digest).collect();
        let mut dismissed = load_dismissed(&guard, vault);
        let before = dismissed.len();
        dismissed.retain(|digest| live.contains(digest));
        if dismissed.len() != before && save_dismissed(&mut guard, vault, &dismissed).is_err() {
            log_event("restored_dismissed_save_failed", &[]);
        }
        drop(guard);
        if self.vault_gen.load(Ordering::SeqCst) == gen {
            *slot = Some(RestoredCache { gen, vault: vault.to_owned(), current, dismissed });
        }
    }

    /// G9 U4: re-reads the vault's device records when due (throttled; a
    /// switch, disconnect or removal forces it). No lock is held across the
    /// reads, which go through the bound provider. A failure keeps the last
    /// cache and is only logged (no notice, no cycle failure).
    async fn refresh_devices<P: ObjectStore>(&self, provider: &Bound<P>, vault: &str, gen: u64) {
        let now = now_ms();
        let due = DevicesCache::due(lock(&self.devices).as_ref(), vault, gen, now);
        if !due {
            return;
        }
        let dek = match load_vault_key(&self.secrets, vault) {
            Ok(Some(dek)) => dek,
            _ => {
                log_event("devices_refresh_failed", &[("code", Field::Label("vault_key"))]);
                return;
            }
        };
        match engine::read_devices(provider, &dek, vault, INITIAL_KEY_EPOCH).await {
            Ok(devices) => {
                let mut slot = lock(&self.devices);
                if self.vault_gen.load(Ordering::SeqCst) == gen {
                    let next = DevicesCache::next(slot.as_ref(), vault, gen, now, devices);
                    *slot = Some(next);
                }
            }
            Err(err) => {
                log_event("devices_refresh_failed", &[("code", Field::Label(engine::classify(&err).as_str()))]);
            }
        }
    }

    /// `sync:chat-changed` when the native projection's conversations or
    /// translations differ from what was last loaded or signalled (G8).
    /// Before `native` the webview reads localStorage: nothing to signal.
    fn notify_chat(&self, host: &dyn Host) {
        let Some(store) = self.store.get() else { return };
        let fp = {
            let guard = lock(store);
            if MigrationState::load(&guard).ok() != Some(MigrationState::Native) {
                return;
            }
            let Ok(Some(vault)) = guard.active_vault() else { return };
            match guard.projection(&vault) {
                Ok(p) => chat_fingerprint(&p),
                Err(_) => {
                    log_event("chat_notify_failed", &[]);
                    return;
                }
            }
        };
        let mut last = lock(&self.chat_fp);
        if *last != Some(fp) {
            *last = Some(fp);
            drop(last);
            host.chat_changed();
        }
    }

    /// A cycle that could not reach the engine (no adapter, no credential,
    /// no vault, a store fault) still reports: a status, the matching notice,
    /// and a failure count (D10).
    fn fail_early(&self, gen: u64, early: Early) -> CycleOutcome {
        let (status, notice, class) = early_status(&early);
        let code = match &early {
            Early::Provider(p) => CommandError::from(p.clone()).code,
            Early::Command(c) => c.code,
        };
        log_event("cycle_not_started", &[("code", Field::Label(code))]);
        if let Some(class) = class {
            self.stats.failed(class);
        }
        let report = CycleReport {
            status,
            notices: notice.into_iter().collect(),
            applied_segments: 0,
            uploaded_segments: 0,
            changed: false,
        };
        self.record(&report, gen);
        CycleOutcome::of(&report)
    }

    /// One engine cycle under the vault fence (D6). `None` when a vault
    /// switch happened meanwhile: the report is dropped and the engine state
    /// is not put back (the switch installed a fresh one).
    async fn cycle_with<P: ObjectStore>(
        &self,
        provider: &Bound<P>,
        store: &Mutex<Store>,
        vault: &str,
        gen: u64,
    ) -> Option<CycleReport> {
        let checkpoint_dir = self.dir.as_ref().map(|d| d.join(SYNC_DIR).join(CHECKPOINT_DIR));
        let install_fp = (self.install_fp)();
        let fence = || self.vault_gen.load(Ordering::SeqCst) == gen;
        let mut engine = std::mem::take(&mut *lock(&self.engine));
        let report = {
            let mut ctx = CycleCtx {
                provider,
                secrets: &self.secrets,
                store,
                clock: &self.clock,
                pause: &TokioPause,
                now: &now_ms,
                stats: &self.stats,
                engine: &mut engine,
                vault,
                key_epoch: INITIAL_KEY_EPOCH,
                install_fp: install_fp.as_deref(),
                device_label: DEVICE_LABEL,
                app_version: env!("CARGO_PKG_VERSION"),
                checkpoint_dir: checkpoint_dir.as_deref(),
                fence: Some(&fence),
            };
            engine::Engine::cycle(&mut ctx).await
        };
        let mut slot = lock(&self.engine);
        if !fence() {
            return None;
        }
        *slot = engine;
        Some(report)
    }

    /// Records a cycle unless a vault switch superseded it. The check runs
    /// under the notices lock, which the switch also takes to clear them.
    fn record(&self, report: &CycleReport, gen: u64) -> bool {
        let mut notices = lock(&self.notices);
        if self.vault_gen.load(Ordering::SeqCst) != gen {
            return false;
        }
        *lock(&self.last_status) = Some(report.status);
        *lock(&self.last_cycle_at) = Some(now_ms());
        // D3a: a fresh manifest-copy count replaces the older one.
        if report.notices.iter().any(|n| matches!(n, EngineNotice::ManifestCopies { .. })) {
            notices.retain(|n| !matches!(n, EngineNotice::ManifestCopies { .. }));
        }
        notices.extend(report.notices.iter().cloned());
        true
    }

    // -- commands --------------------------------------------------------

    pub(crate) fn status(&self) -> SyncStatus {
        let enabled = self.enabled();
        let mut status = SyncStatus {
            enabled,
            provider: None,
            vault: None,
            last_status: *lock(&self.last_status),
            last_cycle_at: *lock(&self.last_cycle_at),
            running: lock(&self.scheduler).is_some(),
            stats: self.stats.snapshot(),
            notices: lock(&self.notices).iter().cloned().collect(),
            discovery: "none",
            discovery_retry_at: None,
            migration: "none",
            migration_state: "none",
            queue_flushed_qseq: None,
            ack_qseq: None,
            chat_src_fallbacks: self.chat_src_fallbacks.load(Ordering::SeqCst),
            wipe: "none",
            reissue_pending: lock(&self.pending_reissue).is_some(),
            create_pending: lock(&self.pending_create).is_some(),
            join_preview: lock(&self.pending_join).as_ref().map(|(_, p)| p.summary().into()),
            finalize_this_boot: *lock(&self.finalize_input) != FinalizeInput::NotThisBoot,
            needs_vault: false,
            store_error: false,
            devices: Vec::new(),
            restored: Vec::new(),
        };
        // Never creates the store. With the flag on, a store not yet opened
        // reports defaults unless its file is already native; with the flag
        // off, only a native device reports its store (E1: its migration
        // state and queue watermarks). A store that cannot be read is
        // `store_error` (E1a), never the defaults read as "not native".
        let store = match self.status_store(enabled) {
            Ok(store) => store,
            Err(_) => {
                status.store_error = true;
                None
            }
        };
        // G9 U4/U5 read the active vault and this device under the store
        // lock, and their caches only after it is released.
        let mut active = None;
        if let Some(store) = store.as_deref() {
            let store = lock(store);
            status.provider = load_connection(&store)
                .ok()
                .flatten()
                .map(|m| ConnectedInfo { provider: m.kind(), account_label: m.account_label() });
            let vault_id = store.active_vault().ok().flatten();
            status.vault = vault_id.as_deref().map(VaultTag::of);
            status.needs_vault = status.provider.is_some() && (status.vault.is_none() || !vault_bound(&store));
            let states = store_states(&store);
            status.discovery = states.discovery;
            status.discovery_retry_at = vault::discovery_retry_at(&store).ok().flatten();
            status.migration = states.migration;
            status.migration_state = states.migration;
            // An unreadable migration state (`unknown`) is a store error too.
            status.store_error = MigrationState::load(&store).is_err();
            status.queue_flushed_qseq = migration::flushed_qseq(&store).ok().flatten();
            status.ack_qseq = migration::ack_qseq(&store).ok().flatten();
            status.wipe = states.wipe;
            if vault::reissue_pending(&store).ok().flatten().is_some() {
                status.reissue_pending = true;
            }
            if let Some(vault_id) = vault_id.filter(|_| !status.needs_vault) {
                let own = store.local_identity(&vault_id).ok().flatten().map(|l| l.device_id);
                active = Some((vault_id, own));
            }
        }
        if let Some((vault_id, own)) = active {
            let gen = self.vault_gen.load(Ordering::SeqCst);
            if let Some(own) = own {
                let self_removed =
                    status.last_status == Some(CycleStatus::Stopped { reason: StopReason::DeviceRemoved });
                status.devices = device_list(lock(&self.devices).as_ref(), &vault_id, gen, &own, self_removed);
            }
            if let Some(cache) = lock(&self.restored).as_ref().filter(|c| c.gen == gen && c.vault == vault_id) {
                status.restored = cache.counts();
            }
        }
        status
    }

    pub(crate) async fn set_enabled(self: &Arc<Self>, host: &Arc<dyn Host>, enabled: bool) -> Result<SyncStatus, CommandError> {
        let settings = self.settings();
        if enabled {
            self.write_settings(SyncSettings { enabled: true, ..settings })?;
            self.flag.store(true, Ordering::SeqCst);
            let done = self.activate(host);
            if !done.contains(&BootStep::NativeCutover) {
                // Fail closed: back to off with the previous markers.
                self.flag.store(false, Ordering::SeqCst);
                self.write_settings(SyncSettings { enabled: false, ..settings })?;
                return Err(CommandError::new("activation", "sync could not start"));
            }
        } else {
            // `local_only` first, in the same atomic write as the flag.
            self.write_settings(SyncSettings { enabled: false, local_only: true })?;
            self.flag.store(false, Ordering::SeqCst);
            self.stop_scheduler().await;
            clipboard::uninstall_sink(host.clipboard());
            *lock(&self.provider) = None;
        }
        Ok(self.status())
    }

    fn ensure_no_other_account(&self, kind: ProviderKind) -> Result<Option<ConnectMeta>, CommandError> {
        let store = self.store()?;
        let existing = load_connection(&lock(&store))?;
        if existing.as_ref().is_some_and(|m| m.kind() != kind) {
            return Err(CommandError::new("already_connected", "disconnect the current account first"));
        }
        Ok(existing)
    }

    pub(crate) async fn connect_oauth(
        self: &Arc<Self>,
        host: &Arc<dyn Host>,
        oauth_state: &OAuthCallbackState,
        open_url: &(dyn Fn(String) -> Result<(), String> + Send + Sync),
        kind: ProviderKind,
    ) -> Result<ConnectedInfo, CommandError> {
        self.require_enabled()?;
        let existing = self.ensure_no_other_account(kind)?;
        let cfg = oauth_client(kind)?;
        let pkce = oauth::Pkce::generate()?;
        let state = oauth::new_state()?;
        let (port, wait) = start_native_listener(oauth_state, 0, oauth::CALLBACK_PATH, state.clone(), OAUTH_TIMEOUT)?;
        let redirect = oauth::redirect_uri(port);
        if open_url(oauth::auth_url(&cfg, &pkce.challenge, &state, &redirect)).is_err() {
            cancel_native(oauth_state);
            return Err(CommandError::new("browser", "could not open the sign-in page"));
        }
        let outcome = wait.wait().await?;
        if outcome.error.is_some() {
            return Err(CommandError::new("oauth_denied", "the provider refused the sign-in"));
        }
        let code = Zeroizing::new(outcome.code.ok_or_else(|| CommandError::new("oauth", "no authorization code"))?);
        let (tokens, identity) = OAuthTokens::connect(KeyringStore, transport()?, cfg, &code, &pkce.verifier, &redirect).await?;
        let meta = parse_oauth_identity(&identity)?;
        if tokens.binding_fp() != meta.binding_fp {
            return Err(ProviderError::BindingMismatch.into());
        }
        if existing.as_ref().is_some_and(|m| m.binding_fp != meta.binding_fp) {
            if delete_secret(&KeyringStore, meta.secret()).is_err() {
                log_event("credential_cleanup_failed", &[]);
            }
            return Err(CommandError::new("already_connected", "disconnect the current account first"));
        }
        let provider = match &meta.params {
            ConnectParams::Gdrive { sub } => ActiveProvider::Drive(Bound::new(
                GDriveStore::new(transport()?, tokens, sub.clone())?,
                meta.binding_fp.clone(),
            )),
            ConnectParams::Onedrive { tenant_id, object_id } => ActiveProvider::OneDrive(Bound::new(
                OneDriveStore::new(transport()?, tokens, tenant_id, object_id)?,
                meta.binding_fp.clone(),
            )),
            _ => return Err(CommandError::new("internal", "unexpected identity")),
        };
        with_provider!(&provider, p => connect_check(p).await)?;
        self.adopt(host, meta, provider)
    }

    pub(crate) async fn connect_keyed(
        self: &Arc<Self>,
        host: &Arc<dyn Host>,
        kind: ProviderKind,
        params: serde_json::Value,
    ) -> Result<ConnectedInfo, CommandError> {
        self.require_enabled()?;
        let existing = self.ensure_no_other_account(kind)?;
        let bad = || CommandError::new("invalid_params", "connection parameters are incomplete");
        let (meta, provider) = match kind {
            ProviderKind::Webdav => {
                let params: WebDavParams = serde_json::from_value(params).map_err(|_| bad())?;
                let (store, meta) = connect_webdav(&self.secrets, params, transport()?).await?;
                let fp = meta.binding_fp.clone();
                (meta, ActiveProvider::WebDav(Bound::new(store, fp)))
            }
            ProviderKind::R2 => {
                let params: R2Params = serde_json::from_value(params).map_err(|_| bad())?;
                let (store, meta) = connect_r2(&self.secrets, params, transport()?).await?;
                let fp = meta.binding_fp.clone();
                (meta, ActiveProvider::R2(Bound::new(store, fp)))
            }
            ProviderKind::Gdrive | ProviderKind::Onedrive => {
                return Err(CommandError::new("unsupported", "this provider signs in with OAuth"));
            }
        };
        if existing.as_ref().is_some_and(|m| m.binding_fp != meta.binding_fp) {
            if delete_secret(&self.secrets, meta.secret()).is_err() {
                log_event("credential_cleanup_failed", &[]);
            }
            return Err(CommandError::new("already_connected", "disconnect the current account first"));
        }
        self.adopt(host, meta, provider)
    }

    /// Stores the connection and resumes sync, unless the active vault was
    /// created or joined under another account: then sync stays stopped and
    /// the status reports `needs_vault` (discover, then create or join; A8).
    fn adopt(self: &Arc<Self>, host: &Arc<dyn Host>, meta: ConnectMeta, provider: ActiveProvider) -> Result<ConnectedInfo, CommandError> {
        let store = self.store()?;
        let needs_vault = {
            let mut guard = lock(&store);
            save_connection(&mut guard, Some(&meta))?;
            !vault_bound(&guard)
        };
        *lock(&self.provider) = Some(Arc::new(provider));
        lock(&self.notices).retain(|n| !matches!(n, EngineNotice::ReconsentRequired | EngineNotice::BindingMismatch));
        if needs_vault {
            lock(&self.notices).insert(EngineNotice::NotJoined);
        } else {
            self.resume(host);
        }
        log_event("connected", &[("provider", Field::Label(meta.kind().label())), ("needs_vault", Field::Bool(needs_vault))]);
        Ok(ConnectedInfo { provider: meta.kind(), account_label: meta.account_label() })
    }

    /// A plain disconnect is refused while a wipe is started (ADV-6): the
    /// wipe must be finished first, or the remote stays half-wiped with no
    /// way back to it.
    pub(crate) async fn disconnect(&self, wipe_cloud: bool, confirm: &str) -> Result<(), CommandError> {
        self.require_enabled()?;
        check_wipe_confirmation(wipe_cloud, confirm)?;
        let store = self.store()?;
        let meta = self.connection(&store)?;
        if wipe_cloud {
            let vault = self.active_vault(&store)?;
            let provider = self.active_provider().await?;
            vault::begin_wipe(&mut lock(&store), &vault, confirm)?;
            self.stop_scheduler().await;
            with_provider!(provider.as_ref(), p => vault::run_wipe(p, &TokioPause, &vault).await)?;
            vault::finish_wipe(&mut lock(&store), &self.secrets, &vault)?;
        } else {
            if store_states(&lock(&store)).wipe == "started" {
                return Err(CommandError::new("wipe_in_progress", "finish the started cloud wipe first"));
            }
            self.stop_scheduler().await;
        }
        save_connection(&mut lock(&store), None)?;
        *lock(&self.provider) = None;
        *lock(&self.devices) = None;
        delete_secret(&self.secrets, meta.secret())?;
        log_event("disconnected", &[("wiped", Field::Bool(wipe_cloud))]);
        Ok(())
    }

    pub(crate) async fn discover(&self) -> Result<DiscoverySummary, CommandError> {
        let store = self.store()?;
        let provider = self.active_provider().await?;
        let result = with_provider!(provider.as_ref(), p => vault::discover(p, &TokioPause, now_ms).await);
        vault::record_discovery(&mut lock(&store), &result, now_ms())?;
        let (state, ids) = match &result {
            Discovery::None => ("none", Vec::new()),
            Discovery::Found(ids) => ("found", ids.clone()),
            Discovery::Retry { seen, .. } => ("retry", seen.clone()),
        };
        let count = ids.len();
        *lock(&self.discovered) = ids;
        Ok(DiscoverySummary { state, vaults: count })
    }

    pub(crate) fn create_vault(&self) -> Result<CreatedVault, CommandError> {
        let store = self.store()?;
        let meta = self.connection(&store)?;
        let pending = vault::begin_create(&lock(&store), &meta.binding_fp, now_ms())?;
        let recovery_key = pending.recovery_key().to_display().to_string();
        let vault = VaultTag::of(pending.vault_id());
        *lock(&self.pending_create) = Some(pending);
        *lock(&self.pending_reissue) = None;
        Ok(CreatedVault { vault, recovery_key })
    }

    /// Re-entry for whichever RK dialog is open (create or reissue). A wrong
    /// RK changes nothing and keeps the dialog's state for another try. A
    /// create publishes the manifest first, then activates the vault inside
    /// the vault-switch critical section (D6).
    pub(crate) async fn confirm_rk(self: &Arc<Self>, host: &Arc<dyn Host>, rk: &str) -> Result<ConfirmResult, CommandError> {
        let store = self.store()?;
        let created = lock(&self.pending_create).take();
        if let Some(mut pending) = created {
            let restore = |pending: PendingVault| *lock(&self.pending_create) = Some(pending);
            let provider = match self.active_provider().await {
                Ok(provider) => provider,
                Err(e) => {
                    restore(pending);
                    return Err(e);
                }
            };
            let binding = match self.connection(&store) {
                Ok(meta) => meta.binding_fp,
                Err(e) => {
                    restore(pending);
                    return Err(e);
                }
            };
            let published = with_provider!(provider.as_ref(), p => publish_create(p, &self.secrets, &mut pending, rk).await);
            if let Err(e) = published {
                restore(pending);
                return Err(e);
            }
            let install_fp = (self.install_fp)();
            let switched = self
                .switch_vault(host, &binding, |store, clock| {
                    vault::activate_created(store, &self.secrets, &pending, install_fp, clock, now_ms())
                })
                .await;
            return match switched {
                Ok(Ok(commit)) => Ok(ConfirmResult::Created { vault: VaultTag::of(&commit.vault_id) }),
                Ok(Err(e)) => {
                    restore(pending);
                    Err(e.into())
                }
                // Committed, then the cutover failed: nothing to retry.
                Err(e) => Err(e),
            };
        }
        let reissue = lock(&self.pending_reissue).take();
        if let Some(pending) = reissue {
            if let Err(e) = vault::confirm_reissue(&mut lock(&store), &pending, rk) {
                *lock(&self.pending_reissue) = Some(pending);
                return Err(e.into());
            }
            let status = self.publish_pending_reissue().await?;
            return Ok(ConfirmResult::Reissued { not_revocation: status.not_revocation });
        }
        Err(VaultError::NothingPending.into())
    }

    async fn publish_pending_reissue(&self) -> Result<vault::ReissueStatus, CommandError> {
        let store = self.store()?;
        let meta = self.connection(&store)?;
        let Some((vault_id, bytes)) = vault::reissue_pending(&lock(&store))? else {
            return Err(VaultError::NothingPending.into());
        };
        let provider = self.active_provider().await?;
        let published = with_provider!(provider.as_ref(), p => {
            vault::publish_reissue(p, &self.secrets, &vault_id, &bytes, &meta.binding_fp).await
        });
        let status = match published {
            Ok(status) => status,
            // Another device reissued first: this RK is never confirmed (D3).
            Err(VaultError::ReissueSuperseded) => {
                vault::abandon_reissue(&mut lock(&store))?;
                return Err(VaultError::ReissueSuperseded.into());
            }
            Err(e) => return Err(e.into()),
        };
        vault::finish_reissue(&mut lock(&store))?;
        Ok(status)
    }

    pub(crate) async fn reissue_rk(&self) -> Result<ReissueResult, CommandError> {
        let store = self.store()?;
        if vault::reissue_pending(&lock(&store))?.is_some() {
            let status = self.publish_pending_reissue().await?;
            return Ok(ReissueResult::Published { not_revocation: status.not_revocation });
        }
        let meta = self.connection(&store)?;
        let vault_id = self.active_vault(&store)?;
        let provider = self.active_provider().await?;
        let pending = with_provider!(provider.as_ref(), p => {
            vault::begin_reissue(p, &self.secrets, &vault_id, &meta.binding_fp).await
        })?;
        let recovery_key = pending.recovery_key().to_display().to_string();
        *lock(&self.pending_reissue) = Some(pending);
        *lock(&self.pending_create) = None;
        Ok(ReissueResult::ShowKey { recovery_key })
    }

    pub(crate) async fn join_preview(&self, rk: &str) -> Result<JoinPreviewOut, CommandError> {
        let store = self.store()?;
        let meta = self.connection(&store)?;
        let ids = lock(&self.discovered).clone();
        if ids.is_empty() {
            return Err(VaultError::DiscoveryRequired.into());
        }
        let provider = self.active_provider().await?;
        let candidates = with_provider!(provider.as_ref(), p => vault::fetch_candidates(p, &ids).await)?;
        let unlocked = vault::unlock_join(&lock(&store), &self.secrets, &candidates, rk, &meta.binding_fp)?;
        let download = with_provider!(provider.as_ref(), p => vault::fetch_target(p, &unlocked.vault_id).await)?;
        let preview = {
            let mut guard = lock(&store);
            vault::apply_target(&mut guard, &self.secrets, &unlocked, &download)?;
            vault::preview_join(&guard, &self.secrets, &unlocked)?
        };
        let out = JoinPreviewOut { vault: VaultTag::of(&unlocked.vault_id), summary: preview.summary().into() };
        *lock(&self.pending_join) = Some((unlocked, preview));
        Ok(out)
    }

    pub(crate) async fn join_commit(self: &Arc<Self>, host: &Arc<dyn Host>) -> Result<JoinCommitOut, CommandError> {
        let store = self.store()?;
        let binding = self.connection(&store)?.binding_fp;
        let Some((unlocked, preview)) = lock(&self.pending_join).take() else {
            return Err(VaultError::NothingPending.into());
        };
        let install_fp = (self.install_fp)();
        let switched = self
            .switch_vault(host, &binding, |store, clock| {
                vault::commit_join(store, &self.secrets, &unlocked, &preview, install_fp, clock, now_ms())
            })
            .await;
        match switched {
            Ok(Ok(commit)) => Ok(JoinCommitOut::Joined { vault: VaultTag::of(&commit.vault_id), ops: commit.ops }),
            Ok(Err(VaultError::PreviewStale(fresh))) => {
                let summary = fresh.summary().into();
                *lock(&self.pending_join) = Some((unlocked, fresh));
                Ok(JoinCommitOut::PreviewChanged { summary })
            }
            Ok(Err(e)) => {
                *lock(&self.pending_join) = Some((unlocked, preview));
                Err(e.into())
            }
            Err(e) => Err(e),
        }
    }

    /// D1 recovery for a removed device: the RK must open the active vault
    /// (and match the DEK this device holds); inside the vault-switch critical
    /// section this device then takes a fresh identity in the same vault and
    /// its unpublished ops go out under the new stream. Published ops stay.
    pub(crate) async fn rejoin(self: &Arc<Self>, host: &Arc<dyn Host>, rk: &str) -> Result<RejoinOut, CommandError> {
        let store = self.store()?;
        let meta = self.connection(&store)?;
        let vault_id = self.active_vault(&store)?;
        let provider = self.active_provider().await?;
        let ids = [vault_id];
        let candidates = with_provider!(provider.as_ref(), p => vault::fetch_candidates(p, &ids).await)?;
        let install_fp = (self.install_fp)();
        let commit = self
            .switch_vault(host, &meta.binding_fp, |store, _clock| {
                vault::rejoin(store, &self.secrets, &candidates, rk, &meta.binding_fp, install_fp)
            })
            .await??;
        log_event("rejoined", &[("vault", Field::Vault(VaultTag::of(&commit.vault_id)))]);
        Ok(RejoinOut { vault: VaultTag::of(&commit.vault_id) })
    }

    /// The vault-switch critical section (D6) around `commit` (J7 of a join,
    /// a create's activation, or a rejoin):
    /// 1. the generation is bumped and the scheduler stopped, so no cycle of
    ///    the old vault can upload, record, mirror, or re-install its engine;
    /// 2. under the sink slot and the clipboard lock (lock order slot → state
    ///    → store → clock), `commit` runs, the binding is recorded, and on a
    ///    native clipboard the new vault is mirrored and its sink installed;
    /// 3. sync restarts.
    ///
    /// The inner `Err` is a refused commit: nothing changed. The outer `Err`
    /// means the commit landed but the cutover failed; the old sink is then
    /// removed and memo commands are local-only until the next cutover
    /// reconciles them.
    async fn switch_vault(
        self: &Arc<Self>,
        host: &Arc<dyn Host>,
        binding: &str,
        commit: impl FnOnce(&mut Store, &mut HlcClock) -> Result<vault::JoinCommit, VaultError>,
    ) -> Result<Result<vault::JoinCommit, VaultError>, CommandError> {
        self.vault_gen.fetch_add(1, Ordering::SeqCst);
        self.stop_scheduler().await;
        let result = self.switch_locked(host, binding, commit);
        self.resume(host);
        result
    }

    /// Steps 2 of [`Self::switch_vault`], synchronous (the clipboard locks are
    /// std mutexes).
    fn switch_locked(
        &self,
        host: &Arc<dyn Host>,
        binding: &str,
        commit: impl FnOnce(&mut Store, &mut HlcClock) -> Result<vault::JoinCommit, VaultError>,
    ) -> Result<Result<vault::JoinCommit, VaultError>, CommandError> {
        let store = self.open_store()?;
        let mut committed = None;
        let mut refused = None;
        let cut = clipboard::cutover_in(
            host.clipboard(),
            |entries| {
                let (done, native) = {
                    let mut guard = lock(&store);
                    let done = {
                        let mut clock = lock(&self.clock);
                        commit(&mut guard, &mut clock)
                    };
                    let done = match done {
                        Ok(done) => done,
                        Err(e) => {
                            refused = Some(e);
                            return Ok(None);
                        }
                    };
                    // Any cycle that read the old vault is stale from here on.
                    self.vault_gen.fetch_add(1, Ordering::SeqCst);
                    let vault_id = done.vault_id.clone();
                    committed = Some(done);
                    guard.meta_set(META_VAULT_BINDING, binding)?;
                    let native = MigrationState::load(&guard)? == MigrationState::Native;
                    (vault_id, native)
                };
                if !native {
                    return Ok(None);
                }
                // The switch already republished the live memos: no reconcile.
                self.cutover_prepare(&store, &done, entries, false).map(Some)
            },
            |entries, max| host.save_clipboard(entries, max),
        );
        if let Some(e) = refused {
            return Ok(Err(e));
        }
        let Some(done) = committed else {
            return Err(cut.err().unwrap_or_else(|| CommandError::new("internal", "vault switch produced no result")));
        };
        {
            let mut notices = lock(&self.notices);
            *lock(&self.engine) = engine::Engine::default();
            notices.clear();
            *lock(&self.devices) = None;
            *lock(&self.restored) = None;
        }
        log_event("vault_switched", &[("vault", Field::Vault(VaultTag::of(&done.vault_id)))]);
        match cut {
            Ok(true) => host.clipboard_changed(),
            Ok(false) => {
                clipboard::uninstall_sink(host.clipboard());
            }
            Err(e) => {
                clipboard::uninstall_sink(host.clipboard());
                let settings = self.settings();
                if let Err(io) = self.write_settings(SyncSettings { local_only: true, ..settings }) {
                    log_event("switch_local_only_failed", &[("code", Field::Label(io.code))]);
                }
                log_event("switch_cutover_failed", &[("code", Field::Label(e.code))]);
                return Err(e);
            }
        }
        Ok(Ok(done))
    }

    pub(crate) async fn remove_device(&self, device_id: &str) -> Result<(), CommandError> {
        let store = self.store()?;
        let vault_id = self.active_vault(&store)?;
        let own = lock(&store).device_id(&vault_id)?;
        let provider = self.active_provider().await?;
        with_provider!(provider.as_ref(), p => vault::remove_device(p, &vault_id, &own, device_id).await)?;
        self.mark_device_removed(device_id);
        Ok(())
    }

    /// G9 U4: after a removal the device shows as removed at once, and the
    /// next cycle re-reads the records without waiting for the throttle.
    fn mark_device_removed(&self, device_id: &str) {
        if let Some(cache) = lock(&self.devices).as_mut() {
            cache.stale = true;
            if let Some(at) = cache.seen.remove(device_id) {
                cache.gone.insert(device_id.to_owned(), at);
            }
        }
    }

    pub(crate) fn dismiss_notice(&self, notice: &str) -> bool {
        if notice == RESTORED_NOTICE {
            return self.dismiss_restored();
        }
        let mut notices = lock(&self.notices);
        let before = notices.len();
        notices.retain(|n| notice_label(n) != notice);
        notices.len() != before
    }

    /// G9 U5: every restored entity shown now is dismissed (persisted per
    /// vault); only one restored later shows again. Lock order: restored →
    /// store.
    fn dismiss_restored(&self) -> bool {
        let mut slot = lock(&self.restored);
        let Some(cache) = slot.as_mut() else { return false };
        let fresh: Vec<[u8; 32]> =
            cache.current.iter().map(|(_, digest)| *digest).filter(|digest| !cache.dismissed.contains(digest)).collect();
        if fresh.is_empty() {
            return false;
        }
        cache.dismissed.extend(fresh);
        if let Some(store) = self.store.get() {
            if save_dismissed(&mut lock(store), &cache.vault, &cache.dismissed).is_err() {
                log_event("restored_dismissed_save_failed", &[]);
            }
        }
        true
    }

    pub(crate) fn export_diagnostics(&self) -> String {
        let mut states = vec![("enabled", if self.enabled() { "on" } else { "off" })];
        let mut vault_id = None;
        if let Some(store) = self.store.get() {
            let store = lock(store);
            let s = store_states(&store);
            states.push(("migration", s.migration));
            states.push(("discovery", s.discovery));
            states.push(("wipe", s.wipe));
            states.push(("provider", load_connection(&store).ok().flatten().map_or("none", |m| m.kind().label())));
            vault_id = store.active_vault().ok().flatten();
        }
        states.push(("scheduler", if lock(&self.scheduler).is_some() { "running" } else { "stopped" }));
        export_diagnostics(&self.stats, vault_id.as_deref(), &states)
    }

    pub(crate) fn migrate_import_webstore(
        &self,
        index: u32,
        total: u32,
        chunk: &str,
        snapshot_hash: &str,
    ) -> Result<&'static str, CommandError> {
        let store = self.store()?;
        let vault_id = self.active_vault(&store)?;
        let dir = self.migration_dir()?;
        let state = migration::push_webstore_chunk(&mut lock(&store), &dir, &vault_id, index, total, chunk.as_bytes(), snapshot_hash)?;
        let state = if state == MigrationState::BackedUp { self.resume_migration()? } else { state };
        Ok(state.as_str())
    }

    pub(crate) fn migrate_finalize(
        &self,
        host: &Arc<dyn Host>,
        webview_hash: &str,
        current: Option<(&str, &str)>,
        queue_tail: u64,
    ) -> Result<FinalizeOut, CommandError> {
        let store = self.store()?;
        let vault_id = self.active_vault(&store)?;
        let dir = self.migration_dir()?;
        let input = *lock(&self.finalize_input);
        let name_key = self.name_key(&vault_id)?;
        let sink_store = store.clone();
        let result = finalize_cutover(
            host.clipboard(),
            input,
            |live| {
                let mut guard = lock(&store);
                let mut clock = lock(&self.clock);
                let current = current.map(|(bytes, hash)| (bytes.as_bytes(), hash));
                Ok(migration::finalize(&mut guard, &dir, &vault_id, &name_key, &mut clock, webview_hash, current, live, queue_tail)?)
            },
            |entries| {
                let projection = lock(&store).projection(&vault_id)?;
                Ok(memo_bridge::mirror(entries, &projection, now_ms() as i64))
            },
            || self.make_sink(sink_store, vault_id.clone()),
            |entries, max| host.save_clipboard(entries, max),
        )?;
        if matches!(result, Finalize::Native { .. }) {
            host.clipboard_changed();
            // finalize already brought the live memos in.
            let settings = self.settings();
            if settings.local_only {
                self.write_settings(SyncSettings { local_only: false, ..settings })?;
            }
            *lock(&self.finalize_input) = FinalizeInput::NotThisBoot;
        }
        Ok(result.into())
    }

    /// Applies one queued webview op (G8: also the steady-state write path
    /// after cutover) and signals `sync:chat-changed` when the projection
    /// moved. Works on a native device with the flag off too (E1); the
    /// scheduler is only nudged when one runs. The result tells a replay
    /// (`duplicate`) from a reused qseq (`stale_qseq`, E3).
    pub(crate) fn queue_apply(
        &self,
        host: &dyn Host,
        qseq: u64,
        op: serde_json::Value,
    ) -> Result<migration::QueueApply, CommandError> {
        let store = self.local_store()?;
        let op: QueuedOp = serde_json::from_value(op).map_err(|_| CommandError::new("invalid_params", "unknown queued op"))?;
        let vault_id = self.active_vault(&store)?;
        let name_key = self.name_key(&vault_id)?;
        let result = {
            let mut guard = lock(&store);
            let mut clock = lock(&self.clock);
            migration::apply_queue_item(&mut guard, &vault_id, &name_key, &mut clock, qseq, &op)?
        };
        if result == (migration::QueueApply::Applied { changed: true }) {
            if let Some(tx) = lock(&self.events).as_ref() {
                let _ = tx.send(SchedEvent::LocalChange);
            }
            self.notify_chat(host);
        }
        Ok(result)
    }

    /// G8 `sync_chat_load`: conversations (active paths, images from the
    /// local blobs) and translation history of the active vault's
    /// projection, under the webview's source ids. `not_native` before the
    /// cutover with the flag on; with the flag off it works for a native
    /// device (E1) and is `disabled` otherwise (nothing created).
    pub(crate) fn chat_load(&self) -> Result<migration::ChatStore, CommandError> {
        let store = self.local_store()?;
        let guard = lock(&store);
        if MigrationState::load(&guard)? != MigrationState::Native {
            return Err(CommandError::new("not_native", "conversations are not stored natively yet"));
        }
        let vault = guard.active_vault()?.ok_or_else(CommandError::not_joined)?;
        let (set, forks) = guard.load_set(&vault)?;
        let projection = project(&set, &forks).map_err(StoreError::from)?;
        let index = migration::SrcIndex::of(&set);
        let out = migration::chat_store(&projection, &index, |blob_id| Ok(guard.blob_bytes(blob_id)?.map(|(_, bytes)| bytes)))?;
        // Under the store lock: no write can land between this projection
        // and the baseline the next `sync:chat-changed` compares with.
        *lock(&self.chat_fp) = Some(chat_fingerprint(&projection));
        drop(guard);
        self.chat_src_fallbacks.store(out.src_fallbacks as u64, Ordering::SeqCst);
        if out.src_fallbacks > 0 {
            log_event("chat_src_fallback", &[("count", Field::Num(out.src_fallbacks as u64))]);
        }
        Ok(out)
    }
}

/// Why a cycle never reached the engine.
enum Early {
    Provider(ProviderError),
    Command(CommandError),
}

/// The status, notice, and failure class of a cycle that stopped before the
/// engine, the same mapping the engine uses for its own halts (D10).
fn early_status(early: &Early) -> (CycleStatus, Option<EngineNotice>, Option<FailureClass>) {
    let stop = |reason| CycleStatus::Stopped { reason };
    match early {
        Early::Provider(err) => match err {
            ProviderError::Auth { reconsent: true } => {
                (stop(StopReason::Reconsent), Some(EngineNotice::ReconsentRequired), Some(FailureClass::Auth))
            }
            ProviderError::Auth { reconsent: false } => {
                (stop(StopReason::PermissionDenied), Some(EngineNotice::PermissionDenied), Some(FailureClass::Auth))
            }
            ProviderError::BindingMismatch => {
                (stop(StopReason::BindingMismatch), Some(EngineNotice::BindingMismatch), Some(FailureClass::Binding))
            }
            // Includes an OAuth client this build was not configured with.
            ProviderError::Unsupported(_) => {
                (stop(StopReason::Unsupported), Some(EngineNotice::Unsupported), Some(FailureClass::Unsupported))
            }
            ProviderError::NotFound => {
                (stop(StopReason::Configuration), Some(EngineNotice::ConfigurationError), Some(FailureClass::Permanent))
            }
            ProviderError::RateLimited { retry_after } => (
                CycleStatus::RateLimited { retry_after: *retry_after },
                Some(EngineNotice::RateLimited),
                Some(FailureClass::Rate),
            ),
            ProviderError::QuotaExceeded => (
                CycleStatus::Failed { class: FailureClass::Quota },
                Some(EngineNotice::QuotaExceeded),
                Some(FailureClass::Quota),
            ),
            other => {
                let class = engine::classify(other);
                (CycleStatus::Failed { class }, None, Some(class))
            }
        },
        Early::Command(e) => match e.code {
            "not_joined" => (stop(StopReason::NotJoined), Some(EngineNotice::NotJoined), None),
            // Disconnected meanwhile: stop quietly, the user did it.
            "not_connected" | "disabled" => (stop(StopReason::Configuration), None, None),
            "keychain" => (
                CycleStatus::Failed { class: FailureClass::Local },
                Some(EngineNotice::KeychainUnavailable),
                Some(FailureClass::Local),
            ),
            _ => (CycleStatus::Failed { class: FailureClass::Local }, None, Some(FailureClass::Local)),
        },
    }
}

/// Whether the active vault was created or joined under the stored
/// connection's account (A8). No connection, or no recorded binding: no.
fn vault_bound(store: &Store) -> bool {
    match (load_connection(store), store.meta_get(META_VAULT_BINDING)) {
        (Ok(Some(meta)), Ok(Some(bound))) => bound == meta.binding_fp,
        _ => false,
    }
}

struct StoreStates {
    migration: &'static str,
    discovery: &'static str,
    wipe: &'static str,
}

/// Digest of the conversation and translation entities (tombstones too) of
/// a projection: what `sync:chat-changed` compares. Memos and policy are
/// left out.
fn chat_fingerprint(p: &Projection) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for ((kind, id), state) in p.entities.iter().filter(|((kind, _), _)| matches!(kind, Kind::Conv | Kind::Tr)) {
        hasher.update(kind.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(id.as_bytes());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(state).unwrap_or_default());
        hasher.update([0xff]);
    }
    hasher.finalize().into()
}

fn store_states(store: &Store) -> StoreStates {
    let migration = MigrationState::load(store).map_or("unknown", MigrationState::as_str);
    let discovery = match store.meta_get("discovery_state").ok().flatten().as_deref() {
        None => "none",
        Some("none") => "nothing_found",
        Some("found") => "found",
        Some("retry") => "retry",
        Some(_) => "unknown",
    };
    let wipe = match store.meta_get("wipe_state").ok().flatten().as_deref() {
        None => "none",
        Some("started") => "started",
        Some("done") => "done",
        Some(_) => "unknown",
    };
    StoreStates { migration, discovery, wipe }
}

/// The fixed `notice` tag of an engine notice.
fn notice_label(notice: &EngineNotice) -> String {
    serde_json::to_value(notice)
        .ok()
        .and_then(|v| v.get("notice").and_then(|n| n.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// G9 U4 devices / U5 restored (caches behind SyncStatus)

/// The last verified read of the vault's device records, under the vault
/// generation it was read in.
#[derive(Debug, Clone)]
struct DevicesCache {
    gen: u64,
    vault: String,
    read_at: u64,
    /// The next refresh may not wait for the throttle (a removal).
    stale: bool,
    /// Device id → `last_seen_hlc.wall_ms` of its record.
    seen: BTreeMap<String, u64>,
    /// Records read earlier under this generation and absent since (a
    /// removal); kept with their last known time until the next switch.
    gone: BTreeMap<String, u64>,
}

impl DevicesCache {
    fn current(&self, vault: &str, gen: u64) -> bool {
        self.gen == gen && self.vault == vault
    }

    fn due(cache: Option<&Self>, vault: &str, gen: u64, now: u64) -> bool {
        match cache {
            Some(c) if c.current(vault, gen) => c.stale || now.saturating_sub(c.read_at) >= DEVICES_REFRESH_MS,
            _ => true,
        }
    }

    /// The cache after a read of `devices`: records that disappeared since
    /// the previous read of the same generation become `gone`.
    fn next(prev: Option<&Self>, vault: &str, gen: u64, now: u64, devices: Vec<engine::DeviceSeen>) -> Self {
        let seen: BTreeMap<String, u64> = devices.into_iter().map(|d| (d.device_id, d.last_seen_ms)).collect();
        let mut gone = BTreeMap::new();
        if let Some(prev) = prev.filter(|p| p.current(vault, gen)) {
            gone.extend(prev.gone.iter().map(|(id, at)| (id.clone(), *at)));
            gone.extend(prev.seen.iter().map(|(id, at)| (id.clone(), *at)));
            gone.retain(|id, _| !seen.contains_key(id));
        }
        Self { gen, vault: vault.to_owned(), read_at: now, stale: false, seen, gone }
    }
}

/// The status list: this device first, then the others by id. Only ids and
/// times; a cache of another vault or generation contributes nothing.
fn device_list(cache: Option<&DevicesCache>, vault: &str, gen: u64, self_id: &str, self_removed: bool) -> Vec<DeviceOut> {
    let cache = cache.filter(|c| c.current(vault, gen));
    let out = |id: &str, last_seen: Option<u64>, is_self: bool, removed: bool| DeviceOut {
        id: id.to_owned(),
        short: id.chars().take(DEVICE_SHORT_LEN).collect(),
        last_seen,
        is_self,
        removed,
    };
    let mut list = vec![out(self_id, cache.and_then(|c| c.seen.get(self_id).copied()), true, self_removed)];
    if let Some(cache) = cache {
        for (id, at) in cache.seen.iter().filter(|(id, _)| *id != self_id) {
            list.push(out(id, Some(*at), false, false));
        }
        for (id, at) in cache.gone.iter().filter(|(id, _)| *id != self_id) {
            list.push(out(id, Some(*at), false, true));
        }
        list[1..].sort_by(|a, b| a.id.cmp(&b.id));
    }
    list
}

/// The active vault's restored-after-delete digests and the dismissed ones.
#[derive(Debug, Clone)]
struct RestoredCache {
    gen: u64,
    vault: String,
    current: Vec<(Kind, [u8; 32])>,
    dismissed: BTreeSet<[u8; 32]>,
}

impl RestoredCache {
    /// Counts per kind of what is restored and not dismissed (no ids).
    fn counts(&self) -> Vec<RestoredOut> {
        let mut counts: BTreeMap<Kind, usize> = BTreeMap::new();
        for (kind, digest) in &self.current {
            if !self.dismissed.contains(digest) {
                *counts.entry(*kind).or_default() += 1;
            }
        }
        counts.into_iter().map(|(kind, count)| RestoredOut { kind, count }).collect()
    }
}

fn restored_meta_key(vault: &str) -> String {
    format!("{META_RESTORED_DISMISSED}{vault}")
}

/// The persisted dismissed digests of `vault`. Unreadable is empty: at worst
/// a dismissed notice shows again.
fn load_dismissed(store: &Store, vault: &str) -> BTreeSet<[u8; 32]> {
    let Ok(Some(raw)) = store.meta_get(&restored_meta_key(vault)) else { return BTreeSet::new() };
    let Ok(list) = serde_json::from_str::<Vec<String>>(&raw) else {
        log_event("restored_dismissed_unreadable", &[]);
        return BTreeSet::new();
    };
    list.iter()
        .filter_map(|h| data_encoding::HEXLOWER.decode(h.as_bytes()).ok())
        .filter_map(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .collect()
}

/// Writes at most [`RESTORED_DISMISSED_CAP`] digests.
fn save_dismissed(store: &mut Store, vault: &str, dismissed: &BTreeSet<[u8; 32]>) -> Result<(), StoreError> {
    let list: Vec<String> = dismissed.iter().take(RESTORED_DISMISSED_CAP).map(|d| super::crypto::hex(d)).collect();
    let raw = serde_json::to_string(&list).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    store.meta_set(&restored_meta_key(vault), &raw)
}

// ---------------------------------------------------------------------------
// What commands return (content-free; the RK only from create / reissue)

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConnectedInfo {
    pub(crate) provider: ProviderKind,
    pub(crate) account_label: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SyncStatus {
    pub(crate) enabled: bool,
    pub(crate) provider: Option<ConnectedInfo>,
    pub(crate) vault: Option<VaultTag>,
    pub(crate) last_status: Option<CycleStatus>,
    pub(crate) last_cycle_at: Option<u64>,
    pub(crate) running: bool,
    pub(crate) stats: StatsSnapshot,
    pub(crate) notices: Vec<EngineNotice>,
    pub(crate) discovery: &'static str,
    pub(crate) discovery_retry_at: Option<u64>,
    pub(crate) migration: &'static str,
    /// G8: the same value as `migration`, under the contract name.
    pub(crate) migration_state: &'static str,
    /// `meta.migration_flushed_qseq`: the last queue item Rust applied.
    pub(crate) queue_flushed_qseq: Option<u64>,
    /// `meta.migration_ack_qseq`: the queue tail the M7 cutover reflected.
    pub(crate) ack_qseq: Option<u64>,
    /// Entities the last `sync_chat_load` exposed under their entity id (no
    /// verifiable `src_id`).
    pub(crate) chat_src_fallbacks: u64,
    pub(crate) wipe: &'static str,
    pub(crate) reissue_pending: bool,
    pub(crate) create_pending: bool,
    pub(crate) join_preview: Option<JoinSummaryOut>,
    pub(crate) finalize_this_boot: bool,
    /// Connected, but the active vault (if any) belongs to another account
    /// or none is active: discover, then create or join (A8).
    pub(crate) needs_vault: bool,
    /// E1a: the local `sync.db` exists but could not be peeked, opened or
    /// loaded. The other fields are then defaults that must not be read as
    /// "not native": the webview keeps its current mode and retries.
    pub(crate) store_error: bool,
    /// G9 U4: this device first, then the active vault's other devices from
    /// their verified records. Ids and times only.
    pub(crate) devices: Vec<DeviceOut>,
    /// G9 U5: restored-after-delete counts per kind, until dismissed
    /// (`sync_dismiss_notice("restored")`). No entity ids or text.
    pub(crate) restored: Vec<RestoredOut>,
}

/// G9 U4: one device of the active vault. `id` is what
/// `sync_remove_device` takes; `removed` marks a record that was read
/// earlier and is gone now (or this device, after `DeviceRemoved`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DeviceOut {
    pub(crate) id: String,
    pub(crate) short: String,
    /// `last_seen_hlc.wall_ms` of its record; `None` until read.
    pub(crate) last_seen: Option<u64>,
    pub(crate) is_self: bool,
    pub(crate) removed: bool,
}

/// G9 U5: how many entities of `kind` were kept against a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct RestoredOut {
    pub(crate) kind: Kind,
    pub(crate) count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct JoinSummaryOut {
    pub(crate) skip: usize,
    pub(crate) republish: usize,
    pub(crate) copy: usize,
    pub(crate) evicted: usize,
    pub(crate) tombstones: usize,
}

impl From<JoinSummary> for JoinSummaryOut {
    fn from(s: JoinSummary) -> Self {
        Self { skip: s.skip, republish: s.republish, copy: s.copy, evicted: s.evicted, tombstones: s.tombstones }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DiscoverySummary {
    pub(crate) state: &'static str,
    pub(crate) vaults: usize,
}

/// Shown once (D9b). No `Debug`: it carries the RK.
#[derive(Serialize)]
pub(crate) struct CreatedVault {
    pub(crate) vault: VaultTag,
    pub(crate) recovery_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum ConfirmResult {
    Created { vault: VaultTag },
    Reissued { not_revocation: bool },
}

#[derive(Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum ReissueResult {
    ShowKey { recovery_key: String },
    Published { not_revocation: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct JoinPreviewOut {
    pub(crate) vault: VaultTag,
    pub(crate) summary: JoinSummaryOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum JoinCommitOut {
    Joined { vault: VaultTag, ops: usize },
    PreviewChanged { summary: JoinSummaryOut },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RejoinOut {
    pub(crate) vault: VaultTag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum FinalizeOut {
    NeedSnapshot,
    Native { ack_qseq: u64 },
    VerifyFailed { attempts: u32, mismatches: usize },
    Deferred { attempts: u32, mismatches: usize },
}

impl From<Finalize> for FinalizeOut {
    fn from(f: Finalize) -> Self {
        match f {
            Finalize::NeedSnapshot => Self::NeedSnapshot,
            Finalize::Native { ack_qseq } => Self::Native { ack_qseq },
            Finalize::VerifyFailed { attempts, mismatches } => Self::VerifyFailed { attempts, mismatches: mismatches.len() },
            Finalize::Deferred { attempts, mismatches } => Self::Deferred { attempts, mismatches: mismatches.len() },
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri commands (registered in lib.rs)

type Runtime<'a> = tauri::State<'a, Arc<SyncRuntime>>;

fn host(app: &AppHandle, clip: &tauri::State<'_, Arc<ClipboardHandle>>) -> Arc<dyn Host> {
    Arc::new(AppHost { app: app.clone(), clipboard: Arc::clone(clip) })
}

#[tauri::command]
pub(crate) async fn sync_get_status(rt: Runtime<'_>) -> Result<SyncStatus, CommandError> {
    Ok(rt.status())
}

#[tauri::command]
pub(crate) async fn sync_set_enabled(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
    enabled: bool,
) -> Result<SyncStatus, CommandError> {
    let host = host(&app, &clip);
    rt.set_enabled(&host, enabled).await
}

#[tauri::command]
pub(crate) async fn sync_connect_start(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
    oauth: tauri::State<'_, OAuthCallbackState>,
    provider: ProviderKind,
    params: Option<serde_json::Value>,
) -> Result<ConnectedInfo, CommandError> {
    let host = host(&app, &clip);
    match provider {
        ProviderKind::Gdrive | ProviderKind::Onedrive => {
            use tauri_plugin_opener::OpenerExt;
            let opener = app.clone();
            let open = move |url: String| opener.opener().open_url(url, None::<&str>).map_err(|e| e.to_string());
            rt.connect_oauth(&host, &oauth, &open, provider).await
        }
        ProviderKind::Webdav | ProviderKind::R2 => {
            rt.require_enabled()?;
            let params = params.ok_or_else(|| CommandError::new("invalid_params", "connection parameters are missing"))?;
            rt.connect_keyed(&host, provider, params).await
        }
    }
}

#[tauri::command]
pub(crate) fn sync_connect_cancel(oauth: tauri::State<'_, OAuthCallbackState>) {
    cancel_native(&oauth);
}

#[tauri::command]
pub(crate) async fn sync_disconnect(rt: Runtime<'_>, wipe_cloud: bool, confirm: String) -> Result<(), CommandError> {
    rt.disconnect(wipe_cloud, &confirm).await
}

#[tauri::command]
pub(crate) async fn sync_discover(rt: Runtime<'_>) -> Result<DiscoverySummary, CommandError> {
    rt.discover().await
}

#[tauri::command]
pub(crate) async fn sync_create_vault(rt: Runtime<'_>) -> Result<CreatedVault, CommandError> {
    rt.create_vault()
}

#[tauri::command]
pub(crate) async fn sync_confirm_rk(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
    rk: String,
) -> Result<ConfirmResult, CommandError> {
    let host = host(&app, &clip);
    let rk = Zeroizing::new(rk);
    rt.confirm_rk(&host, &rk).await
}

#[tauri::command]
pub(crate) async fn sync_join_preview(rt: Runtime<'_>, rk: String) -> Result<JoinPreviewOut, CommandError> {
    let rk = Zeroizing::new(rk);
    rt.join_preview(&rk).await
}

#[tauri::command]
pub(crate) async fn sync_join_commit(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
) -> Result<JoinCommitOut, CommandError> {
    let host = host(&app, &clip);
    rt.join_commit(&host).await
}

/// D1: a removed device re-enters the RK and continues in the same vault
/// under a fresh device identity.
#[tauri::command]
pub(crate) async fn sync_rejoin(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
    rk: String,
) -> Result<RejoinOut, CommandError> {
    let host = host(&app, &clip);
    let rk = Zeroizing::new(rk);
    rt.rejoin(&host, &rk).await
}

#[tauri::command]
pub(crate) async fn sync_reissue_rk(rt: Runtime<'_>) -> Result<ReissueResult, CommandError> {
    rt.reissue_rk().await
}

#[tauri::command]
pub(crate) async fn sync_remove_device(rt: Runtime<'_>, device_id: String) -> Result<(), CommandError> {
    rt.remove_device(&device_id).await
}

#[tauri::command]
pub(crate) fn sync_dismiss_notice(rt: Runtime<'_>, notice: String) -> bool {
    rt.dismiss_notice(&notice)
}

#[tauri::command]
pub(crate) async fn sync_export_diagnostics(rt: Runtime<'_>) -> Result<String, CommandError> {
    Ok(rt.export_diagnostics())
}

#[tauri::command]
pub(crate) async fn sync_migrate_import_webstore(
    rt: Runtime<'_>,
    index: u32,
    total: u32,
    chunk: String,
    snapshot_hash: String,
) -> Result<&'static str, CommandError> {
    rt.migrate_import_webstore(index, total, &chunk, &snapshot_hash)
}

#[tauri::command]
pub(crate) async fn sync_migrate_finalize(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
    webview_hash: String,
    current: Option<String>,
    current_hash: Option<String>,
    queue_tail: u64,
) -> Result<FinalizeOut, CommandError> {
    let host = host(&app, &clip);
    let current = match (&current, &current_hash) {
        (Some(bytes), Some(hash)) => Some((bytes.as_str(), hash.as_str())),
        _ => None,
    };
    rt.migrate_finalize(&host, &webview_hash, current, queue_tail)
}

/// G8: one queued webview op; `{"result": "applied", "changed": bool}`,
/// `{"result": "duplicate"}`, or `{"result": "stale_qseq", "watermark": n}`
/// (see [`SyncRuntime::queue_apply`]).
#[tauri::command]
pub(crate) async fn sync_queue_apply(
    app: AppHandle,
    rt: Runtime<'_>,
    clip: tauri::State<'_, Arc<ClipboardHandle>>,
    qseq: u64,
    op: serde_json::Value,
) -> Result<migration::QueueApply, CommandError> {
    let host = host(&app, &clip);
    rt.queue_apply(host.as_ref(), qseq, op)
}

/// G8: the native conversations and translation history (see
/// [`SyncRuntime::chat_load`]).
#[tauri::command]
pub(crate) async fn sync_chat_load(rt: Runtime<'_>) -> Result<migration::ChatStore, CommandError> {
    rt.chat_load()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::sync::migration::QueueApply;
    use crate::sync::provider::fake::FakeStore;
    use crate::sync::provider::transport::MockTransport;
    use crate::sync::provider::webdav::tests_support::{self, DavServer};
    use crate::sync::rotation::UploadState;
    use crate::sync::secrets::MemoryStore;
    use crate::sync::sim_tests::{create, identity, TempDir, VAULT};

    // -- fakes -----------------------------------------------------------

    struct NoPause;

    impl Pause for NoPause {
        async fn pause(&self, _: Duration) {}
    }

    #[derive(Default)]
    struct FakeSink {
        changed: AtomicUsize,
    }

    impl MemoSink for FakeSink {
        fn changed(&self, _: Option<&ClipEntry>, _: Option<&ClipEntry>) -> Result<Option<String>, String> {
            self.changed.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }

        fn delete_all(&self, _: &[String]) -> Result<(), String> {
            Ok(())
        }
    }

    /// Records saves (and whether the sink slot was held during each) and
    /// emits; never spawns.
    struct FakeHost {
        clip: Arc<ClipboardHandle>,
        log: Arc<Mutex<Vec<String>>>,
    }

    type Log = Arc<Mutex<Vec<String>>>;

    impl FakeHost {
        fn create() -> (Arc<dyn Host>, Arc<ClipboardHandle>, Log) {
            let clip = ClipboardHandle::new();
            let log = Arc::new(Mutex::new(Vec::new()));
            (Arc::new(FakeHost { clip: clip.clone(), log: log.clone() }), clip, log)
        }
    }

    impl Host for FakeHost {
        fn clipboard(&self) -> &ClipboardHandle {
            &self.clip
        }

        fn save_clipboard(&self, entries: &[ClipEntry], _: usize) {
            let slot_held = self.clip.memo_sink.try_lock().is_err();
            lock(&self.log).push(format!("save:{}:slot_held={slot_held}", entries.len()));
        }

        fn clipboard_changed(&self) {
            lock(&self.log).push("emit".to_owned());
        }

        fn chat_changed(&self) {
            lock(&self.log).push("chat".to_owned());
        }

        fn spawn(&self, _: BoxFuture) -> Option<tauri::async_runtime::JoinHandle<()>> {
            lock(&self.log).push("spawn".to_owned());
            None
        }
    }

    fn entry(id: &str, text: &str, kind: EntryKind) -> ClipEntry {
        ClipEntry {
            id: id.to_owned(),
            text: text.to_owned(),
            preview: clipboard::make_preview(text),
            ts: 1,
            pinned: false,
            note: None,
            kind,
            conflict_of: None,
        }
    }

    fn runtime(dir: &Path) -> Arc<SyncRuntime<MemoryStore>> {
        Arc::new(SyncRuntime::new(Some(dir.to_path_buf()), MemoryStore::default(), Box::new(|| None)))
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// A store at the runtime's path: identity, active vault, migration
    /// state, and one memo `m1`.
    fn seed_store(dir: &Path, migration_state: &str) {
        let mut store = Store::open(&dir.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        store.init_identity(&identity("dev-a")).unwrap();
        store.set_active_vault(VAULT).unwrap();
        store.meta_set("migration_state", migration_state).unwrap();
        store.insert_local_op(VAULT, create(1, 1_000, "m1", "from the store"), UploadState::Never).unwrap();
    }

    // -- flag and boot (AC 15) ------------------------------------------

    #[test]
    fn boot_plan_is_empty_with_the_flag_off() {
        assert!(boot_plan(false).is_empty());
        assert_eq!(
            boot_plan(true),
            &[BootStep::OpenStore, BootStep::ResumeMigration, BootStep::NativeCutover, BootStep::StartScheduler]
        );
    }

    #[test]
    fn settings_default_off_and_fail_closed() {
        let dir = TempDir::new("g7-settings");
        assert_eq!(load_settings(None), SyncSettings::default());
        assert_eq!(load_settings(Some(&dir.0)), SyncSettings { enabled: false, local_only: false });
        std::fs::write(settings_path(&dir.0), b"{not json").unwrap();
        assert!(!load_settings(Some(&dir.0)).enabled, "malformed settings must stay off");
        std::fs::write(settings_path(&dir.0), b"{\"enabled\":\"yes\"}").unwrap();
        assert!(!load_settings(Some(&dir.0)).enabled, "a wrong type must stay off");
        // D9: only a JSON object with the boolean `enabled == true` is on.
        for off in [&b"[true]"[..], b"[true, true]", b"true", b"null", b"1", b"{\"enabled\":1}", b"{\"enabled\":\"true\"}", b"{}"] {
            std::fs::write(settings_path(&dir.0), off).unwrap();
            assert_eq!(load_settings(Some(&dir.0)), SyncSettings::default(), "{:?}", String::from_utf8_lossy(off));
        }
        std::fs::write(settings_path(&dir.0), b"{\"enabled\":true,\"local_only\":\"yes\",\"extra\":[1]}").unwrap();
        assert_eq!(load_settings(Some(&dir.0)), SyncSettings { enabled: true, local_only: false });
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: true }).unwrap();
        assert_eq!(load_settings(Some(&dir.0)), SyncSettings { enabled: true, local_only: true });
    }

    #[test]
    fn boot_with_the_flag_off_touches_no_sync_path() {
        let dir = TempDir::new("g7-off");
        let (host, clip, log) = FakeHost::create();
        let before = vec![entry("m1", "a memo", EntryKind::Memo), entry("c1", "a clip", EntryKind::Clip)];
        lock(&clip.state).entries = before.clone();
        let rt = runtime(&dir.0);

        assert!(rt.boot(&host, LoadOutcome::Loaded).is_empty());

        assert!(dir_entries(&dir.0).is_empty(), "nothing created: {:?}", dir_entries(&dir.0));
        assert!(rt.store.get().is_none(), "no store opened");
        assert!(lock(&rt.provider).is_none(), "no provider built");
        assert!(clip.memo_sink.lock().unwrap().is_none(), "no sink installed");
        assert!(lock(&rt.scheduler).is_none(), "no scheduler");
        assert!(lock(&log).is_empty(), "no save, emit, or spawn: {:?}", lock(&log));
        assert_eq!(lock(&clip.state).entries, before, "clipboard unchanged");
        // Status and diagnostics stay off without opening anything either.
        let status = rt.status();
        assert!(!status.enabled && status.provider.is_none() && status.vault.is_none());
        assert!(rt.export_diagnostics().contains("\"off\""));
        assert!(dir_entries(&dir.0).is_empty());
        // Every sync command refuses.
        assert_eq!(rt.create_vault().err().map(|e| e.code), Some("disabled"));
        // The flag is checked before any input is parsed (ADV-1).
        assert_eq!(rt.queue_apply(host.as_ref(), 1, serde_json::json!({})).err().map(|e| e.code), Some("disabled"));
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("disabled"));
        let disconnect = tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(rt.disconnect(true, "wrong"));
        assert_eq!(disconnect.err().map(|e| e.code), Some("disabled"));
        assert_eq!(rt.migrate_import_webstore(0, 1, "{}", "h").err().map(|e| e.code), Some("disabled"));
        assert!(dir_entries(&dir.0).is_empty());
    }

    #[test]
    fn boot_with_the_flag_on_and_no_vault_opens_only_the_store() {
        let dir = TempDir::new("g7-on-empty");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        let (host, clip, log) = FakeHost::create();
        let rt = runtime(&dir.0);
        let done = rt.boot(&host, LoadOutcome::Missing);
        assert_eq!(done, boot_plan(true));
        assert!(dir.0.join(SYNC_DIR).join(STORE_FILE).exists());
        assert!(clip.memo_sink.lock().unwrap().is_none(), "no sink before native");
        assert!(lock(&log).is_empty(), "no mirror, no scheduler without a connection: {:?}", lock(&log));
    }

    #[test]
    fn native_boot_mirrors_and_saves_before_the_sink_is_installed() {
        let dir = TempDir::new("g7-native");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let (host, clip, log) = FakeHost::create();
        lock(&clip.state).entries = vec![entry("stale", "stale memo", EntryKind::Memo), entry("c1", "clip", EntryKind::Clip)];
        let rt = runtime(&dir.0);

        assert_eq!(rt.boot(&host, LoadOutcome::Loaded), boot_plan(true));

        let entries = lock(&clip.state).entries.clone();
        let memos: Vec<&str> = entries.iter().filter(|e| e.kind == EntryKind::Memo).map(|e| e.text.as_str()).collect();
        assert_eq!(memos, vec!["from the store"], "the mirror replaced the memos");
        assert!(entries.iter().any(|e| e.id == "c1"), "clips kept");
        // One save, inside the critical section, then the emit; the sink is in.
        assert_eq!(*lock(&log), vec!["save:2:slot_held=true".to_owned(), "emit".to_owned()]);
        assert!(clip.memo_sink.lock().unwrap().is_some());
    }

    #[test]
    fn a_non_native_boot_installs_no_sink_and_never_mirrors() {
        for state in ["none", "verified"] {
            let dir = TempDir::new("g7-not-native");
            save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
            seed_store(&dir.0, state);
            let (host, clip, log) = FakeHost::create();
            let before = vec![entry("stale", "stale memo", EntryKind::Memo)];
            lock(&clip.state).entries = before.clone();
            let rt = runtime(&dir.0);
            rt.boot(&host, LoadOutcome::Loaded);
            assert!(clip.memo_sink.lock().unwrap().is_none(), "{state}");
            assert_eq!(lock(&clip.state).entries, before, "{state}");
            assert!(lock(&log).is_empty(), "{state}: {:?}", lock(&log));
        }
    }

    #[test]
    fn local_only_edits_are_reconciled_into_the_store_before_the_mirror() {
        let dir = TempDir::new("g7-reconcile");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: true }).unwrap();
        seed_store(&dir.0, "native");
        let (host, clip, _) = FakeHost::create();
        // m1 edited while sync was off; m2 created while off.
        lock(&clip.state).entries =
            vec![entry("m1", "edited while off", EntryKind::Memo), entry("m2", "new while off", EntryKind::Memo)];
        let rt = runtime(&dir.0);
        rt.boot(&host, LoadOutcome::Loaded);

        let mut texts: Vec<String> = lock(&clip.state).entries.iter().map(|e| e.text.clone()).collect();
        texts.sort();
        assert_eq!(texts, vec!["edited while off".to_owned(), "new while off".to_owned()], "nothing reverted");
        let store = rt.store.get().unwrap();
        let projection = lock(store).projection(VAULT).unwrap();
        let mut stored: Vec<String> = projection
            .entities
            .values()
            .filter_map(|e| match e {
                crate::sync::merge::EntityState::Memo(m) => Some(m.text.clone()),
                _ => None,
            })
            .collect();
        stored.sort();
        assert_eq!(stored, texts, "both edits are ops in the store");
        assert!(!load_settings(Some(&dir.0)).local_only, "marker cleared after the cutover");
        assert!(clip.memo_sink.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn disabling_at_runtime_uninstalls_the_sink_and_marks_local_only() {
        let dir = TempDir::new("g7-disable");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let (host, clip, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        rt.boot(&host, LoadOutcome::Loaded);
        assert!(clip.memo_sink.lock().unwrap().is_some());

        let status = rt.set_enabled(&host, false).await.unwrap();
        assert!(!status.enabled);
        assert!(clip.memo_sink.lock().unwrap().is_none());
        assert_eq!(load_settings(Some(&dir.0)), SyncSettings { enabled: false, local_only: true });
        assert_eq!(rt.create_vault().err().map(|e| e.code), Some("disabled"));
    }

    // -- finalize decision table (G4 delta review) -------------------------

    #[test]
    fn finalize_input_decision_table() {
        let corrupt = LoadOutcome::Corrupt { preserved: PathBuf::from("clipboard.json.corrupt") };
        let cases = [
            (Some(&LoadOutcome::Unreadable), Some(true), FinalizeInput::NotThisBoot),
            (Some(&LoadOutcome::Unreadable), Some(false), FinalizeInput::NotThisBoot),
            (Some(&corrupt), Some(true), FinalizeInput::Backup),
            (Some(&corrupt), Some(false), FinalizeInput::Backup),
            (Some(&LoadOutcome::Missing), Some(true), FinalizeInput::Backup),
            (Some(&LoadOutcome::Missing), Some(false), FinalizeInput::Live),
            (Some(&LoadOutcome::Loaded), Some(true), FinalizeInput::Live),
            (Some(&LoadOutcome::Loaded), Some(false), FinalizeInput::Live),
            // Unknown boot load or unreadable migration manifest: fail closed.
            (None, Some(false), FinalizeInput::NotThisBoot),
            (Some(&LoadOutcome::Loaded), None, FinalizeInput::NotThisBoot),
        ];
        for (outcome, recorded, want) in cases {
            assert_eq!(finalize_input(outcome, recorded), want, "{outcome:?} recorded={recorded:?}");
        }
    }

    // -- cutover ordering (AD-A2) -----------------------------------------

    fn native() -> Result<Finalize, CommandError> {
        Ok(Finalize::Native { ack_qseq: 7 })
    }

    #[test]
    fn finalize_cutover_runs_finalize_mirror_save_then_installs_the_sink() {
        let clip = ClipboardHandle::new();
        lock(&clip.state).entries = vec![entry("m1", "memo", EntryKind::Memo), entry("c1", "clip", EntryKind::Clip)];
        let steps = Mutex::new(Vec::new());
        let sink: Arc<dyn MemoSink> = Arc::new(FakeSink::default());
        let out = finalize_cutover(
            &clip,
            FinalizeInput::Live,
            |live| {
                let ids: Vec<&str> = live.unwrap().iter().map(|e| e.id.as_str()).collect();
                lock(&steps).push(format!("finalize:{}", ids.join(",")));
                native()
            },
            |entries| {
                lock(&steps).push("mirror".to_owned());
                let mut next = entries.to_vec();
                next[0].text = "mirrored".to_owned();
                Ok(next)
            },
            || {
                lock(&steps).push("sink".to_owned());
                sink.clone()
            },
            |entries, _| {
                let slot_held = clip.memo_sink.try_lock().is_err();
                let installed = !slot_held && clip.memo_sink.lock().unwrap().is_some();
                lock(&steps).push(format!("save:{}:held={slot_held}:installed={installed}", entries[0].text));
            },
        )
        .unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 7 });
        assert_eq!(
            *lock(&steps),
            vec!["finalize:m1", "mirror", "sink", "save:mirrored:held=true:installed=false"],
            "only memos reach finalize; the save sees the mirror and runs before the install"
        );
        assert!(clip.memo_sink.lock().unwrap().is_some(), "installed last");
        assert_eq!(lock(&clip.state).entries[0].text, "mirrored");
    }

    #[test]
    fn finalize_cutover_passes_none_for_backup_and_installs_nothing_unless_native() {
        let clip = ClipboardHandle::new();
        let before = vec![entry("m1", "memo", EntryKind::Memo)];
        lock(&clip.state).entries = before.clone();
        let saved = AtomicBool::new(false);
        for (input, result) in [
            (FinalizeInput::Backup, Finalize::NeedSnapshot),
            (FinalizeInput::Live, Finalize::VerifyFailed { attempts: 1, mismatches: vec!["x".to_owned()] }),
            (FinalizeInput::Live, Finalize::Deferred { attempts: 3, mismatches: Vec::new() }),
        ] {
            let got = finalize_cutover(
                &clip,
                input,
                |live| {
                    assert_eq!(live.is_none(), input == FinalizeInput::Backup);
                    Ok(result.clone())
                },
                |_| panic!("no mirror unless native"),
                || panic!("no sink unless native"),
                |_, _| saved.store(true, Ordering::SeqCst),
            )
            .unwrap();
            assert_eq!(got, result);
        }
        assert!(!saved.load(Ordering::SeqCst));
        assert!(clip.memo_sink.lock().unwrap().is_none());
        assert_eq!(lock(&clip.state).entries, before);
    }

    #[test]
    fn an_unreadable_boot_never_finalizes() {
        let clip = ClipboardHandle::new();
        let err = finalize_cutover(
            &clip,
            FinalizeInput::NotThisBoot,
            |_| panic!("finalize must not run"),
            |_| panic!("no mirror"),
            || panic!("no sink"),
            |_, _| panic!("no save"),
        )
        .unwrap_err();
        assert_eq!(err.code, "not_this_boot");
        assert!(clip.memo_sink.lock().unwrap().is_none());
    }

    #[test]
    fn a_memo_command_during_cutover_waits_and_reaches_the_new_sink() {
        let clip = ClipboardHandle::new();
        lock(&clip.state).entries = vec![entry("m1", "memo", EntryKind::Memo)];
        let sink = Arc::new(FakeSink::default());
        let started = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let mut worker = None;
            clipboard::cutover_in(
                &clip,
                |entries| {
                    worker = Some(scope.spawn(|| {
                        started.wait();
                        // What every memo command does: hold the slot throughout.
                        let slot = clip.memo_sink.lock().unwrap();
                        clipboard::update_memo_in(&clip, slot.as_deref(), "m1", "edited", 2, |_, _| {}).unwrap();
                    }));
                    started.wait();
                    std::thread::sleep(Duration::from_millis(50));
                    let installed: Arc<dyn MemoSink> = sink.clone();
                    Ok::<_, CommandError>(Some((entries.to_vec(), installed)))
                },
                |_, _| {},
            )
            .unwrap();
            worker.unwrap().join().unwrap();
        });
        assert_eq!(sink.changed.load(Ordering::SeqCst), 1, "the command used the sink installed by the cutover");
        assert_eq!(lock(&clip.state).entries[0].text, "edited");
    }

    #[test]
    fn mirror_in_saves_only_on_a_change() {
        let clip = ClipboardHandle::new();
        lock(&clip.state).entries = vec![entry("m1", "memo", EntryKind::Memo)];
        let saves = AtomicUsize::new(0);
        let same = clipboard::mirror_in(&clip, |e| Ok::<_, CommandError>(e.to_vec()), |_, _| {
            saves.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(!same);
        let changed = clipboard::mirror_in(&clip, |_| Ok::<_, CommandError>(Vec::new()), |_, _| {
            saves.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(changed);
        assert_eq!(saves.load(Ordering::SeqCst), 1);
        assert!(lock(&clip.state).entries.is_empty());
    }

    // -- command validation -----------------------------------------------

    #[test]
    fn a_wipe_needs_exactly_the_confirmation_word() {
        for wrong in ["", "삭 제", " 삭제", "삭제 ", "delete", "DELETE", "삭제삭제"] {
            assert_eq!(check_wipe_confirmation(true, wrong).unwrap_err().code, "confirmation_mismatch", "{wrong:?}");
        }
        assert!(check_wipe_confirmation(true, "삭제").is_ok());
        assert!(check_wipe_confirmation(false, "").is_ok(), "a plain disconnect needs no word");
    }

    #[tokio::test]
    async fn disconnect_with_a_wrong_word_changes_nothing() {
        let dir = TempDir::new("g7-wipe-word");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        let rt = runtime(&dir.0);
        let meta = ConnectMeta {
            binding_fp: binding_fp("webdav|https://dav.example.test/|alice"),
            params: ConnectParams::Webdav { base_url: "https://dav.example.test/".into(), username: "alice".into() },
        };
        let store = rt.open_store().unwrap();
        save_connection(&mut lock(&store), Some(&meta)).unwrap();
        write_secret(&rt.secrets, meta.secret(), b"pw").unwrap();

        assert_eq!(rt.disconnect(true, "delete").await.unwrap_err().code, "confirmation_mismatch");
        assert_eq!(load_connection(&lock(&store)).unwrap(), Some(meta.clone()), "still connected");
        assert!(read_secret(&rt.secrets, meta.secret()).unwrap().is_some(), "credential kept");

        rt.disconnect(false, "").await.unwrap();
        assert_eq!(load_connection(&lock(&store)).unwrap(), None);
        assert!(read_secret(&rt.secrets, meta.secret()).unwrap().is_none(), "credential removed");
    }

    #[tokio::test]
    async fn a_wrong_rk_reentry_is_refused_and_can_be_retried() {
        let remote = FakeStore::new(true);
        let secrets = MemoryStore::default();
        let store = Mutex::new(Store::open_in_memory().unwrap());
        let clock = Mutex::new(HlcClock::default());
        let result = vault::discover(&remote, &NoPause, now_ms).await;
        vault::record_discovery(&mut lock(&store), &result, now_ms()).unwrap();
        let fp = binding_fp("fake|default");
        let mut pending = vault::begin_create(&lock(&store), &fp, now_ms()).unwrap();
        let rk = pending.recovery_key().to_display().to_string();
        let other = crate::sync::recovery::RecoveryKey::generate().unwrap().to_display().to_string();
        let after_discovery = remote.requests();

        for wrong in ["", "not a key", other.as_str()] {
            let err = finish_create(&remote, &secrets, &store, &clock, &mut pending, wrong, None).await.unwrap_err();
            assert!(matches!(err.code, "rk_format" | "rk_wrong"), "{wrong:?} → {err:?}");
            assert!(secrets.accounts().is_empty(), "no key stored on a wrong RK");
            assert_eq!(remote.requests(), after_discovery, "nothing published on a wrong RK");
            assert!(lock(&store).active_vault().unwrap().is_none());
        }

        let commit = finish_create(&remote, &secrets, &store, &clock, &mut pending, &rk.to_lowercase(), None).await.unwrap();
        assert_eq!(commit.vault_id, pending.vault_id());
        assert_eq!(lock(&store).active_vault().unwrap().as_deref(), Some(pending.vault_id()));
        assert!(load_vault_key(&secrets, pending.vault_id()).unwrap().is_some());
    }

    #[test]
    fn status_and_diagnostics_carry_no_secret() {
        let dir = TempDir::new("g7-status");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let rt = runtime(&dir.0);
        let password = "app-password-SECRET-1";
        let meta = ConnectMeta {
            binding_fp: binding_fp("webdav|https://dav.example.test/remote/|alice"),
            params: ConnectParams::Webdav { base_url: "https://dav.example.test/remote/".into(), username: "alice".into() },
        };
        let store = rt.open_store().unwrap();
        save_connection(&mut lock(&store), Some(&meta)).unwrap();
        write_secret(&rt.secrets, meta.secret(), password.as_bytes()).unwrap();
        let dek = [9u8; 32];
        crate::sync::secrets::store_vault_key(&rt.secrets, VAULT, &SecretKey::from_bytes(dek)).unwrap();

        let status = serde_json::to_string(&rt.status()).unwrap();
        let diagnostics = rt.export_diagnostics();
        let dek_hex = crate::sync::crypto::hex(&dek);
        for text in [&status, &diagnostics] {
            assert!(!text.contains(password), "{text}");
            assert!(!text.contains(&meta.binding_fp), "full binding fp leaked: {text}");
            assert!(!text.contains(VAULT), "raw vault id leaked: {text}");
            assert!(!text.contains(&dek_hex), "{text}");
            assert!(!text.contains("from the store"), "memo content leaked: {text}");
            assert!(!text.contains("dev-a"), "device id leaked: {text}");
        }
        assert!(status.contains("\"account_label\":\"alice@dav.example.test\""), "{status}");
        assert!(status.contains(VaultTag::of(VAULT).as_str()));
    }

    #[test]
    fn connect_params_never_print_secrets() {
        let params: R2Params = serde_json::from_value(serde_json::json!({
            "account_id": "a", "bucket": "b", "access_key_id": "AKID", "secret_access_key": "SK-SECRET"
        }))
        .unwrap();
        let meta = ConnectMeta {
            binding_fp: "f".repeat(64),
            params: ConnectParams::R2 { account_id: params.account_id.clone(), bucket: params.bucket.clone() },
        };
        let stored = serde_json::to_string(&meta).unwrap();
        assert!(!stored.contains("SK-SECRET") && !stored.contains("AKID"), "{stored}");
        assert_eq!(meta.account_label(), "b");
    }

    #[test]
    fn oauth_identities_map_to_their_binding() {
        let drive = parse_oauth_identity("gdrive|1234").unwrap();
        assert_eq!(drive.params, ConnectParams::Gdrive { sub: "1234".into() });
        assert_eq!(drive.binding_fp, binding_fp("gdrive|1234"));
        assert!(drive.account_label().starts_with("gdrive "));
        let od = parse_oauth_identity("onedrive|tid|oid").unwrap();
        assert_eq!(od.params, ConnectParams::Onedrive { tenant_id: "tid".into(), object_id: "oid".into() });
        for bad in ["", "gdrive", "gdrive|a|b", "webdav|x", "onedrive|tid"] {
            assert_eq!(parse_oauth_identity(bad).unwrap_err().code, "oauth", "{bad:?}");
        }
    }

    // -- WebDAV probe on connect --------------------------------------------

    /// The emulated server's transport (the unprobed store is discarded).
    fn dav(server: DavServer) -> MockTransport {
        tests_support::over(server).1
    }

    fn dav_params() -> WebDavParams {
        WebDavParams {
            base_url: tests_support::BASE.to_owned(),
            username: tests_support::USER.to_owned(),
            password: tests_support::PASSWORD.to_owned(),
        }
    }

    #[tokio::test]
    async fn webdav_connect_probes_before_storing_the_password() {
        let secrets = MemoryStore::default();
        let (store, meta) = connect_webdav(&secrets, dav_params(), dav(DavServer::new())).await.unwrap();
        assert!(store.create_only(), "probed");
        assert_eq!(binding_fp(&store.identity()), meta.binding_fp);
        let stored = read_secret(&secrets, meta.secret()).unwrap().unwrap();
        assert_eq!(&stored[..], tests_support::PASSWORD.as_bytes());
        assert!(!serde_json::to_string(&meta).unwrap().contains(tests_support::PASSWORD));
        assert_eq!(meta.account_label(), "alice@dav.example.test");
    }

    #[tokio::test]
    async fn webdav_connect_refuses_a_server_without_if_none_match() {
        let secrets = MemoryStore::default();
        let mut server = DavServer::new();
        server.honour_if_none_match = false;
        let err = connect_webdav(&secrets, dav_params(), dav(server)).await.err().unwrap();
        assert_eq!(err.code, "unsupported");
        assert!(secrets.accounts().is_empty(), "no credential for a refused server");
    }

    // -- vault switch critical section (D6), early failures (D10), A8 --------

    const V2: &str = "vault-2";

    fn dav_meta() -> ConnectMeta {
        ConnectMeta {
            binding_fp: binding_fp("webdav|https://dav.example.test/|alice"),
            params: ConnectParams::Webdav { base_url: "https://dav.example.test/".into(), username: "alice".into() },
        }
    }

    /// `seed_store` (native, V1 active, memo `m1`) plus a stored connection
    /// the active vault is bound to.
    fn seed_connected(dir: &Path) -> ConnectMeta {
        save_settings(dir, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(dir, "native");
        let meta = dav_meta();
        let mut store = Store::open(&dir.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        save_connection(&mut store, Some(&meta)).unwrap();
        store.meta_set(META_VAULT_BINDING, &meta.binding_fp).unwrap();
        meta
    }

    fn memo_text(store: &Mutex<Store>, vault: &str, id: &str) -> Option<String> {
        match lock(store).projection(vault).unwrap().entities.get(&(crate::sync::ops::Kind::Memo, id.to_owned())) {
            Some(crate::sync::merge::EntityState::Memo(m)) => Some(m.text.clone()),
            _ => None,
        }
    }

    /// A stand-in for J7: V2 becomes active, holding `m1` as V1 had it.
    fn fake_commit(store: &mut Store) -> Result<vault::JoinCommit, VaultError> {
        store.init_identity(&crate::sync::identity::DeviceIdentity {
            v: 1,
            vault_id: V2.to_owned(),
            device_id: "dev-a2".to_owned(),
            install_fp: None,
            generation: 0,
        })?;
        store.set_active_vault(V2)?;
        store.insert_local_op(V2, create(50, 2_000, "m1", "from the store"), UploadState::Never)?;
        Ok(vault::JoinCommit {
            vault_id: V2.to_owned(),
            device_id: "dev-a2".to_owned(),
            ops: 1,
            blobs: 0,
            projection_hash: [0; 32],
        })
    }

    /// CB-B2: a memo edit that arrives while J7 runs waits on the sink slot
    /// and lands in the new vault (store and clipboard); V1 is never written.
    /// The switch also records the binding and restarts the scheduler.
    #[tokio::test]
    async fn a_memo_edit_racing_a_vault_switch_lands_in_the_new_vault() {
        let dir = TempDir::new("g7-switch-race");
        let meta = seed_connected(&dir.0);
        let (host, clip, log) = FakeHost::create();
        lock(&clip.state).entries = vec![entry("m1", "from the store", EntryKind::Memo)];
        let rt = runtime(&dir.0);
        rt.boot(&host, LoadOutcome::Loaded);
        assert!(lock(&clip.memo_sink).is_some(), "native boot installed the V1 sink");
        lock(&log).clear();
        let gen_before = rt.vault_gen.load(Ordering::SeqCst);

        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let worker_clip = clip.clone();
        let worker = std::thread::spawn(move || {
            go_rx.recv().unwrap();
            // What every memo command does: take the slot, then edit.
            let slot = lock(&worker_clip.memo_sink);
            clipboard::update_memo_in(&worker_clip, slot.as_deref(), "m1", "edited during the switch", 3, |_, _| {})
        });
        let switched = rt
            .switch_vault(&host, &meta.binding_fp, |store, _clock| {
                let done = fake_commit(store);
                go_tx.send(()).unwrap();
                // Let the command reach the slot while J7 still holds it.
                std::thread::sleep(Duration::from_millis(50));
                done
            })
            .await;
        let commit = switched.unwrap().unwrap();
        assert_eq!(commit.vault_id, V2);
        worker.join().unwrap().expect("the edit went through the V2 sink");

        let store = rt.store.get().unwrap();
        assert_eq!(memo_text(store, V2, "m1").as_deref(), Some("edited during the switch"), "edit is in V2");
        assert_eq!(memo_text(store, VAULT, "m1").as_deref(), Some("from the store"), "V1 untouched");
        let entries = lock(&clip.state).entries.clone();
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].id.as_str(), entries[0].text.as_str()), ("m1", "edited during the switch"));
        // Fence bumped (before the stop and at the commit), binding recorded,
        // scheduler restarted for the new vault.
        assert_eq!(rt.vault_gen.load(Ordering::SeqCst), gen_before + 2);
        assert_eq!(lock(store).meta_get(META_VAULT_BINDING).unwrap().as_deref(), Some(meta.binding_fp.as_str()));
        assert!(!rt.status().needs_vault);
        assert!(lock(&log).iter().any(|l| l == "spawn"), "scheduler restarted: {:?}", lock(&log));
        assert!(lock(&log).iter().any(|l| l == "save:1:slot_held=true"), "mirror saved inside the section: {:?}", lock(&log));
    }

    /// A refused commit changes nothing: same vault, same sink, same entries,
    /// and sync restarts on the old vault.
    #[tokio::test]
    async fn a_refused_switch_changes_nothing() {
        let dir = TempDir::new("g7-switch-refused");
        let meta = seed_connected(&dir.0);
        let (host, clip, log) = FakeHost::create();
        lock(&clip.state).entries = vec![entry("m1", "from the store", EntryKind::Memo)];
        let rt = runtime(&dir.0);
        rt.boot(&host, LoadOutcome::Loaded);
        let sink_before = lock(&clip.memo_sink).clone().unwrap();
        let entries_before = lock(&clip.state).entries.clone();
        lock(&log).clear();

        let switched = rt.switch_vault(&host, &meta.binding_fp, |_, _| Err(VaultError::NoVaultUnlocked)).await.unwrap();
        assert!(matches!(switched, Err(VaultError::NoVaultUnlocked)));
        let store = rt.store.get().unwrap();
        assert_eq!(lock(store).active_vault().unwrap().as_deref(), Some(VAULT));
        assert!(Arc::ptr_eq(&lock(&clip.memo_sink).clone().unwrap(), &sink_before), "sink not rebound");
        assert_eq!(lock(&clip.state).entries, entries_before);
        assert!(!lock(&log).iter().any(|l| l.starts_with("save")), "nothing saved: {:?}", lock(&log));
        assert!(lock(&log).iter().any(|l| l == "spawn"), "sync restarted: {:?}", lock(&log));
    }

    /// CB-B2, the other order: a command still holding the V1 sink after the
    /// switch is refused with nothing written, never committed to V1.
    #[test]
    fn a_sink_of_a_switched_away_vault_refuses() {
        let dir = TempDir::new("g7-stale-sink");
        seed_connected(&dir.0);
        let (host, clip, _) = FakeHost::create();
        lock(&clip.state).entries = vec![entry("m1", "from the store", EntryKind::Memo)];
        let rt = runtime(&dir.0);
        rt.boot(&host, LoadOutcome::Loaded);
        let old_sink = lock(&clip.memo_sink).clone().unwrap();
        let store = rt.store.get().unwrap().clone();
        fake_commit(&mut lock(&store)).unwrap();

        let before = entry("m1", "from the store", EntryKind::Memo);
        let after = entry("m1", "late edit", EntryKind::Memo);
        assert!(old_sink.changed(Some(&before), Some(&after)).is_err());
        assert!(old_sink.delete_all(&["m1".to_owned()]).is_err());
        assert_eq!(memo_text(&store, VAULT, "m1").as_deref(), Some("from the store"), "V1 untouched");
        assert_eq!(memo_text(&store, V2, "m1").as_deref(), Some("from the store"));
    }

    /// Counts writes; on the first read it bumps the vault generation, as a
    /// switch landing mid-cycle would.
    struct Bumping<'a> {
        inner: &'a FakeStore,
        gen: Option<&'a AtomicU64>,
        writes: &'a AtomicUsize,
    }

    impl Bumping<'_> {
        fn read(&self) {
            if let Some(gen) = self.gen {
                if self.writes.load(Ordering::SeqCst) == 0 {
                    gen.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        fn write(&self) {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ObjectStore for Bumping<'_> {
        fn create_only(&self) -> bool {
            self.inner.create_only()
        }

        fn identity(&self) -> String {
            self.inner.identity()
        }

        async fn list(&self, q: ListQuery<'_>) -> Result<Vec<super::super::provider::ListedObject>, ProviderError> {
            self.read();
            self.inner.list(q).await
        }

        async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
            self.read();
            self.inner.get(key).await
        }

        async fn get_all(&self, key: &str) -> Result<Vec<crate::sync::publish::RemoteObject>, ProviderError> {
            self.read();
            self.inner.get_all(key).await
        }

        async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<super::super::provider::ListedObject, ProviderError> {
            self.write();
            self.inner.put_create(key, bytes).await
        }

        async fn put_replace(
            &self,
            key: &str,
            bytes: &[u8],
            if_match: Option<&str>,
        ) -> Result<super::super::provider::ListedObject, ProviderError> {
            self.write();
            self.inner.put_replace(key, bytes, if_match).await
        }

        async fn delete(&self, key: &str) -> Result<(), ProviderError> {
            self.write();
            self.inner.delete(key).await
        }

        async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
            self.write();
            self.inner.delete_ref(provider_ref).await
        }

        async fn quota(&self) -> Result<super::super::provider::Quota, ProviderError> {
            self.inner.quota().await
        }

        async fn ensure_container(&self) -> Result<(), ProviderError> {
            self.inner.ensure_container().await
        }
    }

    /// A runtime whose store, keychain, and remote let a real engine cycle
    /// upload `m1` (identity, DEK, manifest).
    fn cycle_runtime(dir: &Path) -> (Arc<SyncRuntime<MemoryStore>>, FakeStore) {
        seed_connected(dir);
        let rt = runtime(dir);
        crate::sync::secrets::store_vault_key(&rt.secrets, VAULT, &SecretKey::from_bytes([9; 32])).unwrap();
        crate::sync::identity::save_identity(&rt.secrets, &identity("dev-a")).unwrap();
        rt.open_store().unwrap();
        let remote = FakeStore::new(true);
        remote.seed(&vault::manifest_key(VAULT).unwrap(), b"{\"manifest\":true}");
        (rt, remote)
    }

    /// A1 / ADV-4: a cycle that a vault switch superseded writes nothing more,
    /// is not recorded, and never puts its engine back; a current cycle
    /// (the control) uploads and is kept.
    #[tokio::test]
    async fn a_cycle_superseded_by_a_vault_switch_is_discarded() {
        let dir = TempDir::new("g7-stale-cycle");
        let (rt, remote) = cycle_runtime(&dir.0);
        let store = rt.store.get().unwrap().clone();
        let fp = binding_fp(&remote.identity());

        // Control: the current generation uploads and keeps its engine.
        let writes = AtomicUsize::new(0);
        let current = Bound::new(Bumping { inner: &remote, gen: None, writes: &writes }, fp.clone());
        let gen = rt.vault_gen.load(Ordering::SeqCst);
        let report = rt.cycle_with(&current, &store, VAULT, gen).await.expect("current cycle kept");
        assert_eq!(report.status, CycleStatus::Ok, "{report:?}");
        assert!(report.uploaded_segments >= 1 && writes.load(Ordering::SeqCst) >= 1, "{report:?}");
        assert!(rt.record(&report, gen));
        let kept = format!("{:?}", lock(&rt.engine));
        assert_ne!(kept, format!("{:?}", engine::Engine::default()), "the control left engine state");

        // A new local op, then a switch lands during the cycle's first read.
        lock(&store).insert_local_op(VAULT, create(2, 3_000, "m2", "second"), UploadState::Never).unwrap();
        *lock(&rt.last_status) = None;
        lock(&rt.notices).clear();
        let gen = rt.vault_gen.load(Ordering::SeqCst);
        let writes = AtomicUsize::new(0);
        let stale = Bound::new(Bumping { inner: &remote, gen: Some(&rt.vault_gen), writes: &writes }, fp);
        assert!(rt.cycle_with(&stale, &store, VAULT, gen).await.is_none(), "superseded report dropped");
        assert_eq!(writes.load(Ordering::SeqCst), 0, "no upload, record, or delete after the switch");
        assert_eq!(format!("{:?}", lock(&rt.engine)), format!("{:?}", engine::Engine::default()), "stale engine not put back");
        assert!(lock(&rt.last_status).is_none() && lock(&rt.notices).is_empty(), "nothing recorded");
        assert!(!rt.record(&report, gen), "a stale report is never recorded");
    }

    /// D10 / cleaner #2: a missing WebDAV credential stops the cycle before
    /// the engine with the reconsent status, its notice, and an auth failure.
    #[tokio::test]
    async fn a_missing_webdav_credential_reports_reconsent() {
        let dir = TempDir::new("g7-no-credential");
        seed_connected(&dir.0);
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        rt.open_store().unwrap();

        assert_eq!(rt.cycle_once(host.as_ref()).await, CycleOutcome::Stop);
        let status = rt.status();
        assert_eq!(status.last_status, Some(CycleStatus::Stopped { reason: StopReason::Reconsent }));
        assert_eq!(status.notices, vec![EngineNotice::ReconsentRequired]);
        assert_eq!(status.stats.failures.auth, 1);
        assert_eq!(status.stats.last_error, Some(FailureClass::Auth));
        assert!(status.last_cycle_at.is_some());
    }

    /// D10: an active vault of another account is a NotJoined stop with its
    /// notice (and `needs_vault`), not a silent stop.
    #[tokio::test]
    async fn a_cycle_on_a_vault_of_another_account_reports_not_joined() {
        let dir = TempDir::new("g7-no-vault");
        seed_connected(&dir.0);
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        lock(&rt.open_store().unwrap()).meta_set(META_VAULT_BINDING, "other").unwrap();

        assert_eq!(rt.cycle_once(host.as_ref()).await, CycleOutcome::Stop);
        let status = rt.status();
        assert_eq!(status.last_status, Some(CycleStatus::Stopped { reason: StopReason::NotJoined }));
        assert_eq!(status.notices, vec![EngineNotice::NotJoined]);
        assert!(status.needs_vault);
    }

    /// A8: connecting an account while a vault of another account is active
    /// does not start sync; the status asks for a vault.
    #[test]
    fn connecting_another_account_with_a_vault_active_needs_a_vault() {
        let dir = TempDir::new("g7-needs-vault");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let (host, _, log) = FakeHost::create();
        let rt = runtime(&dir.0);
        let old = dav_meta();
        lock(&rt.open_store().unwrap()).meta_set(META_VAULT_BINDING, &old.binding_fp).unwrap();
        const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
        let other = ConnectMeta {
            binding_fp: binding_fp("r2|other-account"),
            params: ConnectParams::R2 { account_id: ACCOUNT.into(), bucket: "bucket".into() },
        };
        let config = R2Config {
            account_id: ACCOUNT.into(),
            bucket: "bucket".into(),
            access_key_id: "AKID".into(),
            secret_access_key: "SK".into(),
        };
        // A real client is built, never used: adopt sends nothing.
        let transport = crate::sync::provider::transport::ReqwestTransport::new().unwrap();
        let provider = ActiveProvider::R2(Bound::new(R2Store::new(config, transport).unwrap(), other.binding_fp.clone()));
        rt.adopt(&host, other, provider).unwrap();

        assert!(lock(&rt.scheduler).is_none(), "scheduler not started");
        assert!(!lock(&log).iter().any(|l| l == "spawn"), "{:?}", lock(&log));
        let status = rt.status();
        assert!(status.needs_vault && !status.running);
        assert!(status.notices.contains(&EngineNotice::NotJoined));
    }

    /// ADV-6: a plain disconnect is refused while a cloud wipe is started.
    #[tokio::test]
    async fn a_plain_disconnect_is_refused_while_a_wipe_is_started() {
        let dir = TempDir::new("g7-wipe-started");
        let meta = seed_connected(&dir.0);
        let rt = runtime(&dir.0);
        write_secret(&rt.secrets, meta.secret(), b"pw").unwrap();
        let store = rt.open_store().unwrap();
        lock(&store).meta_set("wipe_state", "started").unwrap();

        assert_eq!(rt.disconnect(false, "").await.unwrap_err().code, "wipe_in_progress");
        assert_eq!(load_connection(&lock(&store)).unwrap(), Some(meta.clone()), "still connected");
        assert!(read_secret(&rt.secrets, meta.secret()).unwrap().is_some(), "credential kept");
    }

    #[test]
    fn dismissing_a_notice_removes_only_it() {
        let dir = TempDir::new("g7-notice");
        let rt = runtime(&dir.0);
        lock(&rt.notices).extend([EngineNotice::RateLimited, EngineNotice::QuotaExceeded]);
        assert!(rt.dismiss_notice("rate_limited"));
        assert!(!rt.dismiss_notice("rate_limited"));
        assert_eq!(lock(&rt.notices).iter().cloned().collect::<Vec<_>>(), vec![EngineNotice::QuotaExceeded]);
    }

    /// D3a: the ManifestCopies notice reaches status with its latest count
    /// only (a new count replaces the old one); a cycle without it keeps it
    /// until dismissed.
    #[test]
    fn d3a_manifest_copies_notice_surfaces_in_status_with_the_latest_count() {
        let dir = TempDir::new("g7-manifest-copies");
        let rt = runtime(&dir.0);
        let gen = rt.vault_gen.load(Ordering::SeqCst);
        let report = |notices: Vec<EngineNotice>| CycleReport {
            status: CycleStatus::Ok,
            notices,
            applied_segments: 0,
            uploaded_segments: 0,
            changed: false,
        };
        assert!(rt.record(&report(vec![EngineNotice::ManifestCopies { count: 2 }]), gen));
        assert!(rt.record(&report(vec![EngineNotice::ManifestCopies { count: 3 }]), gen));
        assert_eq!(rt.status().notices, vec![EngineNotice::ManifestCopies { count: 3 }]);
        assert!(rt.record(&report(Vec::new()), gen));
        assert_eq!(rt.status().notices, vec![EngineNotice::ManifestCopies { count: 3 }]);
        assert!(rt.dismiss_notice("manifest_copies"));
        assert!(rt.status().notices.is_empty());
    }

    // -- G8 chat load and sync:chat-changed ----------------------------------

    fn chat_log(log: &Log) -> usize {
        lock(log).iter().filter(|l| *l == "chat").count()
    }

    #[test]
    fn g8_chat_load_before_the_cutover_is_not_native() {
        for state in ["none", "backed_up", "imported", "verified"] {
            let dir = TempDir::new("g8-not-native");
            save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
            seed_store(&dir.0, state);
            let rt = runtime(&dir.0);
            assert_eq!(rt.chat_load().err().map(|e| e.code), Some("not_native"), "{state}");
            assert!(lock(&rt.chat_fp).is_none());
        }
    }

    #[test]
    fn g8_queue_apply_signals_chat_changed_and_the_status_carries_the_queue_watermarks() {
        let dir = TempDir::new("g8-queue-apply");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        Store::open(&dir.0.join(SYNC_DIR).join(STORE_FILE)).unwrap().meta_set("migration_ack_qseq", "5").unwrap();
        let (host, _, log) = FakeHost::create();
        let rt = runtime(&dir.0);
        crate::sync::secrets::store_vault_key(&rt.secrets, VAULT, &SecretKey::from_bytes([9; 32])).unwrap();

        let empty = rt.chat_load().unwrap();
        assert!(empty.conversations.is_empty() && empty.history.is_empty());
        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!(status["migration_state"], "native");
        assert_eq!(status["migration"], "native");
        assert_eq!(status["ack_qseq"], 5);
        assert!(status["queue_flushed_qseq"].is_null());
        assert_eq!(status["chat_src_fallbacks"], 0);

        let create = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "hello"});
        assert_eq!(rt.queue_apply(host.as_ref(), 6, create.clone()), Ok(QueueApply::Applied { changed: true }));
        assert_eq!(chat_log(&log), 1, "a changed apply signals once");
        assert_eq!(rt.queue_apply(host.as_ref(), 6, create), Ok(QueueApply::Duplicate), "same qseq: skipped");
        let msg = serde_json::json!({"op": "msg_add", "conv": "c1", "parent": null,
            "msg": {"id": "m1", "role": "user", "content": "hi", "ts": 7, "images": []}});
        assert_eq!(rt.queue_apply(host.as_ref(), 7, msg), Ok(QueueApply::Applied { changed: true }));
        assert_eq!(chat_log(&log), 2);
        // A no-op item (already present) flushes without a signal.
        let again = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "other"});
        assert_eq!(rt.queue_apply(host.as_ref(), 8, again), Ok(QueueApply::Applied { changed: false }));
        assert_eq!(chat_log(&log), 2);

        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!((status["queue_flushed_qseq"].as_u64(), status["ack_qseq"].as_u64()), (Some(8), Some(5)));
        let chat = serde_json::to_value(rt.chat_load().unwrap()).unwrap();
        assert_eq!(chat["conversations"][0]["id"], "c1");
        assert_eq!(chat["conversations"][0]["title"], "hello");
        assert_eq!(chat["conversations"][0]["updatedAt"], 7);
        assert_eq!(chat["conversations"][0]["messages"][0]["id"], "m1");
        assert_eq!(chat["history"], serde_json::json!([]));
        assert!(lock(&log).iter().all(|l| l == "chat"), "{:?}", lock(&log));
    }

    /// A store at the runtime's path that reached `native` with a vault key,
    /// and the settings with the flag off.
    fn seed_native_flag_off(dir: &Path, rt: &SyncRuntime<MemoryStore>) {
        save_settings(dir, SyncSettings { enabled: false, local_only: true }).unwrap();
        seed_store(dir, "native");
        Store::open(&dir.join(SYNC_DIR).join(STORE_FILE)).unwrap().meta_set("migration_ack_qseq", "2").unwrap();
        crate::sync::secrets::store_vault_key(&rt.secrets, VAULT, &SecretKey::from_bytes([9; 32])).unwrap();
    }

    #[test]
    fn e1_a_native_device_keeps_load_apply_and_status_with_the_flag_off() {
        let dir = TempDir::new("e1-native-off");
        let (host, clip, log) = FakeHost::create();
        let rt = runtime(&dir.0);
        seed_native_flag_off(&dir.0, &rt);
        assert!(!rt.enabled());

        // Boot stays network-free: no store opened, no scheduler, no sink.
        assert!(rt.boot(&host, LoadOutcome::Loaded).is_empty());
        assert!(rt.store.get().is_none());

        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["migration"], "native");
        assert_eq!(status["migration_state"], "native");
        assert_eq!(status["ack_qseq"], 2);
        assert!(status["queue_flushed_qseq"].is_null());
        assert_eq!(status["running"], false);

        assert!(rt.chat_load().unwrap().conversations.is_empty());
        let create = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "off"});
        assert_eq!(rt.queue_apply(host.as_ref(), 3, create), Ok(QueueApply::Applied { changed: true }));
        assert_eq!(chat_log(&log), 1, "sync:chat-changed after an apply with the flag off");
        let chat = serde_json::to_value(rt.chat_load().unwrap()).unwrap();
        assert_eq!(chat["conversations"][0]["title"], "off");
        assert_eq!(serde_json::to_value(rt.status()).unwrap()["queue_flushed_qseq"], 3);

        // The network stays off.
        assert!(lock(&rt.scheduler).is_none(), "no scheduler");
        assert!(lock(&rt.events).is_none(), "no scheduler channel");
        assert!(lock(&rt.provider).is_none(), "no provider");
        assert!(clip.memo_sink.lock().unwrap().is_none(), "no sink installed");
        assert!(lock(&log).iter().all(|l| l == "chat"), "no spawn, save or emit: {:?}", lock(&log));
        assert_eq!(rt.create_vault().err().map(|e| e.code), Some("disabled"));
        let discover = tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(rt.discover());
        assert_eq!(discover.err().map(|e| e.code), Some("disabled"));
        assert!(!rt.enabled());
    }

    #[test]
    fn e1_a_device_that_never_migrated_stays_disabled_with_the_flag_off() {
        // A store left by an earlier flag-on boot that never reached native.
        for state in ["none", "backed_up", "imported", "verified"] {
            let dir = TempDir::new("e1-not-native-off");
            seed_store(&dir.0, state);
            let db = dir.0.join(SYNC_DIR).join(STORE_FILE);
            let before = std::fs::read(&db).unwrap();
            let (host, _, log) = FakeHost::create();
            let rt = runtime(&dir.0);
            let create = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "x"});
            assert_eq!(rt.queue_apply(host.as_ref(), 1, create).err().map(|e| e.code), Some("disabled"), "{state}");
            assert_eq!(rt.chat_load().err().map(|e| e.code), Some("disabled"), "{state}");
            let status = serde_json::to_value(rt.status()).unwrap();
            assert_eq!((status["enabled"].as_bool(), status["migration"].as_str()), (Some(false), Some("none")), "{state}");
            assert!(status["queue_flushed_qseq"].is_null() && status["ack_qseq"].is_null());
            assert!(rt.store.get().is_none(), "{state}: the store is only peeked at");
            assert_eq!(std::fs::read(&db).unwrap(), before, "{state}: sync.db unchanged");
            assert!(lock(&log).is_empty());
        }
        // No store at all: nothing is created.
        let dir = TempDir::new("e1-no-db-off");
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("disabled"));
        assert_eq!(rt.queue_apply(host.as_ref(), 1, serde_json::json!({})).err().map(|e| e.code), Some("disabled"));
        assert_eq!(rt.status().migration, "none");
        assert!(dir_entries(&dir.0).is_empty(), "{:?}", dir_entries(&dir.0));
    }

    /// A `sync.db` that is not a SQLite database: the peek fails.
    fn garbage_store(dir: &Path) -> PathBuf {
        let db = dir.join(SYNC_DIR).join(STORE_FILE);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, vec![0x5a; 8192]).unwrap();
        db
    }

    /// A native `sync.db` from a newer build: the peek reads `native`, the
    /// open fails (`user_version` beyond this build's migrations).
    fn unopenable_native_store(dir: &Path) {
        seed_store(dir, "native");
        let conn = rusqlite::Connection::open(dir.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        conn.pragma_update(None, "user_version", 9_999).unwrap();
    }

    #[test]
    fn e1a_peek_open_or_load_failure_is_store_error_not_the_defaults() {
        let create = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "x"});
        // Peek failure, flag off.
        let dir = TempDir::new("e1a-peek-off");
        let db = garbage_store(&dir.0);
        let before = std::fs::read(&db).unwrap();
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        assert!(!rt.enabled());
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("store_error"), "peek");
        assert_eq!(rt.queue_apply(host.as_ref(), 1, create.clone()).err().map(|e| e.code), Some("store_error"), "peek");
        assert!(rt.store.get().is_none(), "nothing opened");
        assert_eq!(std::fs::read(&db).unwrap(), before, "the unreadable file is left alone");

        // Open failure, flag off: peeked as native, cannot be opened.
        let dir = TempDir::new("e1a-open-off");
        unopenable_native_store(&dir.0);
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("store_error"), "open");
        assert_eq!(rt.queue_apply(host.as_ref(), 1, create.clone()).err().map(|e| e.code), Some("store_error"), "open");

        // Load failure, flag off: an open store whose migration state no
        // longer parses.
        let dir = TempDir::new("e1a-load-off");
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        seed_native_flag_off(&dir.0, &rt);
        assert!(rt.chat_load().is_ok(), "native first");
        lock(&rt.open_store().unwrap()).meta_set("migration_state", "migrated-v9").unwrap();
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("store_error"), "load");
        assert_eq!(rt.queue_apply(host.as_ref(), 1, create).err().map(|e| e.code), Some("store_error"), "load");

        // A definite "not native" is still `disabled`.
        let dir = TempDir::new("e1a-not-native-off");
        seed_store(&dir.0, "verified");
        let rt = runtime(&dir.0);
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("disabled"));
    }

    #[test]
    fn e1a_status_carries_store_error() {
        // Flag off: peek and open failures.
        for (name, seed) in [("peek", garbage_store as fn(&Path) -> PathBuf), ("open", |d: &Path| {
            unopenable_native_store(d);
            d.join(SYNC_DIR).join(STORE_FILE)
        })] {
            let dir = TempDir::new("e1a-status-off");
            seed(&dir.0);
            let status = serde_json::to_value(runtime(&dir.0).status()).unwrap();
            assert_eq!(status["store_error"], true, "{name}");
            assert_eq!(status["enabled"], false, "{name}");
        }

        // Flag on, store not opened yet: a peek failure is not "no store".
        let dir = TempDir::new("e1a-status-on-peek");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        garbage_store(&dir.0);
        let rt = runtime(&dir.0);
        assert!(rt.enabled());
        assert_eq!(serde_json::to_value(rt.status()).unwrap()["store_error"], true);

        // Flag on, a native file not opened yet that fails to open.
        let dir = TempDir::new("e1a-status-on-open");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        unopenable_native_store(&dir.0);
        assert_eq!(serde_json::to_value(runtime(&dir.0).status()).unwrap()["store_error"], true);

        // Flag on, a native file not opened yet: reported as native.
        let dir = TempDir::new("e1a-status-on-native");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let status = serde_json::to_value(runtime(&dir.0).status()).unwrap();
        assert_eq!((status["store_error"].as_bool(), status["migration"].as_str()), (Some(false), Some("native")));

        // Flag on, an open store whose migration state does not parse.
        let dir = TempDir::new("e1a-status-on-load");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "migrated-v9");
        let rt = runtime(&dir.0);
        rt.open_store().unwrap();
        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!((status["store_error"].as_bool(), status["migration"].as_str()), (Some(true), Some("unknown")));

        // Healthy: native (flag off), never migrated, and no store at all.
        let dir = TempDir::new("e1a-status-ok");
        let rt = runtime(&dir.0);
        assert_eq!(serde_json::to_value(rt.status()).unwrap()["store_error"], false, "no sync.db");
        seed_native_flag_off(&dir.0, &rt);
        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!((status["store_error"].as_bool(), status["migration"].as_str()), (Some(false), Some("native")));
        let dir = TempDir::new("e1a-status-not-native");
        seed_store(&dir.0, "verified");
        let status = serde_json::to_value(runtime(&dir.0).status()).unwrap();
        assert_eq!((status["store_error"].as_bool(), status["migration"].as_str()), (Some(false), Some("none")));
    }

    /// E1a (cleaner #1): a `sync.db` whose stat fails for a reason other
    /// than `NotFound` is a store error, never "no store". The sync dir is
    /// made unreadable (mode 000) and restored on drop, whatever happens.
    #[cfg(unix)]
    #[test]
    fn e1a_an_unstattable_sync_db_is_store_error_not_no_store() {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let dir = TempDir::new("e1a-stat");
        seed_store(&dir.0, "native");
        let sync_dir = dir.0.join(SYNC_DIR);
        let db = sync_dir.join(STORE_FILE);
        std::fs::set_permissions(&sync_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = Restore(sync_dir.clone());
        // Root (or a filesystem ignoring modes) can still stat: nothing to test.
        if std::fs::metadata(&db).is_ok() {
            return;
        }
        assert!(Store::peek_meta(&db, "migration_state").is_err(), "the stat error is surfaced");
        let rt = runtime(&dir.0);
        assert!(!rt.enabled());
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("store_error"));
        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!(status["store_error"], true);
        assert_eq!(status["enabled"], false);
    }

    /// E1a (cleaner #1): a `sync.db` path that is not a regular file is a
    /// store error; a missing parent directory is still "no store".
    #[test]
    fn e1a_a_non_file_sync_db_is_store_error() {
        let dir = TempDir::new("e1a-nonfile");
        std::fs::create_dir_all(dir.0.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        let rt = runtime(&dir.0);
        assert_eq!(rt.chat_load().err().map(|e| e.code), Some("store_error"));
        assert_eq!(serde_json::to_value(rt.status()).unwrap()["store_error"], true);

        let dir = TempDir::new("e1a-nodir");
        assert_eq!(Store::peek_meta(&dir.0.join(SYNC_DIR).join(STORE_FILE), "migration_state").unwrap(), None);
        assert_eq!(runtime(&dir.0).chat_load().err().map(|e| e.code), Some("disabled"));
    }

    /// G8 red team (final): with the flag on, a native store that can no
    /// longer be opened or whose migration state no longer parses must never
    /// answer as "not native" (`disabled`, `not_native`, `migration_state`),
    /// which would make the webview leave native mode and drop its marker.
    #[test]
    fn e1a_redteam_flag_on_open_or_load_failure_is_never_a_definite_answer() {
        let definite = ["disabled", "not_native", "migration_state"];
        let create = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "x"});

        let dir = TempDir::new("e1a-rt-on-load");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        crate::sync::secrets::store_vault_key(&rt.secrets, VAULT, &SecretKey::from_bytes([9; 32])).unwrap();
        assert!(rt.enabled());
        assert!(rt.chat_load().is_ok(), "native first");
        lock(&rt.open_store().unwrap()).meta_set("migration_state", "migrated-v9").unwrap();
        let load = rt.chat_load().err().map(|e| e.code);
        assert!(load.is_some_and(|c| !definite.contains(&c)), "chat_load: {load:?}");
        let apply = rt.queue_apply(host.as_ref(), 1, create.clone()).err().map(|e| e.code);
        assert!(apply.is_some_and(|c| !definite.contains(&c)), "queue_apply: {apply:?}");
        let status = serde_json::to_value(rt.status()).unwrap();
        assert_eq!((status["store_error"].as_bool(), status["migration"].as_str()), (Some(true), Some("unknown")));

        let dir = TempDir::new("e1a-rt-on-open");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        unopenable_native_store(&dir.0);
        let (host, _, _) = FakeHost::create();
        let rt = runtime(&dir.0);
        assert!(rt.enabled());
        let load = rt.chat_load().err().map(|e| e.code);
        assert!(load.is_some_and(|c| !definite.contains(&c)), "chat_load: {load:?}");
        let apply = rt.queue_apply(host.as_ref(), 1, create).err().map(|e| e.code);
        assert!(apply.is_some_and(|c| !definite.contains(&c)), "queue_apply: {apply:?}");
        assert_eq!(serde_json::to_value(rt.status()).unwrap()["store_error"], true);
    }

    #[test]
    fn e3_queue_apply_tells_a_duplicate_from_a_stale_qseq() {
        let dir = TempDir::new("e3-dup-stale");
        let (host, _, log) = FakeHost::create();
        let rt = runtime(&dir.0);
        seed_native_flag_off(&dir.0, &rt);
        let a = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "a"});
        let b = serde_json::json!({"op": "conv_create", "conv": "c2", "title": "b"});
        assert_eq!(rt.queue_apply(host.as_ref(), 3, a.clone()), Ok(QueueApply::Applied { changed: true }));
        // Same qseq, same op (key order does not matter): a replay.
        let a_reordered = serde_json::json!({"title": "a", "conv": "c1", "op": "conv_create"});
        assert_eq!(rt.queue_apply(host.as_ref(), 3, a_reordered), Ok(QueueApply::Duplicate));
        // Same qseq, another op: reused, nothing written.
        assert_eq!(rt.queue_apply(host.as_ref(), 3, b.clone()), Ok(QueueApply::StaleQseq { watermark: 3 }));
        // Below the watermark with no recorded hash: stale too.
        assert_eq!(rt.queue_apply(host.as_ref(), 1, b.clone()), Ok(QueueApply::StaleQseq { watermark: 3 }));
        let chat = rt.chat_load().unwrap();
        assert_eq!(chat.conversations.len(), 1, "a stale item writes nothing");
        assert_eq!(chat_log(&log), 1);
        // Renumbered above the watermark, it applies.
        assert_eq!(rt.queue_apply(host.as_ref(), 4, b), Ok(QueueApply::Applied { changed: true }));
        assert_eq!(rt.status().queue_flushed_qseq, Some(4));
        // The wire shapes.
        assert_eq!(
            serde_json::to_value(QueueApply::Applied { changed: true }).unwrap(),
            serde_json::json!({"result": "applied", "changed": true})
        );
        assert_eq!(serde_json::to_value(QueueApply::Duplicate).unwrap(), serde_json::json!({"result": "duplicate"}));
        assert_eq!(
            serde_json::to_value(QueueApply::StaleQseq { watermark: 4 }).unwrap(),
            serde_json::json!({"result": "stale_qseq", "watermark": 4})
        );
    }

    #[test]
    fn g8_chat_changed_fires_only_when_conversations_or_translations_move() {
        use crate::sync::sim_tests::op;
        let dir = TempDir::new("g8-notify");
        save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
        seed_store(&dir.0, "native");
        let (host, _, log) = FakeHost::create();
        let rt = runtime(&dir.0);
        let store = rt.open_store().unwrap();
        rt.chat_load().unwrap();

        rt.notify_chat(host.as_ref());
        assert_eq!(chat_log(&log), 0, "nothing moved since the load");
        // A memo-only change (what a remote cycle may bring) is not a chat change.
        lock(&store).insert_local_op(VAULT, create(2, 2_000, "m2", "memo"), UploadState::Never).unwrap();
        rt.notify_chat(host.as_ref());
        assert_eq!(chat_log(&log), 0);
        // A translation arrives: one signal, and none again for the same state.
        let tid = migration::tr_entity_id("t1");
        let add = op(3, 3_000, Kind::Tr, &tid, "tr.add", serde_json::json!({"source": "a", "ts": 3, "src_id": "t1"}));
        lock(&store).insert_local_op(VAULT, add, UploadState::Never).unwrap();
        rt.notify_chat(host.as_ref());
        assert_eq!(chat_log(&log), 1);
        rt.notify_chat(host.as_ref());
        assert_eq!(chat_log(&log), 1);
        // A tombstone counts too.
        lock(&store).insert_local_op(VAULT, op(4, 4_000, Kind::Tr, &tid, "tr.delete", serde_json::json!({})), UploadState::Never).unwrap();
        rt.notify_chat(host.as_ref());
        assert_eq!(chat_log(&log), 2);
        assert_eq!(serde_json::to_value(rt.chat_load().unwrap()).unwrap()["history"], serde_json::json!([]));

        // Before `native` nothing is signalled (the webview reads localStorage).
        lock(&store).meta_set("migration_state", "verified").unwrap();
        let cid = migration::conv_entity_id("c1");
        let conv = op(5, 5_000, Kind::Conv, &cid, "create", serde_json::json!({"title": "t", "src_id": "c1"}));
        lock(&store).insert_local_op(VAULT, conv, UploadState::Never).unwrap();
        rt.notify_chat(host.as_ref());
        assert_eq!(chat_log(&log), 2);
    }

    /// G8 red team (confirm, cleaner #1): `peek_meta` tells a stat failure
    /// from `NotFound`. A stat failure (ELOOP, ENOTDIR, EACCES) is
    /// `store_error` from the peek, the local commands and status, flag off
    /// and on; nothing is created over the path. Only a definite absence
    /// (no `sync/`, an empty `sync/`, a dangling link: all `NotFound`) is
    /// "no store" (`disabled`, `store_error=false`).
    #[cfg(unix)]
    #[test]
    fn g8_confirm_peek_meta_stat_failure_vs_not_found() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let definite = ["disabled", "not_native", "migration_state"];
        let create = serde_json::json!({"op": "conv_create", "conv": "c1", "title": "x"});
        let on = |dir: &Path| save_settings(dir, SyncSettings { enabled: true, local_only: false }).unwrap();
        let stat_failure = |dir: &Path, what: &str| {
            let db = dir.join(SYNC_DIR).join(STORE_FILE);
            assert!(std::fs::metadata(&db).is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound), "{what}: a stat failure");
            assert!(Store::peek_meta(&db, "migration_state").is_err(), "{what}: peek_meta");
            assert!(MigrationState::file_is_native(&db).is_err(), "{what}: file_is_native");
            let (host, _, _) = FakeHost::create();
            let rt = runtime(dir);
            assert!(!rt.enabled());
            assert_eq!(rt.chat_load().err().map(|e| e.code), Some("store_error"), "{what}: chat_load, flag off");
            assert_eq!(rt.queue_apply(host.as_ref(), 1, create.clone()).err().map(|e| e.code), Some("store_error"), "{what}: queue_apply, flag off");
            let status = serde_json::to_value(rt.status()).unwrap();
            assert_eq!((status["store_error"].as_bool(), status["enabled"].as_bool()), (Some(true), Some(false)), "{what}: status, flag off");
            assert!(rt.store.get().is_none(), "{what}: nothing opened");
            on(dir);
            let rt = runtime(dir);
            assert!(rt.enabled());
            assert_eq!(serde_json::to_value(rt.status()).unwrap()["store_error"], true, "{what}: status, flag on");
            let load = rt.chat_load().err().map(|e| e.code);
            assert!(load.is_some_and(|c| !definite.contains(&c)), "{what}: chat_load, flag on: {load:?}");
            std::fs::remove_file(dir.join(SETTINGS_FILE)).unwrap();
        };

        // ELOOP: sync.db is a link to itself (independent of uid).
        let dir = TempDir::new("g8c-eloop");
        std::fs::create_dir_all(dir.0.join(SYNC_DIR)).unwrap();
        symlink(STORE_FILE, dir.0.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        stat_failure(&dir.0, "ELOOP");
        let meta = std::fs::symlink_metadata(dir.0.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        assert!(meta.file_type().is_symlink(), "the link is left alone");

        // ENOTDIR: a regular file where the sync dir should be.
        let dir = TempDir::new("g8c-enotdir");
        std::fs::write(dir.0.join(SYNC_DIR), b"not a dir").unwrap();
        stat_failure(&dir.0, "ENOTDIR");
        assert_eq!(std::fs::read(dir.0.join(SYNC_DIR)).unwrap(), b"not a dir");

        // EACCES: the sync dir is mode 000 (restored on drop). Root can stat.
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let dir = TempDir::new("g8c-eacces");
        seed_store(&dir.0, "native");
        let sync_dir = dir.0.join(SYNC_DIR);
        std::fs::set_permissions(&sync_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let restore = Restore(sync_dir);
        if std::fs::metadata(dir.0.join(SYNC_DIR).join(STORE_FILE)).is_err() {
            stat_failure(&dir.0, "EACCES");
        } else {
            eprintln!("g8_confirm: EACCES case not exercised (stat succeeds under mode 000)");
        }
        drop(restore);
        // Readable again: the native store answers as native.
        assert!(runtime(&dir.0).chat_load().is_ok(), "EACCES: recovered");

        // NotFound: no sync dir, an empty sync dir, a dangling link.
        let none = |dir: &Path, what: &str| {
            let db = dir.join(SYNC_DIR).join(STORE_FILE);
            assert_eq!(Store::peek_meta(&db, "migration_state").unwrap(), None, "{what}");
            let (host, _, _) = FakeHost::create();
            let rt = runtime(dir);
            assert_eq!(rt.chat_load().err().map(|e| e.code), Some("disabled"), "{what}: chat_load");
            assert_eq!(rt.queue_apply(host.as_ref(), 1, create.clone()).err().map(|e| e.code), Some("disabled"), "{what}: queue_apply");
            let status = serde_json::to_value(rt.status()).unwrap();
            assert_eq!((status["store_error"].as_bool(), status["migration"].as_str()), (Some(false), Some("none")), "{what}: status");
            on(dir);
            assert_eq!(serde_json::to_value(runtime(dir).status()).unwrap()["store_error"], false, "{what}: status, flag on");
            std::fs::remove_file(dir.join(SETTINGS_FILE)).unwrap();
        };
        let dir = TempDir::new("g8c-nodir");
        none(&dir.0, "no sync dir");
        let dir = TempDir::new("g8c-emptydir");
        std::fs::create_dir_all(dir.0.join(SYNC_DIR)).unwrap();
        none(&dir.0, "empty sync dir");
        let dir = TempDir::new("g8c-dangling");
        std::fs::create_dir_all(dir.0.join(SYNC_DIR)).unwrap();
        symlink("missing-target.db", dir.0.join(SYNC_DIR).join(STORE_FILE)).unwrap();
        none(&dir.0, "dangling link");
    }

    // -- G9 U4 devices / U5 restored ---------------------------------------

    const PEER_B: &str = "0190b000-0000-7000-8000-00000000000b";
    const PEER_C: &str = "0190c000-0000-7000-8000-00000000000c";

    fn record_key(device: &str) -> String {
        crate::sync::envelope::ObjectAddress::device_record(VAULT, INITIAL_KEY_EPOCH, device).remote_key().unwrap()
    }

    /// `devices/<device>.rec` sealed under `dek`, stating `wall` as last seen.
    fn record_bytes(dek: &SecretKey, device: &str, wall: u64) -> Vec<u8> {
        let record = engine::DeviceRecord {
            v: 1,
            device_id: device.to_owned(),
            label: "SECRET-LABEL".to_owned(),
            last_seq: 7,
            chain_head: super::super::crypto::hex(&[3; 32]),
            frontier: BTreeMap::from([("dev-a".to_owned(), 4)]),
            last_seen_hlc: crate::sync::ops::Hlc { wall_ms: wall, ctr: 0 },
            app_version: "SECRET-VERSION".to_owned(),
        };
        engine::seal_record(dek, VAULT, INITIAL_KEY_EPOCH, &record).unwrap()
    }

    async fn refresh(rt: &SyncRuntime<MemoryStore>, remote: &FakeStore) {
        let writes = AtomicUsize::new(0);
        let p = Bound::new(Bumping { inner: remote, gen: None, writes: &writes }, binding_fp(&remote.identity()));
        rt.refresh_devices(&p, VAULT, rt.vault_gen.load(Ordering::SeqCst)).await;
        assert_eq!(writes.load(Ordering::SeqCst), 0, "a device read never writes");
    }

    fn ids(status: &SyncStatus) -> Vec<(String, bool, bool)> {
        status.devices.iter().map(|d| (d.id.clone(), d.is_self, d.removed)).collect()
    }

    /// U4: the list holds this device first, then every other device whose
    /// record opens under the DEK at its own address; a forged or moved
    /// record is left out, and nothing but ids and times reaches the status.
    #[tokio::test]
    async fn devices_are_built_from_verified_records_with_self_first() {
        let dir = TempDir::new("g9-devices");
        let (rt, remote) = cycle_runtime(&dir.0);
        let dek = SecretKey::from_bytes([9; 32]);
        remote.seed(&record_key("dev-a"), &record_bytes(&dek, "dev-a", 50));
        remote.seed(&record_key(PEER_C), &record_bytes(&dek, PEER_C, 300));
        remote.seed(&record_key(PEER_B), &record_bytes(&dek, PEER_B, 200));
        // Sealed under another key: does not open.
        remote.seed(&record_key("dev-forged"), &record_bytes(&SecretKey::from_bytes([1; 32]), "dev-forged", 9));
        // A valid record copied under another device's name: the address binds it.
        remote.seed(&record_key("dev-moved"), &record_bytes(&dek, PEER_B, 9));

        assert_eq!(rt.status().devices, vec![DeviceOut {
            id: "dev-a".into(),
            short: "dev-a".into(),
            last_seen: None,
            is_self: true,
            removed: false
        }]);
        refresh(&rt, &remote).await;
        let status = rt.status();
        assert_eq!(
            ids(&status),
            vec![("dev-a".into(), true, false), (PEER_B.into(), false, false), (PEER_C.into(), false, false)]
        );
        assert_eq!(status.devices[0].last_seen, Some(50));
        assert_eq!((status.devices[1].short.as_str(), status.devices[1].last_seen), ("0190b000", Some(200)));
        assert_eq!(status.devices[2].last_seen, Some(300));
        let json = serde_json::to_string(&status).unwrap();
        for leak in ["SECRET-LABEL", "SECRET-VERSION", "dev-forged", "dev-moved", "frontier", "chain_head", VAULT] {
            assert!(!json.contains(leak), "{leak} in {json}");
        }
        let keys: Vec<String> = serde_json::to_value(&status).unwrap()["devices"][1]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["id", "is_self", "last_seen", "removed", "short"]);
    }

    /// U4: reads are throttled; a removal shows at once and forces the next
    /// read; a failed read keeps the cache; a switch (generation) empties it.
    #[tokio::test]
    async fn device_reads_are_throttled_forced_by_removal_and_fenced_by_switches() {
        let dir = TempDir::new("g9-devices-throttle");
        let (rt, remote) = cycle_runtime(&dir.0);
        let dek = SecretKey::from_bytes([9; 32]);
        remote.seed(&record_key(PEER_B), &record_bytes(&dek, PEER_B, 200));
        refresh(&rt, &remote).await;
        let after_first = remote.requests();
        assert!(after_first >= 2, "list + get");

        // Throttled: a second cycle sends nothing, even with a new record out there.
        remote.seed(&record_key(PEER_C), &record_bytes(&dek, PEER_C, 300));
        refresh(&rt, &remote).await;
        assert_eq!(remote.requests(), after_first, "throttled");
        assert_eq!(ids(&rt.status()).len(), 2);

        // Past the throttle the records are read again.
        lock(&rt.devices).as_mut().unwrap().read_at -= DEVICES_REFRESH_MS;
        refresh(&rt, &remote).await;
        assert!(remote.requests() > after_first);
        assert_eq!(
            ids(&rt.status()),
            vec![("dev-a".into(), true, false), (PEER_B.into(), false, false), (PEER_C.into(), false, false)]
        );

        // A removal: shown removed at once, and the next read is not throttled.
        rt.mark_device_removed(PEER_B);
        assert_eq!(
            ids(&rt.status()),
            vec![("dev-a".into(), true, false), (PEER_B.into(), false, true), (PEER_C.into(), false, false)]
        );
        remote.delete(&record_key(PEER_B)).await.unwrap();
        let before = remote.requests();
        refresh(&rt, &remote).await;
        assert!(remote.requests() > before, "forced");
        let status = rt.status();
        assert_eq!(
            ids(&status),
            vec![("dev-a".into(), true, false), (PEER_B.into(), false, true), (PEER_C.into(), false, false)]
        );
        assert_eq!(status.devices[1].last_seen, Some(200), "last known time kept");

        // A record that disappears between two reads is a removal too.
        remote.delete(&record_key(PEER_C)).await.unwrap();
        lock(&rt.devices).as_mut().unwrap().stale = true;
        refresh(&rt, &remote).await;
        assert!(rt.status().devices.iter().all(|d| d.is_self || d.removed));

        // A failed read keeps what was there and raises nothing.
        lock(&rt.devices).as_mut().unwrap().stale = true;
        remote.fail_next(crate::sync::provider::fake::Op::List, ProviderError::Transient("down".into()));
        let kept = ids(&rt.status());
        refresh(&rt, &remote).await;
        assert_eq!(ids(&rt.status()), kept);
        assert!(lock(&rt.devices).as_ref().unwrap().stale, "still due");
        assert!(rt.status().notices.is_empty());

        // This device's own removal marks it.
        *lock(&rt.last_status) = Some(CycleStatus::Stopped { reason: StopReason::DeviceRemoved });
        assert!(rt.status().devices[0].removed);
        *lock(&rt.last_status) = None;

        // A switch: the old generation's cache shows nothing more, and the
        // next cycle reads again at once.
        rt.vault_gen.fetch_add(1, Ordering::SeqCst);
        assert_eq!(ids(&rt.status()), vec![("dev-a".into(), true, false)]);
        let before = remote.requests();
        refresh(&rt, &remote).await;
        assert!(remote.requests() > before, "a new generation reads at once");
        assert_eq!(ids(&rt.status()), vec![("dev-a".into(), true, false)], "nothing gone under the new generation");
    }

    /// U4: a read that a switch overtook is dropped.
    #[tokio::test]
    async fn a_device_read_overtaken_by_a_switch_is_dropped() {
        let dir = TempDir::new("g9-devices-fence");
        let (rt, remote) = cycle_runtime(&dir.0);
        remote.seed(&record_key(PEER_B), &record_bytes(&SecretKey::from_bytes([9; 32]), PEER_B, 200));
        let writes = AtomicUsize::new(0);
        let p = Bound::new(
            Bumping { inner: &remote, gen: Some(&rt.vault_gen), writes: &writes },
            binding_fp(&remote.identity()),
        );
        rt.refresh_devices(&p, VAULT, rt.vault_gen.load(Ordering::SeqCst)).await;
        assert!(lock(&rt.devices).is_none());
        assert_eq!(ids(&rt.status()), vec![("dev-a".into(), true, false)]);
    }

    /// Two blind deletes from `dev-x<base>` of a memo and a translation that
    /// `dev-w<base>` created: both are kept with a restored notice in the
    /// runtime's store. `base` names fresh streams for every call.
    fn restore_two(store: &Mutex<Store>, memo: &str, tr: &str, base: u64) {
        use crate::sync::sim_tests::{delete, device, op, publish, sync_stream, Remote};
        let (wd, xd) = (format!("dev-w{base}"), format!("dev-x{base}"));
        let mut remote = Remote::new(true);
        let mut w = device(&wd);
        w.insert_local_op(VAULT, create(base, 100, memo, "SECRET-TEXT"), UploadState::Never).unwrap();
        let add = op(base + 1, 100, Kind::Tr, tr, "tr.add", serde_json::json!({"src": "SECRET-SRC", "dst": "x"}));
        w.insert_local_op(VAULT, add, UploadState::Never).unwrap();
        while publish(&mut w, &mut remote).is_some() {}
        let mut x = device(&xd);
        x.insert_local_op(VAULT, delete(base + 2, 200, memo), UploadState::Never).unwrap();
        let del = op(base + 3, 200, Kind::Tr, tr, "tr.delete", serde_json::json!({}));
        x.insert_local_op(VAULT, del, UploadState::Never).unwrap();
        while publish(&mut x, &mut remote).is_some() {}
        let mut guard = lock(store);
        sync_stream(&mut guard, &remote, &wd, &|_, _| true);
        sync_stream(&mut guard, &remote, &xd, &|_, _| true);
    }

    /// U5: restored counts per kind from the projection, no ids or text in
    /// the status; dismissing hides them across a restart, and only an
    /// entity restored later shows again.
    #[test]
    fn restored_counts_are_content_free_and_dismissal_survives_a_restart() {
        const MEMO_1: &str = "0190d000-0000-7000-8000-0000000000d1";
        const TR_1: &str = "tr-secret-id-1";
        let dir = TempDir::new("g9-restored");
        let (rt, _) = cycle_runtime(&dir.0);
        let store = rt.store.get().unwrap().clone();
        let gen = rt.vault_gen.load(Ordering::SeqCst);
        rt.refresh_restored(&store, VAULT, gen);
        assert!(rt.status().restored.is_empty());
        assert!(!rt.dismiss_notice("restored"), "nothing to dismiss");

        restore_two(&store, MEMO_1, TR_1, 10);
        rt.refresh_restored(&store, VAULT, gen);
        let status = rt.status();
        assert_eq!(
            status.restored,
            vec![RestoredOut { kind: Kind::Memo, count: 1 }, RestoredOut { kind: Kind::Tr, count: 1 }]
        );
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains(r#""restored":[{"kind":"memo","count":1},{"kind":"tr","count":1}]"#), "{json}");
        for leak in [MEMO_1, TR_1, "SECRET-TEXT", "SECRET-SRC", "dev-w", "dev-x", "0190d000"] {
            assert!(!json.contains(leak), "{leak} in {json}");
        }
        let meta = lock(&store).meta_get(&restored_meta_key(VAULT)).unwrap();
        assert!(meta.is_none(), "nothing persisted before a dismiss");

        // Stable until dismissed.
        rt.refresh_restored(&store, VAULT, gen);
        assert_eq!(rt.status().restored.len(), 2);
        assert!(rt.dismiss_notice("restored"));
        assert!(rt.status().restored.is_empty());
        assert!(!rt.dismiss_notice("restored"), "already dismissed");
        let meta = lock(&store).meta_get(&restored_meta_key(VAULT)).unwrap().unwrap();
        for leak in [MEMO_1, TR_1, "SECRET"] {
            assert!(!meta.contains(leak), "only digests persisted: {meta}");
        }
        drop(store);
        drop(rt);

        // Restart: still dismissed.
        let rt = runtime(&dir.0);
        let store = rt.open_store().unwrap();
        let gen = rt.vault_gen.load(Ordering::SeqCst);
        rt.refresh_restored(&store, VAULT, gen);
        assert!(rt.status().restored.is_empty(), "dismissal survives a restart");

        // A new restore shows, alone.
        restore_two(&store, "0190d000-0000-7000-8000-0000000000d2", "tr-secret-id-2", 20);
        rt.refresh_restored(&store, VAULT, gen);
        assert_eq!(
            rt.status().restored,
            vec![RestoredOut { kind: Kind::Memo, count: 1 }, RestoredOut { kind: Kind::Tr, count: 1 }]
        );

        // A switch: the cache of the old generation is not shown.
        rt.vault_gen.fetch_add(1, Ordering::SeqCst);
        assert!(rt.status().restored.is_empty());
    }

    /// U5: dismissed digests no longer restored are pruned; the set is capped.
    #[test]
    fn dismissed_restored_digests_are_pruned_and_capped() {
        let dir = TempDir::new("g9-restored-prune");
        let (rt, _) = cycle_runtime(&dir.0);
        let store = rt.store.get().unwrap().clone();
        let stale: BTreeSet<[u8; 32]> = (0..=255u8).map(|b| [b; 32]).collect();
        save_dismissed(&mut lock(&store), VAULT, &stale).unwrap();
        rt.refresh_restored(&store, VAULT, rt.vault_gen.load(Ordering::SeqCst));
        assert!(load_dismissed(&lock(&store), VAULT).is_empty(), "not restored any more");

        let many: BTreeSet<[u8; 32]> = (0..RESTORED_DISMISSED_CAP as u32 + 10)
            .map(|i| {
                let mut d = [0u8; 32];
                d[..4].copy_from_slice(&i.to_be_bytes());
                d
            })
            .collect();
        save_dismissed(&mut lock(&store), VAULT, &many).unwrap();
        assert_eq!(load_dismissed(&lock(&store), VAULT).len(), RESTORED_DISMISSED_CAP);
        lock(&store).meta_set(&restored_meta_key(VAULT), "not json").unwrap();
        assert!(load_dismissed(&lock(&store), VAULT).is_empty(), "unreadable reads as empty");
    }
}
