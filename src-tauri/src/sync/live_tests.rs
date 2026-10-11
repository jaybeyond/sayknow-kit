//! G10 live opt-in harness (design §B): the automatable G-EXT subset against
//! a real provider account.
//!
//! Every live test is `#[ignore]`d and also returns early unless
//! `SAYKNOW_SYNC_LIVE` names its provider (`webdav`, `r2`, `gdrive`,
//! `onedrive`; several may be given comma-separated). An ignored run without
//! the variable prints `live skipped: SAYKNOW_SYNC_LIVE unset` (visible with
//! `--nocapture`), so it is never mistaken for a real pass.
//!
//! Credentials come only from environment variables (see
//! `docs/sync-live-testing.md`); OAuth refresh tokens go into an in-memory
//! secret store, never the keychain. Each run uses fresh random vault ids
//! and deletes only those prefixes at the end. Output is one content-free
//! JSON line per check: provider, check name, outcome, timings, counts and a
//! fixed error class. Tokens, keys, URLs, and object bytes are never printed.
//!
//! The same checks run without network against the emulated adapters
//! (`self_test_*`), so the harness logic itself is exercised by the normal
//! suite.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::commands::CommandError;
use super::engine::{CycleCtx, CycleReport, CycleStatus, Engine};
use super::envelope::ObjectAddress;
use super::merge::EntityState;
use super::oauth::{self, identity_from_id_token, OAuthClientConfig, OAuthTokens, ProviderAuth};
use super::ops::{Hlc, HlcClock, Kind, OpBody};
use super::provider::gdrive::GDriveStore;
use super::provider::onedrive::OneDriveStore;
use super::provider::r2::{R2Config, R2Store};
use super::provider::transport::ReqwestTransport;
use super::provider::webdav::{WebDavConfig, WebDavStore};
use super::provider::{binding_fp, Bound, ListQuery, ObjectStore, ProviderError, RemoteKind};
use super::publish::Pause;
use super::recovery::INITIAL_KEY_EPOCH;
use super::rotation::UploadState;
use super::secrets::{write_secret, CloudProvider, MemoryStore, SyncSecret};
use super::store::{now_ms, Store};
use super::telemetry::SyncStats;
use super::vault::{
    activate_created, begin_create, begin_wipe, commit_join, confirm_create, discover, fetch_candidates,
    fetch_target, finish_wipe, preview_join, publish_created, record_discovery, run_wipe, unlock_join, Discovery,
    VaultError, WIPE_CONFIRMATION,
};

const LIVE_ENV: &str = "SAYKNOW_SYNC_LIVE";
const SKIP_LINE: &str = "live skipped: SAYKNOW_SYNC_LIVE unset";
const PROBE_DEVICE_A: &str = "live-probe-a";
const PROBE_DEVICE_B: &str = "live-probe-b";
const LIVE: Wait = Wait { live: true };

// ---------------------------------------------------------------------------
// Gating and credentials

/// `true` when `SAYKNOW_SYNC_LIVE` names `provider`. Otherwise prints why the
/// test did nothing and returns `false`.
fn live_enabled(provider: &str) -> bool {
    match std::env::var(LIVE_ENV) {
        Ok(value) if value.split(',').any(|p| p.trim() == provider) => true,
        Ok(value) if !value.trim().is_empty() => {
            println!("live skipped: SAYKNOW_SYNC_LIVE does not name {provider}");
            false
        }
        _ => {
            println!("{SKIP_LINE}");
            false
        }
    }
}

/// A required credential variable. A missing one fails the run naming only
/// the variable, never a value.
fn env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value,
        _ => panic!("live run needs {name}"),
    }
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Pauses: real sleeps live, none against the emulators

#[derive(Clone, Copy)]
struct Wait {
    live: bool,
}

impl Pause for Wait {
    async fn pause(&self, duration: Duration) {
        if self.live {
            tokio::time::sleep(duration).await;
        }
    }
}

impl Wait {
    /// How often a visibility poll may run before it gives up.
    fn attempts(self) -> usize {
        if self.live {
            60
        } else {
            5
        }
    }

    async fn step(self) {
        self.pause(Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// Content-free reporting

/// A fixed class for a provider error; the error text is never printed.
fn provider_class(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::Auth { reconsent: true } => "auth_reconsent",
        ProviderError::Auth { reconsent: false } => "auth_denied",
        ProviderError::RateLimited { .. } => "rate_limited",
        ProviderError::QuotaExceeded => "quota",
        ProviderError::NotFound => "not_found",
        ProviderError::AlreadyExists => "already_exists",
        ProviderError::PreconditionFailed => "precondition_failed",
        ProviderError::Unsupported(_) => "unsupported",
        ProviderError::BindingMismatch => "binding_mismatch",
        ProviderError::Transient(_) => "transient",
        ProviderError::Permanent(_) => "permanent",
    }
}

fn vault_class(e: VaultError) -> String {
    format!("vault:{}", CommandError::from(e).code)
}

fn pclass(e: ProviderError) -> String {
    provider_class(&e).to_owned()
}

/// The status of a cycle without any text (stop reason / failure class are
/// fixed enum labels).
fn cycle_class(report: &CycleReport) -> String {
    match serde_json::to_value(report.status) {
        Ok(v) => format!("cycle:{v}"),
        Err(_) => "cycle:unknown".to_owned(),
    }
}

type Check = Result<Value, String>;

/// One run's output. `run` is the probe vault id's random part, so every
/// line of a run (and its remote prefix `v1/live-<run>…`) can be matched.
struct Report {
    provider: &'static str,
    live: bool,
    probe_vault: String,
    failed: Vec<&'static str>,
}

impl Report {
    fn new(provider: &'static str, wait: Wait) -> Self {
        let probe_vault = format!("live-{}", uuid::Uuid::new_v4().simple());
        Report { provider, live: wait.live, probe_vault, failed: Vec::new() }
    }

    fn run_tag(&self) -> &str {
        &self.probe_vault[5..13]
    }

    fn emit(&mut self, check: &'static str, started: Instant, result: &Check) {
        let ms = started.elapsed().as_millis() as u64;
        let (provider, live, run) = (self.provider, self.live, self.run_tag().to_owned());
        let line = match result {
            Ok(metrics) => {
                json!({"live": live, "provider": provider, "run": run, "check": check, "ok": true, "ms": ms, "metrics": metrics})
            }
            Err(class) => {
                self.failed.push(check);
                json!({"live": live, "provider": provider, "run": run, "check": check, "ok": false, "ms": ms, "error": class})
            }
        };
        println!("{line}");
    }

    fn finish(self) {
        assert!(self.failed.is_empty(), "live checks failed: {:?}", self.failed);
    }
}

fn ensure(condition: bool, class: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(class.to_owned())
    }
}

// ---------------------------------------------------------------------------
// Keys and visibility

fn seg_key(vault: &str, device: &str, seq: u64) -> String {
    ObjectAddress::segment(vault, INITIAL_KEY_EPOCH, device, seq, seq).remote_key().expect("layout key")
}

fn random_bytes() -> Vec<u8> {
    uuid::Uuid::new_v4().as_bytes().to_vec()
}

async fn keys<P: ObjectStore>(remote: &P, q: ListQuery<'_>) -> Result<BTreeSet<String>, ProviderError> {
    Ok(remote.list(q).await?.into_iter().map(|o| o.key).collect())
}

fn seg_query<'a>(vault: &'a str, device: Option<&'a str>) -> ListQuery<'a> {
    ListQuery::Props { vault_id: Some(vault), kind: Some(RemoteKind::Seg), device_id: device }
}

/// Polls `q` until `done` holds for its keys. Returns (polls, first-seen ms).
async fn poll_until<P: ObjectStore>(
    remote: &P,
    wait: Wait,
    q: ListQuery<'_>,
    done: impl Fn(&BTreeSet<String>) -> bool,
) -> Result<(usize, u64), String> {
    let started = Instant::now();
    for poll in 1..=wait.attempts() {
        if done(&keys(remote, q).await.map_err(pclass)?) {
            return Ok((poll, started.elapsed().as_millis() as u64));
        }
        wait.step().await;
    }
    Err("not_visible_within_cap".to_owned())
}

// ---------------------------------------------------------------------------
// Object-level checks (probe vault, no manifest, never discovered as a vault)

async fn check_container<P: ObjectStore>(remote: &P) -> Check {
    // WebDAV: the second MKCOL of an existing collection answers 405;
    // OneDrive: the second folder create answers 409. Both must be success.
    remote.ensure_container().await.map_err(pclass)?;
    remote.ensure_container().await.map_err(pclass)?;
    Ok(json!({"calls": 2}))
}

/// Create an object and measure until a listing shows it.
async fn check_list_lag<P: ObjectStore>(remote: &P, wait: Wait, probe: &str) -> Check {
    let key = seg_key(probe, PROBE_DEVICE_A, 1);
    let put_at = Instant::now();
    remote.put_create(&key, &random_bytes()).await.map_err(pclass)?;
    let put_ms = put_at.elapsed().as_millis() as u64;
    let (polls, visible_ms) = poll_until(remote, wait, seg_query(probe, Some(PROBE_DEVICE_A)), |k| k.contains(&key)).await?;
    Ok(json!({"put_ms": put_ms, "visible_after_ms": visible_ms, "polls": polls}))
}

/// A second create of a taken name is refused and nothing is overwritten
/// (`If-None-Match: *` → 412 on WebDAV / R2, `conflictBehavior=fail` → 409
/// on OneDrive, the name pre-query on Drive).
async fn check_create_conflict<P: ObjectStore>(remote: &P, wait: Wait, probe: &str) -> Check {
    let key = seg_key(probe, PROBE_DEVICE_A, 2);
    let first = random_bytes();
    remote.put_create(&key, &first).await.map_err(pclass)?;
    // Drive's refusal is a pre-query: give the listing time to show the name.
    poll_until(remote, wait, seg_query(probe, Some(PROBE_DEVICE_A)), |k| k.contains(&key)).await?;
    let second = remote.put_create(&key, &random_bytes()).await;
    ensure(second == Err(ProviderError::AlreadyExists), "second_create_not_refused")?;
    let copies = remote.get_all(&key).await.map_err(pclass)?;
    ensure(copies.len() == 1, "copy_count")?;
    ensure(copies[0].bytes == first, "original_overwritten")?;
    Ok(json!({"second_create": "already_exists", "copies": 1}))
}

/// A replace with a stale `If-Match` is refused (412) and changes nothing;
/// Drive has no conditional replace and must say so.
async fn check_stale_replace<P: ObjectStore>(remote: &P, probe: &str) -> Check {
    let key = ObjectAddress::device_record(probe, INITIAL_KEY_EPOCH, PROBE_DEVICE_A).remote_key().expect("layout key");
    let first = random_bytes();
    remote.put_create(&key, &first).await.map_err(pclass)?;
    let stale = remote.put_replace(&key, &random_bytes(), Some("\"live-stale-etag\"")).await;
    if remote.create_only() {
        ensure(stale == Err(ProviderError::PreconditionFailed), "stale_if_match_not_refused")?;
        ensure(remote.get(&key).await.map_err(pclass)? == first, "record_changed")?;
        Ok(json!({"stale_replace": "precondition_failed"}))
    } else {
        ensure(matches!(stale, Err(ProviderError::Unsupported(_))), "conditional_replace_not_refused")?;
        Ok(json!({"stale_replace": "unsupported"}))
    }
}

/// Drive only: an unchecked create next to an existing name keeps both
/// copies (the fork / resend path relies on it, never an overwrite).
async fn check_duplicate_name<P: ObjectStore>(remote: &P, wait: Wait, probe: &str) -> Check {
    let key = seg_key(probe, PROBE_DEVICE_A, 3);
    let first = random_bytes();
    let second = random_bytes();
    remote.put_create(&key, &first).await.map_err(pclass)?;
    remote.put_create_unchecked(&key, &second).await.map_err(pclass)?;
    let started = Instant::now();
    for poll in 1..=wait.attempts() {
        let copies = remote.get_all(&key).await.map_err(pclass)?;
        if copies.len() >= 2 {
            let bytes: BTreeSet<Vec<u8>> = copies.into_iter().map(|c| c.bytes).collect();
            ensure(bytes.contains(&first) && bytes.contains(&second), "copy_overwritten")?;
            return Ok(json!({"copies": 2, "polls": poll, "visible_after_ms": started.elapsed().as_millis() as u64}));
        }
        wait.step().await;
    }
    Err("second_copy_not_visible".to_owned())
}

/// The Props query (Drive: an `appProperties` query; elsewhere the exact
/// §4.2 prefix) returns exactly the objects of the named vault / kind /
/// device.
async fn check_props_query<P: ObjectStore>(remote: &P, wait: Wait, probe: &str) -> Check {
    let key_b = seg_key(probe, PROBE_DEVICE_B, 1);
    remote.put_create(&key_b, &random_bytes()).await.map_err(pclass)?;
    poll_until(remote, wait, seg_query(probe, Some(PROBE_DEVICE_B)), |k| k.contains(&key_b)).await?;

    let only_b = keys(remote, seg_query(probe, Some(PROBE_DEVICE_B))).await.map_err(pclass)?;
    ensure(only_b == BTreeSet::from([key_b.clone()]), "device_filter")?;
    let segs = keys(remote, seg_query(probe, None)).await.map_err(pclass)?;
    ensure(segs.contains(&key_b) && segs.contains(&seg_key(probe, PROBE_DEVICE_A, 1)), "vault_kind_filter")?;
    ensure(segs.iter().all(|k| k.starts_with(&format!("v1/{probe}/log/"))), "kind_leak")?;
    let recs = keys(remote, ListQuery::Props { vault_id: Some(probe), kind: Some(RemoteKind::Rec), device_id: None })
        .await
        .map_err(pclass)?;
    ensure(recs.iter().all(|k| k.ends_with(".rec")), "rec_filter")?;
    let stranger = format!("{probe}-none");
    let none = keys(remote, seg_query(&stranger, None)).await.map_err(pclass)?;
    ensure(none.is_empty(), "foreign_vault_leak")?;
    let prefix = match remote.list(ListQuery::Prefix(&format!("v1/{probe}/"))).await {
        Ok(_) => "supported",
        Err(ProviderError::Unsupported(_)) => "unsupported",
        Err(e) => return Err(pclass(e)),
    };
    Ok(json!({"device_hits": only_b.len(), "seg_hits": segs.len(), "rec_hits": recs.len(), "prefix_list": prefix}))
}

// ---------------------------------------------------------------------------
// Two engines on the real provider

struct Device {
    store: Mutex<Store>,
    clock: Mutex<HlcClock>,
    secrets: MemoryStore,
    engine: Engine,
    stats: SyncStats,
    vault: String,
}

impl Device {
    fn new(store: Store, secrets: MemoryStore, vault: &str) -> Self {
        Device {
            store: Mutex::new(store),
            clock: Mutex::new(HlcClock::default()),
            secrets,
            engine: Engine::default(),
            stats: SyncStats::default(),
            vault: vault.to_owned(),
        }
    }

    async fn cycle<P: ObjectStore>(&mut self, remote: &Bound<P>, wait: Wait) -> CycleReport {
        let now = now_ms;
        let mut ctx = CycleCtx {
            provider: remote,
            secrets: &self.secrets,
            store: &self.store,
            clock: &self.clock,
            pause: &wait,
            now: &now,
            stats: &self.stats,
            engine: &mut self.engine,
            vault: &self.vault,
            key_epoch: INITIAL_KEY_EPOCH,
            install_fp: None,
            device_label: "live harness",
            app_version: "0.0.0-live",
            checkpoint_dir: None,
            fence: None,
        };
        Engine::cycle(&mut ctx).await
    }

    /// Cycles until one ends `Ok` (rate limits and transient failures retry).
    async fn cycle_ok<P: ObjectStore>(&mut self, remote: &Bound<P>, wait: Wait) -> Result<usize, String> {
        let mut last = String::new();
        for n in 1..=wait.attempts() {
            let report = self.cycle(remote, wait).await;
            match report.status {
                CycleStatus::Ok => return Ok(n),
                CycleStatus::Stopped { .. } => return Err(cycle_class(&report)),
                _ => last = cycle_class(&report),
            }
            wait.step().await;
        }
        Err(last)
    }

    fn local(&self, op_type: &str, entity: &str, payload: Value) -> Result<String, String> {
        let op_id = uuid::Uuid::now_v7().to_string();
        let body = OpBody {
            v: 1,
            op_id: op_id.clone(),
            origin_device: "unset".to_owned(),
            hlc: Hlc { wall_ms: now_ms(), ctr: 0 },
            ctx: Default::default(),
            obs: Vec::new(),
            kind: Kind::Memo,
            entity_id: entity.to_owned(),
            op_type: op_type.to_owned(),
            payload,
        };
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        store.insert_local_op(&self.vault, body, UploadState::Never).map_err(|_| "local_op".to_owned())?;
        Ok(op_id)
    }

    fn memo_text(&self, id: &str) -> Option<String> {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        match store.projection(&self.vault).ok()?.entities.get(&(Kind::Memo, id.to_owned())) {
            Some(EntityState::Memo(m)) => Some(m.text.clone()),
            _ => None,
        }
    }

    fn hash(&self) -> Option<[u8; 32]> {
        self.store.lock().unwrap_or_else(|p| p.into_inner()).projection(&self.vault).ok().map(|p| p.hash)
    }
}

/// Discovery until a trustworthy answer.
async fn discovery<P: ObjectStore>(remote: &P, wait: Wait) -> Result<Discovery, String> {
    for _ in 0..3 {
        match discover(remote, &wait, now_ms).await {
            Discovery::Retry { .. } => wait.step().await,
            found => return Ok(found),
        }
    }
    Err("discovery_retry".to_owned())
}

/// Device A creates a vault (discovery must find none: a dedicated, empty
/// test account), writes a memo and uploads it. Device B joins with the RK
/// through J1–J7, sees the memo, edits it and uploads; A applies the edit;
/// both projection hashes end equal. `vault_out` receives the vault id as
/// soon as it exists and `device_out` device A once it is active, so cleanup
/// and the wipe check run on every path.
async fn check_round_trip<P: ObjectStore>(
    remote: &Bound<P>,
    wait: Wait,
    fp: &str,
    vault_out: &mut Option<String>,
    device_out: &mut Option<Device>,
) -> Check {
    let discovered = discovery(remote, wait).await?;
    ensure(discovered == Discovery::None, "account_not_empty")?;

    // Device A: create (D-1 create path, RK re-entry), activate.
    let mut a_store = Store::open_in_memory().map_err(|_| "local_store")?;
    let a_secrets = MemoryStore::default();
    record_discovery(&mut a_store, &discovered, now_ms()).map_err(|_| "local_store")?;
    let mut pending = begin_create(&a_store, fp, now_ms()).map_err(vault_class)?;
    let rk = pending.recovery_key().to_display().to_string();
    confirm_create(&mut pending, &a_secrets, &rk).map_err(vault_class)?;
    let vault = pending.vault_id().to_owned();
    *vault_out = Some(vault.clone());
    publish_created(remote, &a_secrets, &pending).await.map_err(vault_class)?;
    activate_created(&mut a_store, &a_secrets, &pending, None, &mut HlcClock::default(), now_ms()).map_err(vault_class)?;
    let a = device_out.insert(Device::new(a_store, a_secrets, &vault));

    let memo = uuid::Uuid::now_v7().to_string();
    let base = a.local("create", &memo, json!({"text": "live round trip", "pinned": false}))?;
    let a_up = Instant::now();
    let a_cycles = a.cycle_ok(remote, wait).await?;
    let a_upload_ms = a_up.elapsed().as_millis() as u64;

    // Device B: discover → candidates → unlock → J2 download/apply → preview → commit.
    let join_at = Instant::now();
    let mut b_store = Store::open_in_memory().map_err(|_| "local_store")?;
    let b_secrets = MemoryStore::default();
    let mut joined = None;
    let mut join_polls = 0;
    for poll in 1..=wait.attempts() {
        join_polls = poll;
        let Discovery::Found(ids) = discovery(remote, wait).await? else {
            wait.step().await;
            continue;
        };
        if !ids.contains(&vault) {
            wait.step().await;
            continue;
        }
        let candidates = fetch_candidates(remote, std::slice::from_ref(&vault)).await.map_err(vault_class)?;
        let unlocked = unlock_join(&b_store, &b_secrets, &candidates, &rk, fp).map_err(vault_class)?;
        match fetch_target(remote, &vault).await {
            Ok(download) => {
                super::vault::apply_target(&mut b_store, &b_secrets, &unlocked, &download).map_err(vault_class)?;
                joined = Some(unlocked);
                break;
            }
            // List lag: a stream that does not yet look contiguous.
            Err(VaultError::JoinIncomplete(_)) => wait.step().await,
            Err(e) => return Err(vault_class(e)),
        }
    }
    let unlocked = joined.ok_or("join_target_not_visible")?;
    let preview = preview_join(&b_store, &b_secrets, &unlocked).map_err(vault_class)?;
    let commit = match commit_join(&mut b_store, &b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), now_ms()) {
        Ok(commit) => commit,
        Err(VaultError::PreviewStale(fresh)) => {
            commit_join(&mut b_store, &b_secrets, &unlocked, &fresh, None, &mut HlcClock::default(), now_ms())
                .map_err(vault_class)?
        }
        Err(e) => return Err(vault_class(e)),
    };
    let join_ms = join_at.elapsed().as_millis() as u64;
    let mut b = Device::new(b_store, b_secrets, &vault);
    ensure(b.memo_text(&memo).as_deref() == Some("live round trip"), "joined_device_missing_memo")?;
    let a_device = a.store.lock().unwrap_or_else(|p| p.into_inner()).device_id(&vault).map_err(|_| "local_store")?;
    ensure(commit.device_id != a_device, "same_device_id")?;

    // B edits, uploads; A applies.
    b.local("edit", &memo, json!({"text": "edited on the joined device", "base_op_id": base}))?;
    let b_cycles = b.cycle_ok(remote, wait).await?;
    let apply_at = Instant::now();
    let mut a_apply_cycles = 0;
    for n in 1..=wait.attempts() {
        a_apply_cycles = n;
        a.cycle_ok(remote, wait).await?;
        if a.memo_text(&memo).as_deref() == Some("edited on the joined device") {
            break;
        }
        wait.step().await;
    }
    ensure(a.memo_text(&memo).as_deref() == Some("edited on the joined device"), "edit_not_applied")?;
    let apply_ms = apply_at.elapsed().as_millis() as u64;
    // B sees its own edit; one more cycle on each settles records.
    b.cycle_ok(remote, wait).await?;
    a.cycle_ok(remote, wait).await?;
    ensure(a.hash().is_some() && a.hash() == b.hash(), "projection_hash_differs")?;
    Ok(json!({
        "a_upload_cycles": a_cycles,
        "a_upload_ms": a_upload_ms,
        "join_polls": join_polls,
        "join_ms": join_ms,
        "b_upload_cycles": b_cycles,
        "a_apply_cycles": a_apply_cycles,
        "edit_visible_after_ms": apply_ms,
        "hash_equal": true,
    }))
}

/// Wipe (§4.7.1) of the engine vault: afterwards no object of it is listed,
/// and the probe vault (a neighbour on the same account) is untouched.
async fn check_wipe_own_prefix<P: ObjectStore>(
    remote: &Bound<P>,
    wait: Wait,
    device: &mut Device,
    probe: &str,
) -> Check {
    let vault = device.vault.clone();
    let probe_before = keys(remote, seg_query(probe, None)).await.map_err(pclass)?;
    ensure(!probe_before.is_empty(), "probe_vault_empty_before_wipe")?;
    {
        let mut store = device.store.lock().unwrap_or_else(|p| p.into_inner());
        begin_wipe(&mut store, &vault, WIPE_CONFIRMATION).map_err(vault_class)?;
    }
    let deleted = run_wipe(remote, &wait, &vault).await.map_err(vault_class)?;
    {
        let mut store = device.store.lock().unwrap_or_else(|p| p.into_inner());
        finish_wipe(&mut store, &device.secrets, &vault).map_err(vault_class)?;
    }
    let all = ListQuery::Props { vault_id: Some(&vault), kind: None, device_id: None };
    let (polls, _) = poll_until(remote, wait, all, BTreeSet::is_empty).await.map_err(|_| "vault_objects_remain".to_owned())?;
    let probe_after = keys(remote, seg_query(probe, None)).await.map_err(pclass)?;
    ensure(probe_after == probe_before, "neighbour_changed")?;
    for key in &probe_before {
        remote.get_all(key).await.map_err(pclass).and_then(|c| ensure(!c.is_empty(), "neighbour_object_gone"))?;
    }
    Ok(json!({"deleted": deleted, "empty_after_polls": polls, "neighbour_objects": probe_before.len()}))
}

/// Deletes only this run's prefixes (idempotent) and confirms they are empty.
async fn cleanup<P: ObjectStore>(remote: &P, wait: Wait, vaults: &[String]) -> Check {
    let mut deleted = 0;
    for vault in vaults {
        deleted += run_wipe(remote, &wait, vault).await.map_err(vault_class)?;
        let all = ListQuery::Props { vault_id: Some(vault), kind: None, device_id: None };
        poll_until(remote, wait, all, BTreeSet::is_empty).await.map_err(|_| "prefix_not_empty".to_owned())?;
    }
    Ok(json!({"vaults": vaults.len(), "deleted": deleted}))
}

// ---------------------------------------------------------------------------
// The run

/// Every check for one bound store. `report` may already carry checks the
/// caller ran (the WebDAV probe).
async fn run<P: ObjectStore>(remote: Bound<P>, fp: &str, wait: Wait, mut report: Report) {
    let probe = report.probe_vault.clone();

    macro_rules! check {
        ($name:literal, $body:expr) => {{
            let started = Instant::now();
            let result: Check = $body;
            report.emit($name, started, &result);
            result
        }};
    }

    let _ = check!("container_idempotent", check_container(&remote).await);
    let _ = check!("list_visibility_lag", check_list_lag(&remote, wait, &probe).await);
    let _ = check!("create_only_conflict", check_create_conflict(&remote, wait, &probe).await);
    let _ = check!("stale_if_match_replace", check_stale_replace(&remote, &probe).await);
    if !remote.create_only() {
        let _ = check!("duplicate_name_kept", check_duplicate_name(&remote, wait, &probe).await);
    }
    let _ = check!("props_query", check_props_query(&remote, wait, &probe).await);

    let mut engine_vault: Option<String> = None;
    let mut device_a: Option<Device> = None;
    let _ = check!(
        "two_engine_round_trip",
        check_round_trip(&remote, wait, fp, &mut engine_vault, &mut device_a).await
    );
    let mut vaults = vec![probe.clone()];
    if let Some(vault) = engine_vault {
        vaults.push(vault);
        match device_a.as_mut() {
            Some(device) => {
                let _ = check!("wipe_own_prefix_only", check_wipe_own_prefix(&remote, wait, device, &probe).await);
            }
            None => {
                let result: Check = Err("round_trip_did_not_activate".to_owned());
                report.emit("wipe_own_prefix_only", Instant::now(), &result);
            }
        }
    }
    let _ = check!("cleanup_run_prefixes", cleanup(&remote, wait, &vaults).await);
    report.finish();
}

// ---------------------------------------------------------------------------
// Live providers

fn transport() -> ReqwestTransport {
    ReqwestTransport::new().map_err(ProviderError::from).expect("TLS transport")
}

/// Redeems the refresh token once for the account identity (the id_token
/// from the token endpoint, as in the app's sign-in), then hands it to the
/// same `OAuthTokens` the app uses, over an in-memory secret store.
async fn oauth_tokens(
    cfg: ProviderAuth,
    provider: CloudProvider,
    refresh_env: &str,
) -> (OAuthTokens<MemoryStore, ReqwestTransport>, String) {
    let refresh = env(refresh_env);
    let set = oauth::refresh(&transport(), &cfg, &refresh).await.unwrap_or_else(|e| panic!("token refresh: {}", provider_class(&e)));
    let id_token = set.id_token.as_ref().unwrap_or_else(|| panic!("token refresh returned no id_token (scope must include openid)"));
    let identity = identity_from_id_token(provider, id_token).unwrap_or_else(|e| panic!("id_token: {}", provider_class(&e)));
    let fp = binding_fp(&identity);
    let secrets = MemoryStore::default();
    let rotated = set.refresh_token.as_ref().map(|t| t.as_str()).unwrap_or(&refresh);
    write_secret(&secrets, SyncSecret::Credential { provider, binding_fp: &fp }, rotated.as_bytes()).expect("memory secret");
    (OAuthTokens::new(secrets, transport(), cfg, fp), identity)
}

fn client(id_env: &str, secret_env: &str) -> OAuthClientConfig {
    OAuthClientConfig { client_id: env(id_env), client_secret: env_opt(secret_env) }
}

#[tokio::test]
#[ignore = "live: set SAYKNOW_SYNC_LIVE"]
async fn live_webdav() {
    if !live_enabled("webdav") {
        return;
    }
    let config = WebDavConfig {
        base_url: env("SAYKNOW_LIVE_WEBDAV_URL"),
        username: env("SAYKNOW_LIVE_WEBDAV_USER"),
        password: env("SAYKNOW_LIVE_WEBDAV_PASS"),
    };
    let store = WebDavStore::new(config, transport()).unwrap_or_else(|e| panic!("webdav config: {}", provider_class(&e)));
    run_webdav(store, LIVE).await;
}

#[tokio::test]
#[ignore = "live: set SAYKNOW_SYNC_LIVE"]
async fn live_r2() {
    if !live_enabled("r2") {
        return;
    }
    let config = R2Config {
        account_id: env("SAYKNOW_LIVE_R2_ACCOUNT"),
        bucket: env("SAYKNOW_LIVE_R2_BUCKET"),
        access_key_id: env("SAYKNOW_LIVE_R2_KEY"),
        secret_access_key: env("SAYKNOW_LIVE_R2_SECRET"),
    };
    let store = R2Store::new(config, transport()).unwrap_or_else(|e| panic!("r2 config: {}", provider_class(&e)));
    let fp = binding_fp(&store.identity());
    run(Bound::new(store, fp.clone()), &fp, LIVE, Report::new("r2", LIVE)).await;
}

#[tokio::test]
#[ignore = "live: set SAYKNOW_SYNC_LIVE"]
async fn live_gdrive() {
    if !live_enabled("gdrive") {
        return;
    }
    let cfg = ProviderAuth::drive(client("SAYKNOW_LIVE_GDRIVE_CLIENT_ID", "SAYKNOW_LIVE_GDRIVE_CLIENT_SECRET"));
    let (tokens, identity) = oauth_tokens(cfg, CloudProvider::GoogleDrive, "SAYKNOW_LIVE_GDRIVE_REFRESH").await;
    let sub = identity.strip_prefix("gdrive|").expect("drive identity").to_owned();
    let store = GDriveStore::new(transport(), tokens, sub).unwrap_or_else(|e| panic!("drive: {}", provider_class(&e)));
    let fp = binding_fp(&store.identity());
    run(Bound::new(store, fp.clone()), &fp, LIVE, Report::new("gdrive", LIVE)).await;
}

#[tokio::test]
#[ignore = "live: set SAYKNOW_SYNC_LIVE"]
async fn live_onedrive() {
    if !live_enabled("onedrive") {
        return;
    }
    let cfg = ProviderAuth::onedrive(client("SAYKNOW_LIVE_ONEDRIVE_CLIENT_ID", "SAYKNOW_LIVE_ONEDRIVE_CLIENT_SECRET"));
    let (tokens, identity) = oauth_tokens(cfg, CloudProvider::OneDrive, "SAYKNOW_LIVE_ONEDRIVE_REFRESH").await;
    let parts: Vec<&str> = identity.split('|').collect();
    let [_, tenant, object] = parts.as_slice() else { panic!("onedrive identity") };
    let store =
        OneDriveStore::new(transport(), tokens, tenant, object).unwrap_or_else(|e| panic!("onedrive: {}", provider_class(&e)));
    let fp = binding_fp(&store.identity());
    run(Bound::new(store, fp.clone()), &fp, LIVE, Report::new("onedrive", LIVE)).await;
}

/// WebDAV runs the A4 conditional-create probe first, exactly as connect
/// does; a server that ignores `If-None-Match` stops the run there.
async fn run_webdav<T: super::provider::transport::HttpTransport>(store: WebDavStore<T>, wait: Wait) {
    let mut report = Report::new("webdav", wait);
    let started = Instant::now();
    let probed: Check = store.probe_conditional_create().await.map(|()| json!({"create_only": true})).map_err(pclass);
    report.emit("conditional_create_probe", started, &probed);
    if probed.is_err() {
        report.finish();
        return;
    }
    let fp = binding_fp(&store.identity());
    run(Bound::new(store, fp.clone()), &fp, wait, report).await;
}

// ---------------------------------------------------------------------------
// The harness itself, against the emulated adapters (no network)

mod self_test {
    use super::*;
    use crate::sync::provider::fake::FakeStore;
    use crate::sync::provider::{gdrive, onedrive, r2, webdav};

    const NO_WAIT: Wait = Wait { live: false };

    async fn run_store<P: ObjectStore>(store: P, provider: &'static str) {
        let fp = binding_fp(&store.identity());
        run(Bound::new(store, fp.clone()), &fp, NO_WAIT, Report::new(provider, NO_WAIT)).await;
    }

    #[tokio::test]
    async fn self_test_fake_create_only() {
        run_store(FakeStore::new(true), "fake").await;
    }

    #[tokio::test]
    async fn self_test_webdav_emulated() {
        let (store, _, _) = webdav::tests_support::over(webdav::tests_support::DavServer::new());
        run_webdav(store, NO_WAIT).await;
    }

    #[tokio::test]
    async fn self_test_r2_emulated() {
        let (store, _) = r2::tests_support::emulated();
        run_store(store, "r2").await;
    }

    #[tokio::test]
    async fn self_test_onedrive_emulated() {
        let (store, _) = onedrive::tests_support::emulated();
        run_store(store, "onedrive").await;
    }

    #[tokio::test]
    async fn self_test_gdrive_emulated() {
        let (store, _, _) = gdrive::tests_support::emulated();
        run_store(store, "gdrive").await;
    }

    /// A server that ignores `If-None-Match` fails the run at the probe: the
    /// harness reports a failed check rather than passing silently.
    #[tokio::test]
    #[should_panic(expected = "live checks failed")]
    async fn self_test_webdav_without_conditional_create_fails() {
        let mut server = webdav::tests_support::DavServer::new();
        server.honour_if_none_match = false;
        let (store, _, _) = webdav::tests_support::over(server);
        run_webdav(store, NO_WAIT).await;
    }

    #[test]
    fn gate_reads_only_the_named_provider() {
        // The variable is process-global; only read here, never set.
        let named = std::env::var(LIVE_ENV).ok();
        let expected = named.as_deref().is_some_and(|v| v.split(',').any(|p| p.trim() == "webdav"));
        assert_eq!(live_enabled("webdav"), expected);
    }
}
