//! G7 red team: the engine cycle, vault create / discover / join J1–J7 /
//! reissue / wipe, the AppHandle-free command helpers, content-free
//! telemetry, and the scheduler, attacked through `FakeStore` fault
//! injection, in-memory stores, and the in-memory keychain. No network and
//! no real keychain (the OAuth cancel case binds a loopback socket only).
//!
//! Tests named `*_blocker_*` reproduced a defect when written (they were
//! `#[ignore]`d until fixed); they now run with the rest and must stay green.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::clipboard::{ClipEntry, ClipboardHandle, LoadOutcome};
use crate::oauth_callback::{cancel_native, start_native_listener, OAuthCallbackState};
use crate::sync::commands::{
    check_wipe_confirmation, connect_webdav, finish_create, save_settings, settings_path, BoxFuture, CommandError,
    ConnectMeta, ConnectParams, Host, SyncRuntime, SyncSettings, WebDavParams,
};
use crate::sync::crypto::{derive_subkey, Purpose, SecretKey};
use crate::sync::engine::{
    backoff, next_delay, open_record, CycleCtx, CycleOutcome, CycleReport, CycleStatus, Decision, Engine,
    EngineNotice, SchedEvent, SchedState, StopReason, A3_NOTICE_CYCLES, BACKOFF_BASE, BACKOFF_CAP, DEBOUNCE,
    MAX_DRIVE_RESENDS, RETRY_AFTER_CAP,
};
use crate::sync::envelope::{seal, ObjectAddress};
use crate::sync::identity::DeviceIdentity;
use crate::sync::ops::{HlcClock, Kind, OpBody};
use crate::sync::provider::fake::{FakeStore, Op};
use crate::sync::provider::webdav::tests_support::{self, DavServer};
use crate::sync::provider::{binding_fp, Bound, ListQuery, ListedObject, ObjectStore, ProviderError, Quota, RemoteKind};
use crate::sync::publish::{Pause, RemoteObject};
use crate::sync::recovery::{RecoveryKey, INITIAL_KEY_EPOCH};
use crate::sync::rotation::UploadState;
use crate::sync::secrets::{read_secret, store_vault_key, write_secret, CloudProvider, MemoryStore, SyncSecret};
use crate::sync::sim_tests::{create, delete, edit, oid, TempDir, M};
use crate::sync::store::{now_ms, open_segment, PendingSegment, Store};
use crate::sync::telemetry::{capture, export_diagnostics, SyncStats};
use crate::sync::vault::{
    activate_created, apply_target, begin_create, begin_reissue, begin_wipe, commit_join, confirm_create,
    confirm_reissue, discover, discovery_retry_at, fetch_candidates, fetch_target, finish_reissue, finish_wipe,
    manifest_key, preview_join, publish_created, publish_reissue, record_discovery, remove_device, run_wipe,
    unlock_join, wipe_pending, Discovery, JoinAction, JoinPreview, RetryReason, UnlockedVault, VaultError,
    DISCOVERY_CAP, DISCOVERY_RETRY_MS, WIPE_CONFIRMATION,
};

const VAULT: &str = "vault-g7rt";
const EPOCH: u32 = 1;
const DEK: [u8; 32] = [9; 32];
const NOW: u64 = 1_700_000_000_000;
const V1: &str = "vault-one-rt";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct NoPause;

impl Pause for NoPause {
    async fn pause(&self, _: Duration) {}
}

fn binding() -> String {
    binding_fp("fake|default")
}

// ---------------------------------------------------------------------------
// A fault-injecting view over one shared FakeStore

/// What happens to the remote just before the first segment put of a cycle:
/// another device removes keys (device removal) or wipes a prefix.
enum Race {
    DeleteKeys(Vec<String>),
    WipePrefix(String),
}

#[derive(Default)]
struct Hooks {
    hidden_prefixes: Mutex<Vec<String>>,
    /// `get_all` answers empty this many more times for the key.
    get_all_misses: Mutex<BTreeMap<String, usize>>,
    /// `get` answers NotFound this many more times for the key.
    get_misses: Mutex<BTreeMap<String, usize>>,
    /// `(key, provider_ref, n)`: the copy is deleted on the n-th `get_all`.
    vanish: Mutex<Option<(String, String, usize)>>,
    get_alls: Mutex<BTreeMap<String, usize>>,
    /// A put lands but its response is lost this many more times.
    lose_put_responses: AtomicUsize,
    race_on_log_put: Mutex<Option<Race>>,
    switch_identity_on_list: AtomicBool,
    /// Every segment put fails and no copy ever shows up.
    black_hole: AtomicBool,
    puts: AtomicUsize,
    log_puts: AtomicUsize,
    /// The n-th delete (1-based) fails like a crash mid-request.
    delete_crash_at: Mutex<Option<usize>>,
    deletes: AtomicUsize,
}

impl Hooks {
    fn hide_list(&self, prefix: &str) {
        lock(&self.hidden_prefixes).push(prefix.to_owned());
    }

    fn miss_get_all(&self, key: &str, n: usize) {
        lock(&self.get_all_misses).insert(key.to_owned(), n);
    }

    fn miss_get(&self, key: &str, n: usize) {
        lock(&self.get_misses).insert(key.to_owned(), n);
    }

    fn take_miss(map: &Mutex<BTreeMap<String, usize>>, key: &str) -> bool {
        let mut map = lock(map);
        match map.get_mut(key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }

    fn holed(&self, key: &str) -> bool {
        self.black_hole.load(Ordering::SeqCst) && key.contains("/log/")
    }

    fn delete_crashes(&self) -> bool {
        let n = self.deletes.fetch_add(1, Ordering::SeqCst) + 1;
        *lock(&self.delete_crash_at) == Some(n)
    }
}

struct Lens<'a> {
    inner: &'a FakeStore,
    hooks: &'a Hooks,
}

async fn run_race(inner: &FakeStore, race: Race) {
    match race {
        Race::DeleteKeys(keys) => {
            for key in keys {
                inner.delete(&key).await.unwrap();
            }
        }
        Race::WipePrefix(prefix) => {
            for object in inner.list(ListQuery::Prefix(&prefix)).await.unwrap() {
                inner.delete_ref(&object.provider_ref).await.unwrap();
            }
        }
    }
}

impl ObjectStore for Lens<'_> {
    fn create_only(&self) -> bool {
        self.inner.create_only()
    }

    fn identity(&self) -> String {
        self.inner.identity()
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        if self.hooks.switch_identity_on_list.swap(false, Ordering::SeqCst) {
            self.inner.set_identity("fake|someone-else");
        }
        let listed = self.inner.list(q).await?;
        let hidden = lock(&self.hooks.hidden_prefixes).clone();
        Ok(listed.into_iter().filter(|o| !hidden.iter().any(|p| o.key.starts_with(p.as_str()))).collect())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        if Hooks::take_miss(&self.hooks.get_misses, key) {
            return Err(ProviderError::NotFound);
        }
        self.inner.get(key).await
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.hooks.puts.fetch_add(1, Ordering::SeqCst);
        if key.contains("/log/") {
            self.hooks.log_puts.fetch_add(1, Ordering::SeqCst);
            let race = lock(&self.hooks.race_on_log_put).take();
            if let Some(race) = race {
                run_race(self.inner, race).await;
            }
            if self.hooks.holed(key) {
                return Err(ProviderError::Transient("connection reset".to_owned()));
            }
        }
        let created = self.inner.put_create(key, bytes).await?;
        let lose = self.hooks.lose_put_responses.load(Ordering::SeqCst);
        if lose > 0 {
            self.hooks.lose_put_responses.store(lose - 1, Ordering::SeqCst);
            return Err(ProviderError::Transient("response lost".to_owned()));
        }
        Ok(created)
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        self.hooks.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_replace(key, bytes, if_match).await
    }

    async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.hooks.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_replace_ref(key, provider_ref, bytes).await
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        if self.hooks.delete_crashes() {
            return Err(ProviderError::Transient("crash".to_owned()));
        }
        self.inner.delete(key).await
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        if self.hooks.holed(key) {
            return Ok(Vec::new());
        }
        if Hooks::take_miss(&self.hooks.get_all_misses, key) {
            return Ok(Vec::new());
        }
        let call = {
            let mut counts = lock(&self.hooks.get_alls);
            let n = counts.entry(key.to_owned()).or_default();
            *n += 1;
            *n
        };
        let vanish = lock(&self.hooks.vanish).clone();
        if let Some((k, r, at)) = vanish {
            if k == key && at == call {
                self.inner.delete_ref(&r).await?;
            }
        }
        self.inner.get_all(key).await
    }

    async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
        if self.hooks.delete_crashes() {
            return Err(ProviderError::Transient("crash".to_owned()));
        }
        self.inner.delete_ref(provider_ref).await
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        self.inner.quota().await
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.inner.ensure_container().await
    }
}

fn bound<'a>(inner: &'a FakeStore, hooks: &'a Hooks) -> Bound<Lens<'a>> {
    Bound::new(Lens { inner, hooks }, binding_fp(&inner.identity()))
}

/// A remote holding a (junk) manifest for `VAULT`: the engine only checks
/// its presence.
fn remote(create_only: bool) -> FakeStore {
    let fake = FakeStore::new(create_only);
    fake.seed(&manifest_key(VAULT).unwrap(), b"{\"manifest\":true}");
    fake
}

fn seg_key_of(vault: &str, device: &str, first: u64, last: u64) -> String {
    ObjectAddress::segment(vault, EPOCH, device, first, last).remote_key().unwrap()
}

fn rec_key(vault: &str, device: &str) -> String {
    ObjectAddress::device_record(vault, EPOCH, device).remote_key().unwrap()
}

fn all_keys(fake: &FakeStore, prefix: &str) -> Vec<String> {
    let listed = if fake.create_only() {
        block(fake.list(ListQuery::Prefix("v1/")))
    } else {
        block(fake.list(ListQuery::Props { vault_id: None, kind: None, device_id: None }))
    };
    let mut keys: Vec<String> = listed.unwrap().into_iter().map(|o| o.key).filter(|k| k.starts_with(prefix)).collect();
    keys.sort();
    keys
}

/// The fake completes every future on its first poll.
fn block<F: std::future::Future>(future: F) -> F::Output {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(out) => out,
        std::task::Poll::Pending => panic!("fake store future pending"),
    }
}

// ---------------------------------------------------------------------------
// One device running the engine

struct Dev {
    store: Mutex<Store>,
    clock: Mutex<HlcClock>,
    secrets: MemoryStore,
    engine: Engine,
    stats: SyncStats,
    vault: String,
    install_fp: Option<String>,
    clock_offset_ms: u64,
}

impl Dev {
    fn new(device: &str) -> Dev {
        let secrets = MemoryStore::default();
        store_vault_key(&secrets, VAULT, &SecretKey::from_bytes(DEK)).unwrap();
        let identity =
            DeviceIdentity { v: 1, vault_id: VAULT.to_owned(), device_id: device.to_owned(), install_fp: None, generation: 0 };
        crate::sync::identity::save_identity(&secrets, &identity).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store.init_identity(&identity).unwrap();
        store.set_active_vault(VAULT).unwrap();
        Dev::from_parts(store, secrets, VAULT)
    }

    fn from_parts(store: Store, secrets: MemoryStore, vault: &str) -> Dev {
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

    async fn cycle<P: ObjectStore>(&mut self, provider: &Bound<P>) -> CycleReport {
        let offset = self.clock_offset_ms;
        let now = move || now_ms() + offset;
        let mut ctx = CycleCtx {
            provider,
            secrets: &self.secrets,
            store: &self.store,
            clock: &self.clock,
            pause: &NoPause,
            now: &now,
            stats: &self.stats,
            engine: &mut self.engine,
            vault: &self.vault,
            key_epoch: EPOCH,
            install_fp: self.install_fp.as_deref(),
            device_label: "red team device",
            app_version: "0.0.0-redteam",
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

    fn state_of(&self, n: u64) -> UploadState {
        lock(&self.store).op_state(&self.vault, &oid(n)).unwrap().unwrap().2
    }

    fn seg_key(&self) -> SecretKey {
        let dek = crate::sync::secrets::load_vault_key(&self.secrets, &self.vault).unwrap().unwrap();
        derive_subkey(&dek, &self.vault, EPOCH, Purpose::Segment)
    }

    /// Seals the unsealed own ops exactly as the engine would.
    fn seal_own(&self) -> PendingSegment {
        let key = self.seg_key();
        let vault = self.vault.clone();
        lock(&self.store)
            .seal_pending(&vault, |s, plain| {
                seal(&key, &ObjectAddress::segment(&vault, EPOCH, &s.device_id, s.first_seq, s.last_seq), plain)
            })
            .unwrap()
            .unwrap()
    }
}

fn assert_ok(report: &CycleReport) {
    assert_eq!(report.status, CycleStatus::Ok, "{report:?}");
}

fn rotated(report: &CycleReport) -> bool {
    report.notices.iter().any(|n| matches!(n, EngineNotice::Rotated { .. }))
}

// ===========================================================================
// Engine attacks (AC 4–6 via the engine, 18, 19b engine part, A2/A3/D3, AC 27/30)
// ===========================================================================

/// A crash after `attempted` was persisted but before the put left: the next
/// cycle settles by probing, resends the identical sealed bytes once, and
/// publishes; no rotation, no second copy.
#[tokio::test]
async fn e01_crash_between_attempted_and_put_resends_identical_bytes() {
    for create_only in [true, false] {
        let fake = remote(create_only);
        let hooks = Hooks::default();
        let p = bound(&fake, &hooks);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        let sealed = a.seal_own();
        lock(&a.store).mark_attempted(VAULT, "dev-a", 1).unwrap();

        let report = a.cycle(&p).await;
        assert_ok(&report);
        assert!(!rotated(&report), "{report:?}");
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)), vec![sealed.bytes], "create_only={create_only}");
        assert_eq!(a.state_of(1), UploadState::Published);
        assert_eq!(a.device(), "dev-a");
    }
}

/// A put that landed but whose response was lost (the cycle was interrupted
/// mid-upload): the next cycle settles it as ours, one copy, no rotation.
#[tokio::test]
async fn e02_cycle_interrupted_mid_upload_settles_without_duplicates() {
    for create_only in [true, false] {
        let fake = remote(create_only);
        let hooks = Hooks::default();
        hooks.lose_put_responses.store(1, Ordering::SeqCst);
        let p = bound(&fake, &hooks);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        let first = a.cycle(&p).await;
        assert!(matches!(first.status, CycleStatus::Failed { .. }), "{first:?}");
        assert_eq!(a.state_of(1), UploadState::Attempted);

        a.local(create(2, 200, "memo-2", "y"));
        let second = a.cycle(&p).await;
        assert_ok(&second);
        assert!(!rotated(&second), "{second:?}");
        assert_eq!(a.state_of(1), UploadState::Published);
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).len(), 1, "create_only={create_only}");
        assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).len(), 1);
        assert_eq!(lock(&a.store).dots_of(VAULT, &oid(1)).unwrap().len(), 1, "no restamp");
    }
}

/// Quota during a pending T2: no new uploads, no restamp, no rotation, and the
/// A3 notice after N cycles (plan-amendments line 49).
#[tokio::test]
async fn e03_quota_during_t2_pauses_rotation_and_notices() {
    let fake = remote(true);
    fake.fail_next_n(Op::PutCreate, ProviderError::QuotaExceeded, 1_000);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    let report = a.cycle(&p).await;
    assert!(matches!(report.status, CycleStatus::Failed { .. }), "{report:?}");

    a.install_fp = Some("install-after-migration".to_owned());
    a.local(create(2, 200, "memo-2", "later"));
    let mut noticed = false;
    for _ in 0..=A3_NOTICE_CYCLES {
        let report = a.cycle(&p).await;
        assert!(!rotated(&report), "{report:?}");
        noticed |= report.notices.contains(&EngineNotice::UploadOutcomeUnknown);
    }
    assert!(noticed, "A3 notice after {A3_NOTICE_CYCLES} cycles");
    assert_eq!(a.device(), "dev-a");
    assert_eq!(lock(&a.store).dots_of(VAULT, &oid(1)).unwrap().len(), 1, "no restamp");
    assert_eq!(lock(&a.store).pending_segments(VAULT, "dev-a").unwrap().len(), 1, "nothing new sealed");
    assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).is_empty());
}

/// The own log listing never catches up: uploads still settle by `get_all`,
/// nothing rotates, nothing is uploaded twice.
#[tokio::test]
async fn e04_list_that_lags_forever_never_rotates_or_duplicates() {
    for create_only in [true, false] {
        let fake = remote(create_only);
        let hooks = Hooks::default();
        hooks.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
        let p = bound(&fake, &hooks);
        let mut a = Dev::new("dev-a");
        for n in 1..=4u64 {
            a.local(create(n, n * 100, &format!("memo-{n}"), "text"));
            let report = a.cycle(&p).await;
            assert_ok(&report);
            assert!(!rotated(&report), "create_only={create_only} cycle {n}: {report:?}");
        }
        assert_eq!(a.device(), "dev-a");
        for n in 1..=4u64 {
            assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", n, n)).len(), 1, "seg {n}");
        }
    }
}

/// D3 timing variant: the unreadable neighbour vanishes on the very first
/// settle probe (not the relist): tolerated, nothing quarantined.
#[tokio::test]
async fn e05_object_vanishing_before_invalid_seen_is_tolerated() {
    let fake = remote(false);
    let key = seg_key_of(VAULT, "dev-a", 1, 1);
    fake.seed(&key, b"garbage that is not an envelope");
    let garbage_ref = fake.list(ListQuery::Prefix(&key)).await.map(|l| l[0].provider_ref.clone());
    let garbage_ref = match garbage_ref {
        Ok(r) => r,
        // Drive answers Prefix with Unsupported: find it through props.
        Err(_) => {
            fake.list(ListQuery::Props { vault_id: Some(VAULT), kind: Some(RemoteKind::Seg), device_id: Some("dev-a") })
                .await
                .unwrap()[0]
                .provider_ref
                .clone()
        }
    };
    let hooks = Hooks::default();
    hooks.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
    *lock(&hooks.vanish) = Some((key.clone(), garbage_ref, 1));
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    let report = a.cycle(&bound(&fake, &hooks)).await;
    assert_ok(&report);
    assert_eq!(report.uploaded_segments, 1);
    assert!(lock(&a.store).quarantine_rows(VAULT).unwrap().is_empty());
    assert_eq!(fake.copies(&key).len(), 1);
}

/// Manifest gone (another device wiped): read missing twice → paused with
/// zero puts; a single transient miss is not a wipe.
#[tokio::test]
async fn e06_manifest_404_pauses_without_recreating_and_one_miss_is_not_a_wipe() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    hooks.miss_get_all(&manifest_key(VAULT).unwrap(), 1);
    assert_ok(&a.cycle(&p).await);

    fake.delete(&manifest_key(VAULT).unwrap()).await.unwrap();
    let puts = hooks.puts.load(Ordering::SeqCst);
    a.local(create(2, 200, "memo-2", "y"));
    let report = a.cycle(&p).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
    assert_eq!(hooks.puts.load(Ordering::SeqCst), puts, "zero puts");
    assert!(fake.copies(&manifest_key(VAULT).unwrap()).is_empty(), "manifest not recreated");
    assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).is_empty());
}

/// AC 27 race: the vault is wiped after this cycle passed its manifest gate
/// and before its first segment put. The rest of the cycle must notice and
/// recreate nothing (fixed by D7: the manifest gate before the record put
/// stops the cycle).
///
/// Updated to D7a (`undo_created` is removed; the engine never deletes on a
/// manifest miss): the one segment whose put was already in flight when the
/// wipe landed stays as an orphan for the wiper's final sweep. Nothing else
/// is recreated: no manifest, no record, no further object.
#[tokio::test]
async fn e07_blocker_mid_cycle_wipe_recreates_objects() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&p).await);
    assert_eq!(fake.copies(&rec_key(VAULT, "dev-a")).len(), 1);

    *lock(&hooks.race_on_log_put) = Some(Race::WipePrefix(format!("v1/{VAULT}/")));
    a.local(create(2, 200, "memo-2", "y"));
    let report = a.cycle(&p).await;
    let left = all_keys(&fake, &format!("v1/{VAULT}/"));
    assert_eq!(
        left,
        vec![seg_key_of(VAULT, "dev-a", 2, 2)],
        "only the in-flight segment may remain (orphan, D7a); nothing recreated: {report:?}"
    );
    assert!(fake.copies(&manifest_key(VAULT).unwrap()).is_empty(), "manifest recreated");
    assert!(fake.copies(&rec_key(VAULT, "dev-a")).is_empty(), "record recreated");
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
    assert_ne!(report.status, CycleStatus::Ok, "{report:?}");
}

/// The own record removed (device removal): one transient 404 is re-read
/// and tolerated; a real removal stops uploads (covered by engine tests).
#[tokio::test]
async fn e08_own_record_single_404_is_rechecked_not_a_removal() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&p).await);
    hooks.miss_get(&rec_key(VAULT, "dev-a"), 1);
    a.local(create(2, 200, "memo-2", "y"));
    let report = a.cycle(&p).await;
    assert_ok(&report);
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).len(), 1);
}

/// AC 30 race: another device removes this device's record while this
/// device's cycle is between its identity step and its record step. The
/// record must not be recreated and the next cycle stops (fixed by D1: a
/// published record is only ever replaced while present).
#[tokio::test]
async fn e09_blocker_removal_racing_a_cycle_is_undone() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&p).await);

    *lock(&hooks.race_on_log_put) = Some(Race::DeleteKeys(vec![rec_key(VAULT, "dev-a")]));
    a.local(create(2, 200, "memo-2", "y"));
    let _ = a.cycle(&p).await;
    assert!(fake.copies(&rec_key(VAULT, "dev-a")).is_empty(), "removed record recreated");

    a.local(create(3, 300, "memo-3", "z"));
    let report = a.cycle(&p).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
    assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 3, 3)).is_empty(), "removed device uploaded");
}

/// The account behind the store changes mid-cycle: the next request is
/// refused before transmission and the cycle stops on BindingMismatch.
#[tokio::test]
async fn e10_binding_change_mid_cycle_stops_with_zero_transmission() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    hooks.switch_identity_on_list.store(true, Ordering::SeqCst);
    let before = fake.requests();
    let report = a.cycle(&p).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::BindingMismatch }, "{report:?}");
    assert!(report.notices.contains(&EngineNotice::BindingMismatch));
    assert_eq!(fake.requests(), before + 1, "only the request already in flight");
    assert!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).is_empty());
    assert_eq!(a.stats.snapshot().failures.binding, 1);
}

/// AlreadyExists with our own identical bytes (a resumed upload) is
/// published, never Rejected: no rotation, no quarantine.
#[tokio::test]
async fn e11_already_exists_with_identical_bytes_is_published_not_rejected() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    let sealed = a.seal_own();
    fake.seed(&seg_key_of(VAULT, "dev-a", 1, 1), &sealed.bytes);
    let report = a.cycle(&bound(&fake, &hooks)).await;
    assert_ok(&report);
    assert!(!report.notices.contains(&EngineNotice::UploadRejected), "{report:?}");
    assert!(!rotated(&report));
    assert_eq!(a.state_of(1), UploadState::Published);
    assert!(lock(&a.store).quarantine_rows(VAULT).unwrap().is_empty());
}

/// Rejected (T6) with a *garbage* object under our name on a create-only
/// store: quarantine + rotation, the foreign object untouched, the op
/// reissued under a new id and uploaded by the new identity.
#[tokio::test]
async fn e12_rejected_by_unreadable_neighbour_rotates_and_reissues() {
    let fake = remote(true);
    let key = seg_key_of(VAULT, "dev-a", 1, 1);
    fake.seed(&key, b"not an envelope at all");
    let hooks = Hooks::default();
    hooks.hide_list(&format!("v1/{VAULT}/log/dev-a/"));
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "keep me"));
    let report = a.cycle(&p).await;
    assert!(report.notices.contains(&EngineNotice::UploadRejected), "{report:?}");
    assert!(rotated(&report), "{report:?}");
    assert_ne!(a.device(), "dev-a");
    assert_eq!(fake.copies(&key), vec![b"not an envelope at all".to_vec()], "foreign object untouched");
    lock(&hooks.hidden_prefixes).clear();
    assert_ok(&a.cycle(&p).await);
    let new_id = a.device();
    let listed = fake
        .list(ListQuery::Props { vault_id: Some(VAULT), kind: Some(RemoteKind::Seg), device_id: Some(&new_id) })
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "the reissued op is uploaded by the new identity");
}

/// A2 with a restart before every cycle: the resend count is persisted, so
/// the cap holds across restarts.
#[tokio::test]
async fn e13_drive_resend_cap_survives_a_restart_every_cycle() {
    let fake = remote(false);
    let hooks = Hooks::default();
    hooks.black_hole.store(true, Ordering::SeqCst);
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    let mut capped = false;
    for _ in 0..(MAX_DRIVE_RESENDS + 4) {
        a.engine = Engine::default();
        let report = a.cycle(&p).await;
        capped |= report.notices.contains(&EngineNotice::ResendCapReached);
    }
    assert!(capped);
    assert_eq!(hooks.log_puts.load(Ordering::SeqCst) as u64, 1 + MAX_DRIVE_RESENDS, "first put plus capped resends");
    assert_eq!(a.state_of(1), UploadState::Attempted);
}

/// Snapshot over the object cap (AF-1): refused before any request, no
/// segment deleted, nothing lost.
#[tokio::test]
async fn e14_snapshot_over_the_size_cap_is_refused_and_segments_stay() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let big = "x".repeat(33 << 20);
    a.local(create(1, 100, M, &big));
    assert_ok(&a.cycle(&p).await);
    a.local(create(2, 200, "memo-2", &big));
    assert_ok(&a.cycle(&p).await);
    let puts = hooks.puts.load(Ordering::SeqCst);

    a.clock_offset_ms = crate::sync::compaction::MAX_SEGMENT_AGE_MS + 24 * 60 * 60 * 1000;
    let report = a.cycle(&p).await;
    assert!(report.notices.contains(&EngineNotice::SnapshotTooLarge), "{report:?}");
    let snaps = fake
        .list(ListQuery::Props { vault_id: Some(VAULT), kind: Some(RemoteKind::Snap), device_id: None })
        .await
        .unwrap();
    assert!(snaps.is_empty());
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).len(), 1, "no segment deleted");
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).len(), 1);
    assert_eq!(lock(&a.store).pending_segments(VAULT, "dev-a").unwrap().len(), 2);
    assert!(hooks.puts.load(Ordering::SeqCst) <= puts + 1, "at most the record put");
}

/// A segment over the cap is refused before any request; the ops stay
/// local. (Risk, reported: nothing splits it, so the chain stays blocked.)
#[tokio::test]
async fn e15_segment_over_the_size_cap_sends_nothing() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, &"y".repeat(65 << 20)));
    let report = a.cycle(&p).await;
    assert!(report.notices.contains(&EngineNotice::SegmentTooLarge), "{report:?}");
    assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0);
    assert_eq!(a.state_of(1), UploadState::Never);
    a.local(create(2, 200, "memo-2", "small"));
    let report = a.cycle(&p).await;
    assert!(report.notices.contains(&EngineNotice::SegmentTooLarge), "the chain stays blocked: {report:?}");
    assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0);
}

/// Content-free telemetry under failures: log lines, the report JSON, and
/// the diagnostics export carry no memo text, vault id, device id, or
/// provider error text.
#[tokio::test]
async fn e16_telemetry_stays_content_free_under_failures() {
    let fake = remote(true);
    fake.fail_next(Op::PutCreate, ProviderError::Transient("SENTINEL-ERR https://u:p@host/x".to_owned()));
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    capture::take();
    a.local(create(1, 100, M, "SENTINEL-MEMO-TEXT"));
    let r1 = a.cycle(&p).await;
    fake.fail_next(Op::List, ProviderError::RateLimited { retry_after: Some(Duration::from_secs(7)) });
    let r2 = a.cycle(&p).await;
    let r3 = a.cycle(&p).await;
    let lines = capture::take();
    assert!(!lines.is_empty());
    let mut text = lines.join("\n");
    for r in [&r1, &r2, &r3] {
        text.push_str(&serde_json::to_string(r).unwrap());
    }
    text.push_str(&export_diagnostics(&a.stats, Some(VAULT), &[("state", "on")]));
    for needle in ["SENTINEL-MEMO-TEXT", "SENTINEL-ERR", "u:p@", VAULT, "dev-a", M] {
        assert!(!text.contains(needle), "{needle} leaked: {text}");
    }
}

// ===========================================================================
// Scheduler attacks
// ===========================================================================

#[test]
fn s01_jitter_stays_in_half_to_full_for_hostile_rng_values() {
    for failures in [0u32, 1, 2, 9, 10, 11, 31, 32, 1_000, u32::MAX] {
        let full = BACKOFF_BASE.saturating_mul(1u32 << failures.saturating_sub(1).min(20)).min(BACKOFF_CAP);
        for r in [-1.0, -0.0, 0.0, 0.5, 0.999_999, 1.0, 2.0, f64::INFINITY, f64::NEG_INFINITY, f64::MIN_POSITIVE] {
            let d = backoff(failures, &mut || r);
            assert!(d >= full / 2 && d <= full, "{failures} {r} {d:?}");
            assert!(d <= BACKOFF_CAP && d >= BACKOFF_BASE / 2);
        }
    }
}

#[test]
fn s02_backoff_never_exceeds_30_minutes() {
    let mut s = SchedState::default();
    let mut high = || 0.999_999_9;
    for outcome in [CycleOutcome::Retry, CycleOutcome::RateLimited(None)] {
        for _ in 0..1_000 {
            assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut high), Decision::RunNow);
            let Decision::Wait(d) = next_delay(&mut s, SchedEvent::CycleDone(outcome), &mut high) else { panic!() };
            assert!(d <= BACKOFF_CAP, "{d:?}");
        }
    }
    assert_eq!(BACKOFF_CAP, Duration::from_secs(30 * 60));
}

#[test]
fn s03_retry_after_is_honoured_and_clamped() {
    let mut s = SchedState::default();
    let mut r = || 0.5;
    for (after, want) in [
        (Duration::from_secs(1), Duration::from_secs(1)),
        (Duration::from_secs(42), Duration::from_secs(42)),
        (RETRY_AFTER_CAP, RETRY_AFTER_CAP),
        (RETRY_AFTER_CAP + Duration::from_secs(1), RETRY_AFTER_CAP),
        (Duration::MAX, RETRY_AFTER_CAP),
    ] {
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut r), Decision::RunNow);
        let got = next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::RateLimited(Some(after))), &mut r);
        assert_eq!(got, Decision::Wait(want), "{after:?}");
    }
    // Observation (risk, not a blocker): `Retry-After: 0` repeated is honoured
    // literally, i.e. an immediate retry with no backoff floor.
    assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut r), Decision::RunNow);
    let zero = next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::RateLimited(Some(Duration::ZERO))), &mut r);
    assert_eq!(zero, Decision::Wait(Duration::ZERO));
}

/// A local change while a backoff or a Retry-After wait is armed must not
/// shorten it, or a busy user turns a 30 min backoff (or a 10 min
/// Retry-After) into a retry every 5 s (fixed by D2: deadline = max(armed,
/// now + DEBOUNCE)).
#[test]
fn s04_blocker_local_change_shortens_backoff_and_retry_after() {
    let mut s = SchedState::default();
    let mut r = || 0.5;
    let mut armed = Duration::ZERO;
    for _ in 0..12 {
        assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut r), Decision::RunNow);
        let Decision::Wait(d) = next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::Retry), &mut r) else { panic!() };
        armed = d;
    }
    assert!(armed >= Duration::from_secs(15 * 60), "{armed:?}");
    let change = next_delay(&mut s, SchedEvent::LocalChange, &mut r);
    assert!(!matches!(change, Decision::Wait(d) if d < armed), "backoff {armed:?} shortened to {change:?}");

    let mut s = SchedState::default();
    assert_eq!(next_delay(&mut s, SchedEvent::Timer, &mut r), Decision::RunNow);
    let after = Duration::from_secs(600);
    assert_eq!(next_delay(&mut s, SchedEvent::CycleDone(CycleOutcome::RateLimited(Some(after))), &mut r), Decision::Wait(after));
    let change = next_delay(&mut s, SchedEvent::LocalChange, &mut r);
    assert!(!matches!(change, Decision::Wait(d) if d < after), "Retry-After {after:?} shortened to {change:?}");
    assert_eq!(DEBOUNCE, Duration::from_secs(5));
}

// ===========================================================================
// Vault attacks: discovery (AC 20/21)
// ===========================================================================

enum Step {
    Ids(Vec<&'static str>),
    Fail,
    Hang,
    /// The listing takes this long (on the injected clock).
    Slow(u64, Vec<&'static str>),
}

/// A store whose manifest listings follow a script, on an injected clock.
struct Script {
    steps: Mutex<VecDeque<Step>>,
    clock: Arc<AtomicU64>,
    lists: AtomicUsize,
}

impl Script {
    fn new(steps: Vec<Step>) -> Script {
        Script { steps: Mutex::new(steps.into()), clock: Arc::new(AtomicU64::new(NOW)), lists: AtomicUsize::new(0) }
    }
}

fn manifests(ids: &[&str]) -> Vec<ListedObject> {
    ids.iter()
        .map(|id| {
            let key = manifest_key(id).unwrap();
            ListedObject { key: key.clone(), provider_ref: key, size: None, etag: None }
        })
        .collect()
}

impl ObjectStore for Script {
    fn create_only(&self) -> bool {
        true
    }

    fn identity(&self) -> String {
        "fake|default".to_owned()
    }

    async fn list(&self, _: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let step = lock(&self.steps).pop_front().unwrap_or(Step::Ids(Vec::new()));
        match step {
            Step::Ids(ids) => Ok(manifests(&ids)),
            Step::Fail => Err(ProviderError::Transient("reset".to_owned())),
            Step::Hang => std::future::pending().await,
            Step::Slow(ms, ids) => {
                self.clock.fetch_add(ms, Ordering::SeqCst);
                Ok(manifests(&ids))
            }
        }
    }

    async fn get(&self, _: &str) -> Result<Vec<u8>, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn put_create(&self, _: &str, _: &[u8]) -> Result<ListedObject, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn put_replace(&self, _: &str, _: &[u8], _: Option<&str>) -> Result<ListedObject, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn delete(&self, _: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Pause advancing the script's clock.
struct ClockPause(Arc<AtomicU64>);

impl Pause for ClockPause {
    async fn pause(&self, d: Duration) {
        self.0.fetch_add(d.as_millis() as u64, Ordering::SeqCst);
    }
}

async fn run_script(steps: Vec<Step>) -> (Discovery, usize) {
    let script = Script::new(steps);
    let clock = script.clock.clone();
    let result = discover(&script, &ClockPause(clock.clone()), move || clock.load(Ordering::SeqCst)).await;
    (result, script.lists.load(Ordering::SeqCst))
}

#[tokio::test]
async fn v01_flapping_discovery_is_retry_never_none() {
    let (result, lists) = run_script(vec![Step::Ids(vec![]), Step::Ids(vec!["vault-x"]), Step::Ids(vec![])]).await;
    assert_eq!(result, Discovery::Retry { reason: RetryReason::Inconsistent, seen: vec!["vault-x".to_owned()] });
    assert_eq!(lists, 3);
    let (result, _) = run_script(vec![Step::Ids(vec!["vault-x"]), Step::Ids(vec![]), Step::Ids(vec!["vault-x"])]).await;
    assert!(matches!(result, Discovery::Retry { reason: RetryReason::Inconsistent, .. }), "{result:?}");

    // A retry forbids creation and schedules the next try 5 min out.
    let mut store = Store::open_in_memory().unwrap();
    record_discovery(&mut store, &result, NOW).unwrap();
    assert!(matches!(begin_create(&store, &binding(), NOW), Err(VaultError::DiscoveryRequired)));
    assert_eq!(discovery_retry_at(&store).unwrap(), Some(NOW + DISCOVERY_RETRY_MS));
}

#[tokio::test]
async fn v02_errors_never_read_as_empty() {
    let (result, _) = run_script(vec![Step::Fail]).await;
    assert!(matches!(result, Discovery::Retry { reason: RetryReason::Error, .. }), "{result:?}");
    let (result, _) = run_script(vec![Step::Ids(vec![]), Step::Fail]).await;
    assert!(matches!(result, Discovery::Retry { reason: RetryReason::Error, .. }), "one empty + error: {result:?}");
    let (result, _) = run_script(vec![Step::Ids(vec!["vault-x"]), Step::Fail]).await;
    assert_eq!(result, Discovery::Retry { reason: RetryReason::Error, seen: vec!["vault-x".to_owned()] });
}

#[tokio::test]
async fn v03_manifest_appearing_between_tries_is_found() {
    let (result, _) = run_script(vec![Step::Ids(vec![]), Step::Ids(vec!["vault-x"]), Step::Ids(vec!["vault-x"])]).await;
    assert_eq!(result, Discovery::Found(vec!["vault-x".to_owned()]));
    // Union of every id seen, even one that vanished again.
    let (result, _) =
        run_script(vec![Step::Ids(vec!["vault-y"]), Step::Ids(vec!["vault-x"]), Step::Ids(vec!["vault-x"])]).await;
    assert_eq!(result, Discovery::Found(vec!["vault-x".to_owned(), "vault-y".to_owned()]));
    let (result, lists) = run_script(vec![Step::Ids(vec![]), Step::Ids(vec![])]).await;
    assert_eq!((result, lists), (Discovery::None, 2));
}

/// Slow listings on the injected clock: the 30 s cap ends in Retry(Timeout).
#[tokio::test]
async fn v04_slow_listings_hit_the_30s_cap() {
    let (result, _) = run_script(vec![Step::Slow(20_000, vec![]), Step::Slow(6_000, vec!["vault-x"])]).await;
    assert!(matches!(result, Discovery::Retry { reason: RetryReason::Timeout, .. }), "{result:?}");
    let (result, _) = run_script(vec![Step::Slow(31_000, vec![])]).await;
    assert!(matches!(result, Discovery::Retry { reason: RetryReason::Timeout, .. }), "{result:?}");
}

/// A listing that never answers (was B6; fixed by D8): each list runs under
/// a timeout of the remaining 30 s cap, so discovery ends `Retry(Timeout)`
/// instead of waiting for the 60 s transport timeout per page. Runs ~30 s.
#[tokio::test]
async fn v05_blocker_hung_listing_is_not_bounded_by_the_cap() {
    let script = Script::new(vec![Step::Hang]);
    let outcome = tokio::time::timeout(DISCOVERY_CAP + Duration::from_secs(2), discover(&script, &NoPause, now_ms)).await;
    assert!(
        matches!(outcome, Ok(Discovery::Retry { reason: RetryReason::Timeout, .. })),
        "discovery still running after the cap"
    );
}

// ===========================================================================
// Vault attacks: create, RK, join J1–J7 (AC 13, 20, N-3)
// ===========================================================================

struct Created {
    store: Store,
    secrets: MemoryStore,
    vault: String,
    rk: String,
}

async fn create_on<P: ObjectStore>(remote: &P) -> Created {
    let mut store = Store::open_in_memory().unwrap();
    let secrets = MemoryStore::default();
    let result = discover(remote, &NoPause, || NOW).await;
    assert_eq!(result, Discovery::None);
    record_discovery(&mut store, &result, NOW).unwrap();
    let mut pending = begin_create(&store, &binding(), NOW).unwrap();
    let rk = pending.recovery_key().to_display().to_string();
    confirm_create(&mut pending, &secrets, &rk).unwrap();
    publish_created(remote, &secrets, &pending).await.unwrap();
    activate_created(&mut store, &secrets, &pending, None, &mut HlcClock::default(), NOW).unwrap();
    Created { store, secrets, vault: pending.vault_id().to_owned(), rk }
}

const M_SAME: &str = "10000000-0000-4000-8000-0000000000a1";
const M_CONFLICT: &str = "10000000-0000-4000-8000-0000000000a2";
const M_MINE: &str = "10000000-0000-4000-8000-0000000000a3";
const M_GONE: &str = "10000000-0000-4000-8000-0000000000a4";
const M_DEAD_HERE: &str = "10000000-0000-4000-8000-0000000000a5";
const M_EVICT: &str = "10000000-0000-4000-8000-0000000000a6";

/// V1 device B: same / conflicting / own memos, a V1-only tombstone, a
/// tombstone of a memo V2 still holds, and an evicted memo. Op ids 1..=9.
fn v1_device() -> Store {
    let mut b = Store::open_in_memory().unwrap();
    let identity =
        DeviceIdentity { v: 1, vault_id: V1.to_owned(), device_id: "dev-b1".to_owned(), install_fp: None, generation: 0 };
    b.init_identity(&identity).unwrap();
    b.set_active_vault(V1).unwrap();
    for body in [
        create(1, 1_000, M_SAME, "same everywhere"),
        create(2, 1_001, M_CONFLICT, "B's version"),
        create(3, 1_002, M_MINE, "only on B"),
        create(4, 1_003, M_GONE, "deleted on B"),
        delete(5, 1_004, M_GONE),
        create(6, 1_005, M_DEAD_HERE, "B's copy, then deleted"),
        delete(7, 1_006, M_DEAD_HERE),
        create(8, 1_007, M_EVICT, "evicted locally"),
    ] {
        b.insert_local_op(V1, body, UploadState::Never).unwrap();
    }
    b.conn().execute("UPDATE entities SET evicted_local = 1 WHERE id = ?1", [M_EVICT]).unwrap();
    b
}

/// A creates V2 on `remote` and publishes M_SAME, M_CONFLICT (other
/// content) and M_DEAD_HERE (live) through its engine. Op ids 101.. .
async fn v2_with_a(remote: &FakeStore, hooks: &Hooks) -> (Dev, String) {
    let created = create_on(remote).await;
    let rk = created.rk.clone();
    let vault = created.vault.clone();
    let mut a = Dev::from_parts(created.store, created.secrets, &vault);
    a.local(create(101, 2_000, M_SAME, "same everywhere"));
    a.local(create(102, 2_001, M_CONFLICT, "A's version"));
    a.local(create(103, 2_002, M_DEAD_HERE, "A keeps this"));
    assert_ok(&a.cycle(&bound(remote, hooks)).await);
    (a, rk)
}

async fn join_preview(
    remote: &FakeStore,
    b: &mut Store,
    b_secrets: &MemoryStore,
    rk: &str,
) -> Result<(UnlockedVault, JoinPreview), VaultError> {
    let Discovery::Found(ids) = discover(remote, &NoPause, || NOW).await else { panic!("no vault found") };
    let candidates = fetch_candidates(remote, &ids).await?;
    let unlocked = unlock_join(b, b_secrets, &candidates, rk, &binding())?;
    let download = fetch_target(remote, &unlocked.vault_id).await?;
    apply_target(b, b_secrets, &unlocked, &download)?;
    let preview = preview_join(b, b_secrets, &unlocked)?;
    Ok((unlocked, preview))
}

fn v1_state(store: &Store) -> (String, [u8; 32], i64) {
    let ops: i64 = store.conn().query_row("SELECT COUNT(*) FROM ops WHERE vault_id = ?1", [V1], |r| r.get(0)).unwrap();
    (store.active_vault().unwrap().unwrap(), store.projection(V1).unwrap().hash, ops)
}

/// Two vaults on one account; RK_a opens only A, RK_b only B, a fresh RK
/// neither (keychain untouched), and a copy of B's manifest planted under
/// A's name (Drive duplicate) neither opens nor makes A ambiguous.
#[tokio::test]
async fn v06_two_vaults_each_rk_opens_exactly_its_own() {
    let remote_a = FakeStore::new(false);
    let a = create_on(&remote_a).await;
    let remote_b = FakeStore::new(false);
    let b = create_on(&remote_b).await;
    let b_manifest = remote_b.copies(&manifest_key(&b.vault).unwrap()).remove(0);
    remote_a.seed(&manifest_key(&b.vault).unwrap(), &b_manifest);
    remote_a.seed(&manifest_key(&a.vault).unwrap(), &b_manifest); // planted under A's name

    let Discovery::Found(ids) = discover(&remote_a, &NoPause, || NOW).await else { panic!() };
    assert_eq!(ids.len(), 2);
    let candidates = fetch_candidates(&remote_a, &ids).await.unwrap();
    assert_eq!(candidates.len(), 3);
    for (rk, want) in [(&a.rk, &a.vault), (&b.rk, &b.vault)] {
        let store = Store::open_in_memory().unwrap();
        let secrets = MemoryStore::default();
        let unlocked = unlock_join(&store, &secrets, &candidates, rk, &binding()).unwrap();
        assert_eq!(&unlocked.vault_id, want);
        assert_eq!(secrets.accounts().len(), 1, "only the opened vault's DEK");
    }
    let store = Store::open_in_memory().unwrap();
    let secrets = MemoryStore::default();
    let stranger = RecoveryKey::generate().unwrap().to_display().to_string();
    let err = unlock_join(&store, &secrets, &candidates, &stranger, &binding()).err().unwrap();
    assert!(matches!(err, VaultError::NoVaultUnlocked), "{err:?}");
    let err = unlock_join(&store, &secrets, &candidates, &a.rk, &binding_fp("fake|other")).err().unwrap();
    // Observation (UX, not a safety defect): which code a binding mismatch
    // reports depends on the random vault-id order — the first candidate's
    // error wins, so a WrongKey from the other vault can mask it as rk_wrong.
    let code = CommandError::from(err).code;
    assert!(["binding_mismatch", "rk_wrong"].contains(&code), "{code}");
    assert!(secrets.accounts().is_empty(), "keychain untouched");
}

/// J1 crash then retry, J2 download fault then retry, J2 apply twice (crash
/// mid-apply and resume): V1 unchanged until J7, the preview is stable, the
/// commit succeeds once, and a second commit is AlreadyMember.
#[tokio::test]
async fn v07_crash_at_each_join_step_leaves_v1_unchanged_and_resumes() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (_a, rk) = v2_with_a(&remote, &hooks).await;
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let before = v1_state(&b);

    // J1, then the app dies.
    let Discovery::Found(ids) = discover(&remote, &NoPause, || NOW).await else { panic!() };
    let candidates = fetch_candidates(&remote, &ids).await.unwrap();
    let unlocked = unlock_join(&b, &b_secrets, &candidates, &rk, &binding()).unwrap();
    assert_eq!(v1_state(&b), before);
    // J2 download fails at the listing, then at a segment fetch.
    remote.fail_next(Op::List, ProviderError::Transient("reset".to_owned()));
    assert!(fetch_target(&remote, &unlocked.vault_id).await.is_err());
    remote.fail_next(Op::Get, ProviderError::Transient("reset".to_owned()));
    assert!(fetch_target(&remote, &unlocked.vault_id).await.is_err());
    assert_eq!(v1_state(&b), before);
    // J2 apply twice (crash after the first apply).
    let download = fetch_target(&remote, &unlocked.vault_id).await.unwrap();
    apply_target(&mut b, &b_secrets, &unlocked, &download).unwrap();
    assert_eq!(v1_state(&b), before);
    let (unlocked, preview) = join_preview(&remote, &mut b, &b_secrets, &rk).await.unwrap();
    let again = preview_join(&b, &b_secrets, &unlocked).unwrap();
    assert_eq!(preview.fingerprint, again.fingerprint);
    assert_eq!(v1_state(&b), before);

    let commit = commit_join(&mut b, &b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).unwrap();
    assert_ne!(commit.device_id, "dev-b1");
    let err = commit_join(&mut b, &b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).err().unwrap();
    assert!(matches!(err, VaultError::AlreadyMember), "{err:?}");
}

/// N-3 byte scan after the joined device's engine uploads to V2: decrypted
/// segments, the device record, and every V2-scoped local row carry no V1
/// op id, V1 device id, or V1 vault id; V1 tombstones never reappear, a V2
/// memo V1 had deleted stays live with V2's content, the evicted memo is
/// kept (no delete op), and reader A applies B's stream without holds.
#[tokio::test]
async fn v08_join_byte_scan_tombstones_and_eviction() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (mut a, rk) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let (unlocked, preview) = join_preview(&remote, &mut b, &b_secrets, &rk).await.unwrap();
    let action = |id: &str| preview.items.iter().find(|i| i.source_id == id).map(|i| (i.action, i.evicted));
    assert_eq!(action(M_SAME).map(|a| a.0), Some(JoinAction::Skip));
    assert_eq!(action(M_CONFLICT).map(|a| a.0), Some(JoinAction::Copy));
    assert_eq!(action(M_MINE).map(|a| a.0), Some(JoinAction::Republish));
    assert_eq!(action(M_GONE), None, "a V1 tombstone is never republished");
    assert_eq!(action(M_DEAD_HERE), None);
    assert_eq!(action(M_EVICT), Some((JoinAction::Republish, true)));
    let commit = commit_join(&mut b, &b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW).unwrap();

    let mut bdev = Dev::from_parts(b, b_secrets, &v2);
    let report = bdev.cycle(&bound(&remote, &hooks)).await;
    assert_ok(&report);
    let new_id = commit.device_id.clone();
    let v1_ids: Vec<String> = (1..=8).map(oid).collect();
    let mut needles: Vec<String> = v1_ids.clone();
    needles.extend(["dev-b1".to_owned(), V1.to_owned()]);

    // Remote: decrypt B's segments and record.
    let seg_key = bdev.seg_key();
    let listed = remote
        .list(ListQuery::Props { vault_id: Some(&v2), kind: Some(RemoteKind::Seg), device_id: Some(&new_id) })
        .await
        .unwrap();
    assert!(!listed.is_empty());
    let mut plain = Vec::new();
    for object in &listed {
        let address = ObjectAddress::from_remote_key(&object.key, INITIAL_KEY_EPOCH).unwrap();
        for copy in remote.copies(&object.key) {
            let segment = open_segment(&seg_key, &address, &copy).unwrap();
            assert_eq!(address.device_id, new_id);
            plain.extend(segment.encode().unwrap());
        }
    }
    let dek = crate::sync::secrets::load_vault_key(&bdev.secrets, &v2).unwrap().unwrap();
    let rec = remote.copies(&rec_key(&v2, &new_id)).remove(0);
    plain.extend(serde_json::to_vec(&open_record(&dek, &v2, EPOCH, &new_id, &rec).unwrap()).unwrap());
    // Local: every V2-scoped row.
    {
        let store = lock(&bdev.store);
        for table in ["vault_identity", "ops", "op_dots", "forks", "memo_lineage", "cursors", "remote_index", "quarantine"] {
            let sql = format!("SELECT * FROM {table} WHERE vault_id = ?1");
            let mut stmt = store.conn().prepare(&sql).unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([&v2], |r| {
                    (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}"))).collect::<Result<Vec<_>, _>>()
                })
                .unwrap();
            for row in rows {
                plain.extend(row.unwrap().join("|").into_bytes());
            }
        }
        let deletes: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM ops WHERE vault_id = ?1 AND op_type = 'delete'", [&v2], |r| r.get(0))
            .unwrap();
        assert_eq!(deletes, 0, "no delete op from a join (tombstones, eviction)");
    }
    for needle in &needles {
        assert!(!plain.windows(needle.len()).any(|w| w == needle.as_bytes()), "{needle} leaked into V2");
    }

    // Projection on B and on reader A.
    assert_ok(&a.cycle(&bound(&remote, &hooks)).await);
    for dev in [&bdev, &a] {
        let projection = lock(&dev.store).projection(&v2).unwrap();
        let live = |id: &str| {
            projection
                .entities
                .get(&(Kind::Memo, id.to_owned()))
                .filter(|s| !matches!(s, crate::sync::merge::EntityState::Deleted))
                .cloned()
        };
        assert!(live(M_MINE).is_some() && live(M_EVICT).is_some() && live(M_SAME).is_some());
        assert!(live(M_GONE).is_none(), "V1 tombstone target reappeared");
        match live(M_DEAD_HERE) {
            Some(crate::sync::merge::EntityState::Memo(m)) => assert_eq!(m.text, "A keeps this"),
            other => panic!("V2's memo must stay as V2 has it: {other:?}"),
        }
        assert!(lock(&dev.store).quarantine_rows(&v2).unwrap().is_empty());
    }
    assert_eq!(lock(&a.store).projection(&v2).unwrap().hash, lock(&bdev.store).projection(&v2).unwrap().hash);
}

/// A V2 device that compacted (snapshot + deleted segments, §4.5).
async fn compacted_v2(remote: &FakeStore, hooks: &Hooks) -> (Dev, String) {
    let (mut a, rk) = v2_with_a(remote, hooks).await;
    a.local(edit(104, 2_100, M_SAME, 101, "edited in V2"));
    assert_ok(&a.cycle(&bound(remote, hooks)).await);
    a.clock_offset_ms = crate::sync::compaction::MAX_SEGMENT_AGE_MS + 24 * 60 * 60 * 1000;
    assert_ok(&a.cycle(&bound(remote, hooks)).await);
    let v2 = a.vault.clone();
    assert!(all_keys(remote, &format!("v1/{v2}/log/")).is_empty(), "precondition: every segment compacted");
    assert!(!all_keys(remote, &format!("v1/{v2}/snap/")).is_empty(), "precondition: a snapshot");
    (a, rk)
}

/// AC 20 / N-3 after 30 days (was B3; fixed by D4): a fully compacted V2 must
/// be read through its snapshot, so memos V2 holds under the same id (one
/// edited in V2, one with other content) are planned as `Copy`, never as a
/// `Republish` of the joiner's V1 content under V2's ids.
#[tokio::test]
async fn v09_blocker_join_into_a_compacted_vault_republishes_same_ids() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (_a, rk) = compacted_v2(&remote, &hooks).await;
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    match join_preview(&remote, &mut b, &b_secrets, &rk).await {
        Err(e) => panic!("fails closed instead of joining: {e:?}"),
        Ok((_, preview)) => {
            let action = |id: &str| preview.items.iter().find(|i| i.source_id == id).map(|i| i.action);
            assert_eq!(action(M_CONFLICT), Some(JoinAction::Copy), "V2 holds other content: {:?}", preview.items);
            assert_eq!(action(M_SAME), Some(JoinAction::Copy), "V2 edited it: {:?}", preview.items);
        }
    }
}

/// Same root cause, other shape (was B4; fixed by D4): once the compacting
/// device writes again (a fresh segment above seq 1), a new device still
/// joins through the snapshot plus the tail.
#[tokio::test]
async fn v09b_blocker_join_after_compaction_and_new_writes_is_impossible() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (mut a, rk) = compacted_v2(&remote, &hooks).await;
    a.clock_offset_ms = 0;
    a.local(create(105, 2_200, "10000000-0000-4000-8000-0000000000b1", "after compaction"));
    assert_ok(&a.cycle(&bound(&remote, &hooks)).await);
    let v2 = a.vault.clone();
    assert!(!all_keys(&remote, &format!("v1/{v2}/log/")).is_empty(), "precondition: a segment above seq 1");
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let joined = join_preview(&remote, &mut b, &b_secrets, &rk).await;
    assert!(joined.is_ok(), "join refused: {:?}", joined.err());
}

/// AC 30: after a reissue the old RK no longer opens new joins, the new one
/// does, and an existing device keeps syncing (no revocation).
#[tokio::test]
async fn v10_reissue_blocks_only_new_joins_with_the_old_rk() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (mut a, old_rk) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let pending = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let new_rk = pending.recovery_key().to_display().to_string();
    // Wrong re-entry changes nothing.
    assert!(confirm_reissue(&mut lock(&a.store), &pending, &old_rk).is_err());
    assert!(crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().is_none());
    confirm_reissue(&mut lock(&a.store), &pending, &new_rk).unwrap();
    let (vault, bytes) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();
    let status = publish_reissue(&remote, &a.secrets, &vault, &bytes, &binding()).await.unwrap();
    assert!(status.not_revocation);
    finish_reissue(&mut lock(&a.store)).unwrap();

    let opens = |rk: String| {
        let remote = &remote;
        async move {
            let store = Store::open_in_memory().unwrap();
            let secrets = MemoryStore::default();
            let Discovery::Found(ids) = discover(remote, &NoPause, || NOW).await else { panic!() };
            let candidates = fetch_candidates(remote, &ids).await.unwrap();
            unlock_join(&store, &secrets, &candidates, &rk, &binding()).is_ok()
        }
    };
    assert!(!opens(old_rk).await, "old RK still joins");
    assert!(opens(new_rk).await);
    a.local(create(110, 3_000, "10000000-0000-4000-8000-0000000000c1", "after reissue"));
    assert_ok(&a.cycle(&bound(&remote, &hooks)).await);
}

/// Two devices reissue concurrently (was B7; fixed by D3): A publishes RK_a
/// and finishes; B's publish must not delete A's manifest, so RK_a, confirmed
/// and shown to the user, keeps opening the vault.
#[tokio::test]
async fn v11_blocker_concurrent_reissue_clobbers_a_confirmed_rk() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (a, _) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let b_secrets = MemoryStore::default();
    for account in a.secrets.accounts() {
        b_secrets.put_raw(&account, &a.secrets.raw(&account).unwrap());
    }
    let mut b_store = Store::open_in_memory().unwrap();

    let pending_a = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let pending_b = begin_reissue(&remote, &b_secrets, &v2, &binding()).await.unwrap();
    let rk_a = pending_a.recovery_key().to_display().to_string();
    let rk_b = pending_b.recovery_key().to_display().to_string();
    confirm_reissue(&mut lock(&a.store), &pending_a, &rk_a).unwrap();
    confirm_reissue(&mut b_store, &pending_b, &rk_b).unwrap();
    let (_, bytes_a) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();
    let (_, bytes_b) = crate::sync::vault::reissue_pending(&b_store).unwrap().unwrap();
    publish_reissue(&remote, &a.secrets, &v2, &bytes_a, &binding()).await.unwrap();
    finish_reissue(&mut lock(&a.store)).unwrap();
    let _ = publish_reissue(&remote, &b_secrets, &v2, &bytes_b, &binding()).await;

    let store = Store::open_in_memory().unwrap();
    let secrets = MemoryStore::default();
    let candidates = fetch_candidates(&remote, std::slice::from_ref(&v2)).await.unwrap();
    assert!(unlock_join(&store, &secrets, &candidates, &rk_a, &binding()).is_ok(), "A's confirmed RK was clobbered");
}

#[tokio::test]
async fn v12_device_removal_refuses_self_and_bad_ids() {
    let fake = remote(true);
    for (device, want) in [("dev-a", "self_removal"), ("../x", "invalid_id"), ("", "invalid_id")] {
        let err = remove_device(&fake, VAULT, "dev-a", device).await.err().unwrap();
        assert_eq!(CommandError::from(err).code, want, "{device:?}");
    }
    fake.seed(&rec_key(VAULT, "dev-b"), b"rec");
    fake.seed(&rec_key(VAULT, "dev-bb"), b"rec");
    remove_device(&fake, VAULT, "dev-a", "dev-b").await.unwrap();
    assert!(fake.copies(&rec_key(VAULT, "dev-b")).is_empty());
    assert_eq!(fake.copies(&rec_key(VAULT, "dev-bb")).len(), 1, "only that record");
}

// ===========================================================================
// Vault attacks: wipe (AC 26/27)
// ===========================================================================

const WIPED: &str = "vault-a";
/// Shares the prefix text `vault-a` but is another vault.
const NEIGHBOURS: [&str; 2] = ["vault-ab", "vault-a.b"];

fn wipe_fixture(create_only: bool) -> FakeStore {
    let fake = FakeStore::new(create_only);
    for vault in std::iter::once(WIPED).chain(NEIGHBOURS) {
        fake.seed(&manifest_key(vault).unwrap(), b"manifest");
        fake.seed(&seg_key_of(vault, "dev-x", 1, 1), b"seg");
        fake.seed(&seg_key_of(vault, "dev-x", 2, 3), b"seg");
        fake.seed(&rec_key(vault, "dev-x"), b"rec");
    }
    fake.seed(&seg_key_of(WIPED, "dev-x", 1, 1), b"seg drive duplicate");
    fake
}

fn neighbours_intact(fake: &FakeStore) {
    for vault in NEIGHBOURS {
        assert_eq!(all_keys(fake, &format!("v1/{vault}/")).len(), 4, "{vault} touched");
    }
}

/// A crash at every delete of the wipe (each step), then a resume: the wipe
/// completes, only the wiped prefix (Drive: its vault_id props) is gone,
/// state and keychain end consistent, and finish is idempotent.
#[tokio::test]
async fn v13_wipe_crash_at_each_step_then_resume_touches_only_its_vault() {
    for create_only in [true, false] {
        for crash_at in 1..=8usize {
            let fake = wipe_fixture(create_only);
            let hooks = Hooks::default();
            let lens = Lens { inner: &fake, hooks: &hooks };
            let mut store = Store::open_in_memory().unwrap();
            let secrets = MemoryStore::default();
            store_vault_key(&secrets, WIPED, &SecretKey::from_bytes(DEK)).unwrap();
            begin_wipe(&mut store, WIPED, WIPE_CONFIRMATION).unwrap();
            *lock(&hooks.delete_crash_at) = Some(crash_at);
            let first = run_wipe(&lens, &NoPause, WIPED).await;
            assert_eq!(wipe_pending(&store).unwrap().as_deref(), Some(WIPED));
            if first.is_err() {
                // Resume after the crash.
                run_wipe(&lens, &NoPause, WIPED).await.unwrap();
            }
            assert!(all_keys(&fake, &format!("v1/{WIPED}/")).is_empty(), "create_only={create_only} crash_at={crash_at}");
            neighbours_intact(&fake);
            finish_wipe(&mut store, &secrets, WIPED).unwrap();
            finish_wipe(&mut store, &secrets, WIPED).unwrap();
            assert!(secrets.accounts().is_empty(), "DEK deleted");
            assert_eq!(wipe_pending(&store).unwrap(), None);
        }
    }
}

#[tokio::test]
async fn v14_wipe_needs_exact_confirmation_and_one_vault_at_a_time() {
    let mut store = Store::open_in_memory().unwrap();
    for wrong in ["", "삭제 ", " 삭제", "delete", "삭 제", "\u{1109}\u{1161}\u{11a8}\u{110c}\u{1166}", "\u{200b}삭제"] {
        assert!(matches!(begin_wipe(&mut store, WIPED, wrong), Err(VaultError::ConfirmationMismatch)), "{wrong:?}");
        assert_eq!(wipe_pending(&store).unwrap(), None);
    }
    assert!(matches!(begin_wipe(&mut store, "../etc", WIPE_CONFIRMATION), Err(VaultError::InvalidId)));
    begin_wipe(&mut store, WIPED, WIPE_CONFIRMATION).unwrap();
    assert!(matches!(begin_wipe(&mut store, NEIGHBOURS[0], WIPE_CONFIRMATION), Err(VaultError::Busy)));
}

// ===========================================================================
// Command attacks (AC 13, 15, 25, 27)
// ===========================================================================

/// A wrong RK re-entry changes nothing (keychain, remote, store) and may be
/// retried; the right one creates. Sloppy spacing / case is accepted.
#[tokio::test]
async fn c01_rk_reentry_mismatch_changes_nothing() {
    let fake = FakeStore::new(true);
    let mut raw = Store::open_in_memory().unwrap();
    let found = discover(&fake, &NoPause, now_ms).await;
    record_discovery(&mut raw, &found, now_ms()).unwrap();
    let mut pending = begin_create(&raw, &binding(), now_ms()).unwrap();
    let rk = pending.recovery_key().to_display().to_string();
    let store = Mutex::new(raw);
    let clock = Mutex::new(HlcClock::default());
    let secrets = MemoryStore::default();
    let requests = fake.requests();

    let mut flipped: Vec<char> = rk.chars().collect();
    flipped[0] = if flipped[0] == 'A' { 'B' } else { 'A' };
    let flipped: String = flipped.into_iter().collect();
    let other = RecoveryKey::generate().unwrap().to_display().to_string();
    for (wrong, codes) in [
        ("".to_owned(), &["rk_format"][..]),
        (flipped, &["rk_format", "rk_wrong"][..]),
        (other, &["rk_wrong"][..]),
        (rk[..rk.len() - 1].to_owned(), &["rk_format"][..]),
    ] {
        let err = finish_create(&fake, &secrets, &store, &clock, &mut pending, &wrong, None).await.err().unwrap();
        assert!(codes.contains(&err.code), "{wrong:?} → {err:?}");
        assert!(!err.message.contains(&rk), "error echoes the RK");
        assert!(secrets.accounts().is_empty(), "keychain untouched");
        assert_eq!(fake.requests(), requests, "no remote change");
        assert_eq!(lock(&store).active_vault().unwrap(), None);
    }
    let sloppy = rk.to_lowercase().replace('-', " ");
    let commit = finish_create(&fake, &secrets, &store, &clock, &mut pending, &sloppy, None).await.unwrap();
    assert_eq!(fake.copies(&manifest_key(&commit.vault_id).unwrap()).len(), 1);
    assert_eq!(lock(&store).active_vault().unwrap(), Some(commit.vault_id));
}

#[derive(Default)]
struct FakeHostLog(Mutex<Vec<String>>);

struct FakeHost {
    clip: Arc<ClipboardHandle>,
    log: Arc<FakeHostLog>,
}

impl Host for FakeHost {
    fn clipboard(&self) -> &ClipboardHandle {
        &self.clip
    }

    fn save_clipboard(&self, entries: &[ClipEntry], _: usize) {
        lock(&self.log.0).push(format!("save:{}", entries.len()));
    }

    fn clipboard_changed(&self) {
        lock(&self.log.0).push("emit".to_owned());
    }

    fn spawn(&self, _: BoxFuture) -> Option<tauri::async_runtime::JoinHandle<()>> {
        lock(&self.log.0).push("spawn".to_owned());
        None
    }
}

fn fake_host() -> (Arc<dyn Host>, Arc<ClipboardHandle>, Arc<FakeHostLog>) {
    let clip = ClipboardHandle::new();
    let log = Arc::new(FakeHostLog::default());
    (Arc::new(FakeHost { clip: clip.clone(), log: log.clone() }), clip, log)
}

fn dir_tree(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((entry.path().strip_prefix(dir).unwrap().to_string_lossy().into_owned(), if meta.is_dir() { 0 } else { meta.len() }));
        }
    }
    out.sort();
    out
}

/// Fail-closed settings (module doc: "malformed → off"): serde accepts a
/// JSON array as a struct, so a malformed `[true]` turns sync on and the
/// flag-on boot then opens the store. Low severity (the file is app-written).
#[test]
fn c02b_blocker_malformed_array_settings_turn_sync_on() {
    let dir = TempDir::new("g7rt-array");
    std::fs::write(settings_path(&dir.0), b"[true]").unwrap();
    assert!(!crate::sync::commands::load_settings(Some(&dir.0)).enabled, "malformed settings read as on");
}

/// Flag-off boot (AC 15) under hostile settings files and with a sync
/// database already on disk: nothing is opened (no -wal/-shm, sizes
/// unchanged), no sink, no scheduler, no save/emit/spawn, commands refuse.
#[test]
fn c02_flag_off_boot_touches_nothing_even_with_hostile_settings() {
    let variants: [&[u8]; 8] = [
        b"\xef\xbb\xbf{\"enabled\":true}",
        b"{\"enabled\":\"true\"}",
        b"{\"enabled\":1}",
        b"null",
        b"",
        b"{\"enabled\":false,\"local_only\":true}",
        b"{\"Enabled\":true}",
        b"{\"enabled\":true",
    ];
    for (i, raw) in variants.iter().enumerate() {
        let dir = TempDir::new("g7rt-off");
        std::fs::create_dir_all(dir.0.join("sync")).unwrap();
        std::fs::write(dir.0.join("sync").join("sync.db"), b"not touched").unwrap();
        std::fs::write(settings_path(&dir.0), raw).unwrap();
        let before = dir_tree(&dir.0);
        let (host, clip, log) = fake_host();
        let rt = Arc::new(SyncRuntime::new(Some(dir.0.clone()), MemoryStore::default(), Box::new(|| None)));
        assert!(rt.boot(&host, LoadOutcome::Loaded).is_empty(), "variant {i}");
        assert!(!rt.enabled(), "variant {i} reads as on");
        assert!(!rt.status().enabled);
        assert_eq!(rt.create_vault().err().map(|e| e.code), Some("disabled"));
        assert!(rt.export_diagnostics().contains("\"off\""));
        assert!(clip.memo_sink.lock().unwrap().is_none());
        assert!(lock(&log.0).is_empty(), "variant {i}: {:?}", lock(&log.0));
        assert_eq!(dir_tree(&dir.0), before, "variant {i} touched the disk");
    }
    // A directory where the settings file should be: still off.
    let dir = TempDir::new("g7rt-off-dir");
    std::fs::create_dir_all(settings_path(&dir.0)).unwrap();
    let rt = Arc::new(SyncRuntime::new(Some(dir.0.clone()), MemoryStore::default(), Box::new(|| None)));
    assert!(!rt.enabled());
    // No app data dir at all: off.
    let rt = Arc::new(SyncRuntime::new(None, MemoryStore::default(), Box::new(|| None)));
    assert!(!rt.enabled());
    assert_eq!(rt.create_vault().err().map(|e| e.code), Some("disabled"));
}

const PASSWORD: &str = "PASSWORD-SENTINEL-9f1";
const INSTALL_FP: &str = "INSTALL-FP-SENTINEL-77";
const SIM_VAULT: &str = crate::sync::sim_tests::VAULT;
const CMD_DEK: [u8; 32] = [0xA5; 32];

fn webdav_meta() -> ConnectMeta {
    ConnectMeta {
        binding_fp: binding_fp("webdav|https://dav.example.test/remote.php/dav/files/alice/|alice"),
        params: ConnectParams::Webdav {
            base_url: "https://dav.example.test/remote.php/dav/files/alice/".to_owned(),
            username: "alice".to_owned(),
        },
    }
}

/// A flag-on runtime directory: a store with an identity, the active vault,
/// a WebDAV connection, and a fresh empty discovery; the keychain holds the
/// DEK and the WebDAV password.
fn seeded_runtime(dir: &TempDir) -> (Arc<SyncRuntime<MemoryStore>>, ConnectMeta) {
    save_settings(&dir.0, SyncSettings { enabled: true, local_only: false }).unwrap();
    let meta = webdav_meta();
    {
        let mut store = Store::open(&dir.0.join("sync").join("sync.db")).unwrap();
        store.init_identity(&crate::sync::sim_tests::identity("dev-a")).unwrap();
        store.set_active_vault(SIM_VAULT).unwrap();
        store.meta_set("connection", &serde_json::to_string(&meta).unwrap()).unwrap();
        store.meta_set("discovery_state", "none").unwrap();
        store.meta_set("discovery_checked_at", &now_ms().to_string()).unwrap();
    }
    let secrets = MemoryStore::default();
    store_vault_key(&secrets, SIM_VAULT, &SecretKey::from_bytes(CMD_DEK)).unwrap();
    write_secret(&secrets, SyncSecret::Credential { provider: CloudProvider::WebDav, binding_fp: &meta.binding_fp }, PASSWORD.as_bytes())
        .unwrap();
    (Arc::new(SyncRuntime::new(Some(dir.0.clone()), secrets, Box::new(|| Some(INSTALL_FP.to_owned())))), meta)
}

/// AC 25 / §4.7: with an RK on screen (create pending), the status JSON,
/// the diagnostics export, and every log line carry no RK, DEK, password,
/// install fingerprint, binding fingerprint, vault id, or device id.
#[test]
fn c03_status_and_diagnostics_carry_no_secrets() {
    let dir = TempDir::new("g7rt-secrets");
    let (rt, meta) = seeded_runtime(&dir);
    capture::take();
    let created = rt.create_vault().unwrap();
    let rk = created.recovery_key.clone();
    let mut text = serde_json::to_string(&rt.status()).unwrap();
    text.push_str(&rt.export_diagnostics());
    text.push_str(&capture::take().join("\n"));
    assert!(text.contains("\"create_pending\":true"), "{text}");
    let dek_hex = crate::sync::crypto::hex(&CMD_DEK);
    let compact_rk = rk.replace('-', "");
    let pending_vault_in_status = serde_json::to_string(&created.vault).unwrap();
    for needle in [
        rk.as_str(),
        compact_rk.as_str(),
        &compact_rk[..16],
        dek_hex.as_str(),
        "paWlpaWl",
        PASSWORD,
        INSTALL_FP,
        meta.binding_fp.as_str(),
        SIM_VAULT,
        "dev-a",
    ] {
        assert!(!text.contains(needle), "{needle} leaked: {text}");
    }
    assert!(!pending_vault_in_status.contains('-'), "the created vault is shown only as a tag");
}

/// The wipe confirmation guard runs before anything: a wrong text changes
/// no state, keeps the connection and the credential.
#[tokio::test]
async fn c04_wrong_wipe_confirmation_changes_nothing() {
    for wrong in ["", "delete", "삭제 ", "\u{1109}\u{1161}\u{11a8}\u{110c}\u{1166}"] {
        assert_eq!(check_wipe_confirmation(true, wrong).err().map(|e| e.code), Some("confirmation_mismatch"), "{wrong:?}");
    }
    assert!(check_wipe_confirmation(true, WIPE_CONFIRMATION).is_ok());
    assert!(check_wipe_confirmation(false, "anything").is_ok(), "no wipe, no confirmation needed");

    let dir = TempDir::new("g7rt-wipe");
    let (rt, _meta) = seeded_runtime(&dir);
    let err = rt.disconnect(true, "delete").await.err().unwrap();
    assert_eq!(err.code, "confirmation_mismatch");
    let store = Store::open(&dir.0.join("sync").join("sync.db")).unwrap();
    assert_eq!(store.meta_get("wipe_state").unwrap(), None);
    assert!(store.meta_get("connection").unwrap().is_some_and(|c| !c.is_empty()), "still connected");
    // The refusal came before the store was even opened.
    assert_eq!(rt.status().wipe, "none");
    assert!(rt.status().provider.is_none(), "store untouched by the refused command");
}

fn dav_params(password: &str) -> WebDavParams {
    WebDavParams {
        base_url: tests_support::BASE.to_owned(),
        username: tests_support::USER.to_owned(),
        password: password.to_owned(),
    }
}

/// Connecting the same WebDAV account twice keeps one credential (the
/// latest); a later connect to a server that fails the probe stores nothing
/// and leaves the working credential alone.
#[tokio::test]
async fn c05_double_connect_keeps_one_credential_and_a_failed_probe_changes_nothing() {
    let (_, transport, server) = tests_support::over(DavServer::new());
    let secrets = MemoryStore::default();
    let (_, first) = connect_webdav(&secrets, dav_params(tests_support::PASSWORD), transport.clone()).await.unwrap();
    let (_, second) = connect_webdav(&secrets, dav_params(tests_support::PASSWORD), transport.clone()).await.unwrap();
    assert_eq!(first, second, "same account, same binding");
    assert_eq!(secrets.accounts().len(), 1);
    let secret = SyncSecret::Credential { provider: CloudProvider::WebDav, binding_fp: &second.binding_fp };
    assert_eq!(&read_secret(&secrets, secret).unwrap().unwrap()[..], tests_support::PASSWORD.as_bytes());
    // A wrong password is refused by the server and stores nothing.
    let err = connect_webdav(&secrets, dav_params("pw-wrong"), transport.clone()).await.err().unwrap();
    assert_eq!(err.code, "reconsent");
    assert!(!err.message.contains("pw-wrong"));

    lock(&server).honour_if_none_match = false;
    let err = connect_webdav(&secrets, dav_params(tests_support::PASSWORD), transport).await.err().unwrap();
    assert_eq!(err.code, "unsupported");
    let secret = SyncSecret::Credential { provider: CloudProvider::WebDav, binding_fp: &second.binding_fp };
    assert_eq!(&read_secret(&secrets, secret).unwrap().unwrap()[..], tests_support::PASSWORD.as_bytes(), "credential untouched");
    assert_eq!(secrets.accounts().len(), 1);
}

/// Cancel during connect: the waiting OAuth listener resolves `cancelled`
/// at once, the slot is freed for the next connect, a second start while
/// one runs is `busy`, and a cancel with nothing running is a no-op.
#[tokio::test]
async fn c06_cancel_during_connect_resolves_cancelled_and_frees_the_slot() {
    let st = OAuthCallbackState::default();
    cancel_native(&st);
    for _ in 0..2 {
        let (_, wait) =
            start_native_listener(&st, 0, crate::sync::oauth::CALLBACK_PATH, "state-g7".to_owned(), Duration::from_secs(300))
                .unwrap();
        let busy = start_native_listener(&st, 0, crate::sync::oauth::CALLBACK_PATH, "other".to_owned(), Duration::from_secs(300));
        assert_eq!(busy.err().map(|e| CommandError::from(e).code), Some("busy"));
        cancel_native(&st);
        let outcome = tokio::time::timeout(Duration::from_secs(2), wait.wait()).await.expect("cancel must not hang");
        assert_eq!(outcome.err().map(|e| CommandError::from(e).code), Some("cancelled"));
    }
}

// ===========================================================================
// Delta (pass 2, story G016): attacks on the D1–D10 fixes
// ===========================================================================

/// What a tapped call does.
enum Act {
    Pass,
    /// `get` → NotFound; `get_all` / `list` → empty.
    Miss,
    Fail(ProviderError),
}

type Rule<'a> = Box<dyn FnMut(&'static str, &str) -> Act + Send + 'a>;

/// A FakeStore view whose every call is logged and first goes through
/// `rule`, which may also change the remote (or the local store) the way
/// another device or the user would at exactly that point.
struct Tap<'a> {
    inner: &'a FakeStore,
    rule: Mutex<Rule<'a>>,
    log: Mutex<Vec<(&'static str, String)>>,
}

impl<'a> Tap<'a> {
    fn new(inner: &'a FakeStore, rule: impl FnMut(&'static str, &str) -> Act + Send + 'a) -> Self {
        Tap { inner, rule: Mutex::new(Box::new(rule)), log: Mutex::default() }
    }

    fn act(&self, op: &'static str, key: &str) -> Act {
        lock(&self.log).push((op, key.to_owned()));
        let mut rule = lock(&self.rule);
        (*rule)(op, key)
    }

    /// `Ok(true)` = answer as missing.
    async fn gate(&self, op: &'static str, key: &str) -> Result<bool, ProviderError> {
        match self.act(op, key) {
            Act::Pass => Ok(false),
            Act::Miss => Ok(true),
            Act::Fail(err) => Err(err),
        }
    }

    fn calls(&self, op: &str) -> Vec<String> {
        lock(&self.log).iter().filter(|(o, _)| *o == op).map(|(_, k)| k.clone()).collect()
    }
}

impl ObjectStore for Tap<'_> {
    fn create_only(&self) -> bool {
        self.inner.create_only()
    }

    fn identity(&self) -> String {
        self.inner.identity()
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        let key = match &q {
            ListQuery::Prefix(prefix) => (*prefix).to_owned(),
            ListQuery::Props { .. } => String::new(),
        };
        if self.gate("list", &key).await? {
            return Ok(Vec::new());
        }
        self.inner.list(q).await
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        if self.gate("get", key).await? {
            return Err(ProviderError::NotFound);
        }
        self.inner.get(key).await
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.gate("put_create", key).await?;
        self.inner.put_create(key, bytes).await
    }

    async fn put_create_unchecked(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.gate("put_create", key).await?;
        self.inner.put_create_unchecked(key, bytes).await
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        self.gate("put_replace", key).await?;
        self.inner.put_replace(key, bytes, if_match).await
    }

    async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        // The same tap point as a name-based replace: rules see one record write.
        self.gate("put_replace", key).await?;
        self.inner.put_replace_ref(key, provider_ref, bytes).await
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        self.gate("delete", key).await?;
        self.inner.delete(key).await
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        if self.gate("get_all", key).await? {
            return Ok(Vec::new());
        }
        self.inner.get_all(key).await
    }

    async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
        self.gate("delete_ref", provider_ref).await?;
        self.inner.delete_ref(provider_ref).await
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        self.inner.quota().await
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.inner.ensure_container().await
    }
}

fn tapped(tap: Tap<'_>) -> Bound<Tap<'_>> {
    let fp = binding_fp(&tap.inner.identity());
    Bound::new(tap, fp)
}

/// One engine cycle of `d` with an engine kept outside the device, so a
/// rule may hold `&d.store` while the cycle runs.
async fn cycle_raw<P: ObjectStore, Z: Pause + Sync>(
    p: &Bound<P>,
    d: &Dev,
    engine: &mut Engine,
    pause: &Z,
    fence: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> CycleReport {
    let offset = d.clock_offset_ms;
    let now = move || now_ms() + offset;
    let mut ctx = CycleCtx {
        provider: p,
        secrets: &d.secrets,
        store: &d.store,
        clock: &d.clock,
        pause,
        now: &now,
        stats: &d.stats,
        engine,
        vault: &d.vault,
        key_epoch: EPOCH,
        install_fp: d.install_fp.as_deref(),
        device_label: "red team device",
        app_version: "0.0.0-redteam",
        checkpoint_dir: None,
        fence,
    };
    Engine::cycle(&mut ctx).await
}

/// A pause that never ends (a settle pause the user cancels).
struct HangPause(AtomicBool);

impl Pause for HangPause {
    async fn pause(&self, _: Duration) {
        self.0.store(true, Ordering::SeqCst);
        std::future::pending::<()>().await
    }
}

/// Another member of `vault` (the DEK copied from `from`).
fn member(vault: &str, device: &str, from: &MemoryStore) -> Dev {
    let secrets = MemoryStore::default();
    let dek = crate::sync::secrets::load_vault_key(from, vault).unwrap().unwrap();
    store_vault_key(&secrets, vault, &dek).unwrap();
    let identity =
        DeviceIdentity { v: 1, vault_id: vault.to_owned(), device_id: device.to_owned(), install_fp: None, generation: 0 };
    crate::sync::identity::save_identity(&secrets, &identity).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    store.init_identity(&identity).unwrap();
    store.set_active_vault(vault).unwrap();
    Dev::from_parts(store, secrets, vault)
}

fn memo_text_of(dev: &Dev, id: &str) -> Option<String> {
    match lock(&dev.store).projection(&dev.vault).unwrap().entities.get(&(Kind::Memo, id.to_owned())) {
        Some(crate::sync::merge::EntityState::Memo(m)) => Some(m.text.clone()),
        _ => None,
    }
}

fn live_memo_count(dev: &Dev) -> usize {
    lock(&dev.store)
        .projection(&dev.vault)
        .unwrap()
        .entities
        .iter()
        .filter(|((kind, _), state)| *kind == Kind::Memo && matches!(state, crate::sync::merge::EntityState::Memo(_)))
        .count()
}

async fn opens_vault(remote: &FakeStore, rk: &str) -> bool {
    let store = Store::open_in_memory().unwrap();
    let secrets = MemoryStore::default();
    let Discovery::Found(ids) = discover(remote, &NoPause, || NOW).await else { return false };
    let candidates = fetch_candidates(remote, &ids).await.unwrap();
    unlock_join(&store, &secrets, &candidates, rk, &binding()).is_ok()
}

fn blob_row_for(dev: &Dev, bytes: &[u8]) -> crate::sync::store::BlobRow {
    let dek = crate::sync::secrets::load_vault_key(&dev.secrets, &dev.vault).unwrap().unwrap();
    let name_key = derive_subkey(&dek, &dev.vault, EPOCH, Purpose::BlobName);
    crate::sync::store::BlobRow {
        blob_id: crate::sync::crypto::hex(&crate::sync::crypto::hmac_sha256(&name_key, bytes)),
        mime: "image/png".to_owned(),
        bytes: bytes.to_vec(),
        sha256: <sha2::Sha256 as sha2::Digest>::digest(bytes).into(),
    }
}

/// A conversation (ops `n`, `n + 1`) whose message carries `blob`.
fn attach_row(store: &Mutex<Store>, vault: &str, n: u64, blob: &crate::sync::store::BlobRow) {
    use crate::sync::sim_tests::op;
    let conv = format!("conv-{n}");
    let bodies = vec![
        op(n, 100 + n, Kind::Conv, &conv, "create", serde_json::json!({"title": "t"})),
        op(
            n + 1,
            101 + n,
            Kind::Conv,
            &conv,
            "msg.add",
            serde_json::json!({"conv_id": conv, "msg_id": format!("msg-{n}"), "role": "user", "content": "see",
                   "attachments": [{"blob_id": blob.blob_id, "mime": blob.mime}], "ts": 101}),
        ),
    ];
    lock(store).insert_local_batch(vault, bodies, vec![blob.clone()], Vec::new()).unwrap();
}

fn blob_key_of(vault: &str, blob_id: &str) -> String {
    ObjectAddress::blob(vault, EPOCH, blob_id).remote_key().unwrap()
}

/// `(first, last)` of a segment key.
fn seg_range_of(key: &str) -> Option<(u64, u64)> {
    let address = ObjectAddress::from_remote_key(key, EPOCH)?;
    let (first, last) = address.object_ref.split_once('-')?;
    Some((first.parse().ok()?, last.parse().ok()?))
}

// -- D1: removed device, rejoin ------------------------------------------------

/// D1: a removed device whose op 2 was attempted (landed with its response
/// lost, or never landed) and whose op 3 was never sent, rejoins with the RK.
/// Every op ends up exactly once on a reader (no conflict copy, no loss),
/// the old record is never recreated, and both devices converge.
#[tokio::test]
async fn d1a_rejoin_after_removal_keeps_every_op_exactly_once() {
    for landed in [false, true] {
        let remote = FakeStore::new(true);
        let hooks = Hooks::default();
        let created = create_on(&remote).await;
        let rk = created.rk.clone();
        let vault = created.vault.clone();
        let mut a = Dev::from_parts(created.store, created.secrets, &vault);
        let old = a.device();
        a.local(create(1, 100, "memo-1", "one"));
        assert_ok(&a.cycle(&bound(&remote, &hooks)).await);

        a.local(create(2, 200, "memo-2", "two"));
        if landed {
            hooks.lose_put_responses.store(1, Ordering::SeqCst);
        } else {
            hooks.black_hole.store(true, Ordering::SeqCst);
        }
        let _ = a.cycle(&bound(&remote, &hooks)).await;
        hooks.black_hole.store(false, Ordering::SeqCst);
        assert_eq!(a.state_of(2), UploadState::Attempted, "landed={landed}");
        assert_eq!(remote.copies(&seg_key_of(&vault, &old, 2, 2)).len(), usize::from(landed));

        // Another device removes A; past the settle window A stops.
        remote.delete(&rec_key(&vault, &old)).await.unwrap();
        a.clock_offset_ms = crate::sync::engine::SETTLE_WINDOW_MS + 1_000;
        a.local(create(3, 300, "memo-3", "three"));
        let report = a.cycle(&bound(&remote, &hooks)).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "landed={landed} {report:?}");
        assert!(remote.copies(&seg_key_of(&vault, &old, 3, 3)).is_empty(), "removed device uploaded");

        // Explicit rejoin with the RK, then the engine runs again.
        let candidates = fetch_candidates(&remote, std::slice::from_ref(&vault)).await.unwrap();
        let commit =
            crate::sync::vault::rejoin(&mut lock(&a.store), &a.secrets, &candidates, &rk, &binding(), None).unwrap();
        assert_ne!(commit.device_id, old);
        a.engine = Engine::default();
        a.clock_offset_ms = 0;
        for _ in 0..2 {
            assert_ok(&a.cycle(&bound(&remote, &hooks)).await);
        }
        assert!(remote.copies(&rec_key(&vault, &old)).is_empty(), "removed record recreated (landed={landed})");
        assert_eq!(remote.copies(&rec_key(&vault, &commit.device_id)).len(), 1);

        let mut b = member(&vault, "dev-reader", &a.secrets);
        assert_ok(&b.cycle(&bound(&remote, &hooks)).await);
        assert_ok(&a.cycle(&bound(&remote, &hooks)).await);
        for dev in [&a, &b] {
            for (id, text) in [("memo-1", "one"), ("memo-2", "two"), ("memo-3", "three")] {
                assert_eq!(memo_text_of(dev, id).as_deref(), Some(text), "landed={landed} {id}");
            }
            assert_eq!(live_memo_count(dev), 3, "duplicate or conflict copy (landed={landed})");
        }
        assert_eq!(
            lock(&a.store).projection(&vault).unwrap().hash,
            lock(&b.store).projection(&vault).unwrap().hash,
            "landed={landed}"
        );
    }
}

/// D1 settle window: a device that keeps writing after its record was
/// removed is not kept alive by its own activity — no record put happens
/// while the record is absent, so the window runs out and the next cycle
/// stops; the record is never recreated on the way.
#[tokio::test]
async fn d1b_removal_is_detected_once_the_window_passes_even_under_steady_writes() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&p).await);
    fake.delete(&rec_key(VAULT, "dev-a")).await.unwrap();
    let mut stopped_at = None;
    for (i, offset) in [0u64, 20_000, 40_000, 70_000, 90_000].into_iter().enumerate() {
        a.clock_offset_ms = offset;
        let n = 10 + i as u64;
        a.local(create(n, 1_000 + n, &format!("memo-{n}"), "steady"));
        let report = a.cycle(&p).await;
        assert!(fake.copies(&rec_key(VAULT, "dev-a")).is_empty(), "record recreated at +{offset} ms");
        if report.status == (CycleStatus::Stopped { reason: StopReason::DeviceRemoved }) {
            stopped_at = Some(offset);
            break;
        }
    }
    let stopped_at = stopped_at.expect("removal never detected");
    assert!((crate::sync::engine::SETTLE_WINDOW_MS..=70_000).contains(&stopped_at), "{stopped_at}");
}

/// D1 on Drive: the record is read (present) and then replaced. If another
/// device removes it between the read and the replace, Drive's
/// `put_replace(key, None)` finds no copy and *uploads a new one*: the
/// removal is silently undone and the removed device keeps syncing.
#[tokio::test]
async fn d1c_blocker_drive_removal_between_read_and_replace_recreates_the_record() {
    let fake = remote(false);
    let hooks = Hooks::default();
    let rec = rec_key(VAULT, "dev-a");
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    assert_eq!(fake.copies(&rec).len(), 1);

    let armed = AtomicBool::new(true);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_replace" && key == rec.as_str() && armed.swap(false, Ordering::SeqCst) {
            // Another device's removal lands right now.
            block(fake.delete(key)).unwrap();
        }
        Act::Pass
    }));
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let _ = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert!(!armed.load(Ordering::SeqCst), "precondition: the record replace ran");
    assert!(fake.copies(&rec).is_empty(), "removed record recreated by the replace");
}

/// D1 settle window, false trigger: a just-written record that two reads
/// miss (lag) inside the window is not a removal; the cycle goes on and the
/// record is updated in place.
#[tokio::test]
async fn d1d_two_misses_inside_the_settle_window_are_not_a_removal() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&p).await);
    let rec = rec_key(VAULT, "dev-a");
    hooks.miss_get(&rec, 2);
    a.local(create(2, 200, "memo-2", "y"));
    let report = a.cycle(&p).await;
    assert_ok(&report);
    assert!(!report.notices.contains(&EngineNotice::DeviceRemoved));
    assert_eq!(fake.copies(&rec).len(), 1);
    assert_eq!(a.state_of(2), UploadState::Published);
}

/// D1 keyed store: the record is removed after the etag listing and before
/// the If-Match replace. The stale If-Match fails (412), nothing is
/// created, and a later cycle past the window stops DeviceRemoved.
#[tokio::test]
async fn d1e_keyed_stale_if_match_never_turns_into_a_create() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let rec = rec_key(VAULT, "dev-a");
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    let armed = AtomicBool::new(true);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_replace" && key == rec.as_str() && armed.swap(false, Ordering::SeqCst) {
            block(fake.delete(key)).unwrap();
        }
        Act::Pass
    }));
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let _ = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert!(!armed.load(Ordering::SeqCst), "precondition: the record replace ran");
    assert!(fake.copies(&rec).is_empty(), "stale If-Match recreated the record");
    a.engine = engine;
    a.clock_offset_ms = crate::sync::engine::SETTLE_WINDOW_MS + 1_000;
    let report = a.cycle(&bound(&fake, &hooks)).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
    assert!(fake.copies(&rec).is_empty());
}

// -- D2: scheduler ------------------------------------------------------------------

/// D2 (A3): after one failed cycle the scheduler is `backing_off`; every
/// later LocalChange answers `AtLeast(DEBOUNCE)` and pushes the deadline to
/// now + 5 s with no max-wait cap, so edits less than 5 s apart starve sync
/// forever — long after the 3.75 s backoff itself has elapsed. Runs ~34 s.
#[tokio::test]
async fn d2a_blocker_change_stream_after_a_failure_starves_sync_past_max_wait() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let scheduler = tokio::spawn(crate::sync::engine::run_scheduler(
        rx,
        move || {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    CycleOutcome::Retry
                } else {
                    CycleOutcome::Success
                }
            }
        },
        || 0.5,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the first (failed) cycle");
    let started = tokio::time::Instant::now();
    while started.elapsed() < crate::sync::engine::DEBOUNCE_MAX_WAIT + Duration::from_secs(4) {
        tx.send(SchedEvent::LocalChange).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let n = runs.load(Ordering::SeqCst);
    tx.send(SchedEvent::Cancel).unwrap();
    scheduler.await.unwrap();
    assert!(n >= 2, "no cycle within max-wait of the first pending change: {n} runs");
}

/// D2: a change during an armed Retry-After never shortens it, and the cycle
/// still runs once it ends. Runs ~9 s.
#[tokio::test]
async fn d2b_change_during_retry_after_neither_shortens_nor_cancels_it() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let scheduler = tokio::spawn(crate::sync::engine::run_scheduler(
        rx,
        move || {
            let counter = counter.clone();
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    CycleOutcome::RateLimited(Some(Duration::from_secs(8)))
                } else {
                    CycleOutcome::Success
                }
            }
        },
        || 0.5,
    ));
    let started = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tx.send(SchedEvent::LocalChange).unwrap();
    tokio::time::sleep_until(started + Duration::from_millis(7_400)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "Retry-After shortened by a local change");
    tokio::time::sleep_until(started + Duration::from_millis(9_500)).await;
    assert_eq!(runs.load(Ordering::SeqCst), 2, "the change was dropped after the Retry-After");
    tx.send(SchedEvent::Cancel).unwrap();
    scheduler.await.unwrap();
}

/// D2: Cancel while a real engine cycle sits in a settle pause (its upload
/// outcome unknown): the scheduler returns at once, and the next cycle
/// settles the same sealed bytes — one copy, nothing lost.
#[tokio::test]
async fn d2c_cancel_inside_a_real_settle_pause_returns_and_loses_nothing() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    hooks.black_hole.store(true, Ordering::SeqCst);
    let _ = a.cycle(&p).await;
    assert_eq!(a.state_of(1), UploadState::Attempted);

    let hang = HangPause(AtomicBool::new(false));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let (dev, provider, pause) = (&a, &p, &hang);
        let scheduler = crate::sync::engine::run_scheduler(
            rx,
            move || async move {
                let mut engine = Engine::default();
                CycleOutcome::of(&cycle_raw(provider, dev, &mut engine, pause, None).await)
            },
            || 0.5,
        );
        let cancel = async {
            for _ in 0..300 {
                if hang.0.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tx.send(SchedEvent::Cancel).unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(scheduler, cancel) })
            .await
            .expect("cancel not honoured inside the settle pause");
    }
    assert!(hang.0.load(Ordering::SeqCst), "precondition: the cycle reached a settle pause");
    assert_eq!(a.state_of(1), UploadState::Attempted, "cancel lost or mis-marked the segment");

    hooks.black_hole.store(false, Ordering::SeqCst);
    a.engine = Engine::default();
    assert_ok(&a.cycle(&p).await);
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1)).len(), 1);
    assert_eq!(a.state_of(1), UploadState::Published);
}

// -- D3: RK reissue -----------------------------------------------------------------

/// D3 keyed store, 412 race: B read the etag, A swapped the manifest before
/// B's If-Match put. B gets 412, re-reads, finds A's verified manifest and
/// aborts (ReissueSuperseded); A's RK keeps opening, B's never does, and B's
/// pending reissue can be abandoned.
#[tokio::test]
async fn d3a_keyed_412_race_aborts_the_loser_and_keeps_the_winner() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (a, old_rk) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let mkey = manifest_key(&v2).unwrap();
    let b_secrets = MemoryStore::default();
    for account in a.secrets.accounts() {
        b_secrets.put_raw(&account, &a.secrets.raw(&account).unwrap());
    }
    let mut b_store = Store::open_in_memory().unwrap();
    let pending_a = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let pending_b = begin_reissue(&remote, &b_secrets, &v2, &binding()).await.unwrap();
    let rk_a = pending_a.recovery_key().to_display().to_string();
    let rk_b = pending_b.recovery_key().to_display().to_string();
    confirm_reissue(&mut lock(&a.store), &pending_a, &rk_a).unwrap();
    confirm_reissue(&mut b_store, &pending_b, &rk_b).unwrap();
    let (_, record_a) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();
    let (_, record_b) = crate::sync::vault::reissue_pending(&b_store).unwrap().unwrap();

    let armed = AtomicBool::new(true);
    let tap_b = Tap::new(&remote, |op, key| {
        if op == "put_replace" && key == mkey.as_str() && armed.swap(false, Ordering::SeqCst) {
            // A's whole swap lands between B's etag read and B's put.
            block(publish_reissue(&remote, &a.secrets, &v2, &record_a, &binding())).unwrap();
        }
        Act::Pass
    });
    let result = publish_reissue(&tap_b, &b_secrets, &v2, &record_b, &binding()).await;
    assert!(!armed.load(Ordering::SeqCst), "precondition: B reached its If-Match put");
    assert!(matches!(result, Err(VaultError::ReissueSuperseded)), "{:?}", result.err());
    finish_reissue(&mut lock(&a.store)).unwrap();
    crate::sync::vault::abandon_reissue(&mut b_store).unwrap();
    assert!(crate::sync::vault::reissue_pending(&b_store).unwrap().is_none());
    assert_eq!(remote.copies(&mkey).len(), 1);
    assert!(opens_vault(&remote, &rk_a).await, "the winner's confirmed RK was clobbered");
    assert!(!opens_vault(&remote, &rk_b).await, "the loser's unconfirmed RK opens the vault");
    assert!(!opens_vault(&remote, &old_rk).await);
}

/// D3 Drive: a crash after the new copy landed and before the old copy was
/// deleted leaves two manifest copies. Discovery sees one vault, the
/// two-vault check sees no other vault and counts two copies, both RKs open
/// it (no Ambiguous), the engine neither banners nor deletes, and resuming
/// the publish finishes the swap.
#[tokio::test]
async fn d3b_drive_crash_between_copy_and_delete_is_one_vault_and_resumes() {
    let remote = FakeStore::new(false);
    let hooks = Hooks::default();
    let (mut a, old_rk) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let mkey = manifest_key(&v2).unwrap();
    let pending = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let new_rk = pending.recovery_key().to_display().to_string();
    confirm_reissue(&mut lock(&a.store), &pending, &new_rk).unwrap();
    let (_, record) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();

    let crash = AtomicBool::new(true);
    let tap = Tap::new(&remote, |op, key| {
        if op == "delete_ref" && key.starts_with(mkey.as_str()) && crash.swap(false, Ordering::SeqCst) {
            Act::Fail(ProviderError::Transient("crash".to_owned()))
        } else {
            Act::Pass
        }
    });
    assert!(publish_reissue(&tap, &a.secrets, &v2, &record, &binding()).await.is_err());
    assert_eq!(remote.copies(&mkey).len(), 2, "precondition: both copies");

    assert_eq!(discover(&remote, &NoPause, || NOW).await, Discovery::Found(vec![v2.clone()]));
    let check = crate::sync::vault::check_vaults(&remote, &v2).await.unwrap();
    assert!(check.manifest_present);
    assert!(check.other_vaults.is_empty(), "two copies read as two vaults");
    assert_eq!(check.manifest_copies, 2);
    assert!(opens_vault(&remote, &old_rk).await, "old RK lost before the cleanup");
    assert!(opens_vault(&remote, &new_rk).await, "new RK does not open yet");

    a.local(create(120, 3_000, "10000000-0000-4000-8000-0000000000d1", "during the dual copy"));
    let report = a.cycle(&bound(&remote, &hooks)).await;
    assert_ok(&report);
    assert!(!report.notices.iter().any(|n| matches!(n, EngineNotice::OtherVaults { .. })), "{report:?}");
    assert_eq!(remote.copies(&mkey).len(), 2, "the engine deleted a manifest copy");

    publish_reissue(&remote, &a.secrets, &v2, &record, &binding()).await.unwrap();
    finish_reissue(&mut lock(&a.store)).unwrap();
    assert_eq!(remote.copies(&mkey).len(), 1);
    assert!(!opens_vault(&remote, &old_rk).await);
    assert!(opens_vault(&remote, &new_rk).await);
}

/// D3 Drive, two devices interleaved: B's whole publish runs between A's
/// create+verify and A's delete of the base. Both copies are verified
/// manifests of one vault: both RKs open (no clobber, no Ambiguous), both
/// publishes succeed, and the base copy is gone.
#[tokio::test]
async fn d3c_drive_interleaved_reissues_leave_both_rks_valid() {
    let remote = FakeStore::new(false);
    let hooks = Hooks::default();
    let (a, old_rk) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let mkey = manifest_key(&v2).unwrap();
    let b_secrets = MemoryStore::default();
    for account in a.secrets.accounts() {
        b_secrets.put_raw(&account, &a.secrets.raw(&account).unwrap());
    }
    let mut b_store = Store::open_in_memory().unwrap();
    let pending_a = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let pending_b = begin_reissue(&remote, &b_secrets, &v2, &binding()).await.unwrap();
    let rk_a = pending_a.recovery_key().to_display().to_string();
    let rk_b = pending_b.recovery_key().to_display().to_string();
    confirm_reissue(&mut lock(&a.store), &pending_a, &rk_a).unwrap();
    confirm_reissue(&mut b_store, &pending_b, &rk_b).unwrap();
    let (_, record_a) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();
    let (_, record_b) = crate::sync::vault::reissue_pending(&b_store).unwrap().unwrap();

    let armed = AtomicBool::new(true);
    let b_result = Mutex::new(None);
    let tap_a = Tap::new(&remote, |op, key| {
        if op == "delete_ref" && key.starts_with(mkey.as_str()) && armed.swap(false, Ordering::SeqCst) {
            *lock(&b_result) = Some(block(publish_reissue(&remote, &b_secrets, &v2, &record_b, &binding())).is_ok());
        }
        Act::Pass
    });
    publish_reissue(&tap_a, &a.secrets, &v2, &record_a, &binding()).await.unwrap();
    assert_eq!(*lock(&b_result), Some(true), "B's interleaved publish");
    assert_eq!(remote.copies(&mkey).len(), 2, "A's and B's copies");
    assert!(opens_vault(&remote, &rk_a).await, "A's confirmed RK clobbered");
    assert!(opens_vault(&remote, &rk_b).await, "B's confirmed RK clobbered");
    assert!(!opens_vault(&remote, &old_rk).await, "the base copy survived");
}

/// D3: a reissue with a keychain DEK that does not open the manifest is
/// refused before any write, at begin and at publish.
#[tokio::test]
async fn d3d_reissue_with_a_wrong_dek_writes_nothing() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (a, rk) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let mkey = manifest_key(&v2).unwrap();
    let before = remote.copies(&mkey);
    let wrong = MemoryStore::default();
    store_vault_key(&wrong, &v2, &SecretKey::from_bytes([0x77; 32])).unwrap();
    assert!(matches!(begin_reissue(&remote, &wrong, &v2, &binding()).await, Err(VaultError::Verification(_))));

    let pending = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let new_rk = pending.recovery_key().to_display().to_string();
    confirm_reissue(&mut lock(&a.store), &pending, &new_rk).unwrap();
    let (_, record) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();
    let writes = Tap::new(&remote, |_, _| Act::Pass);
    assert!(publish_reissue(&writes, &wrong, &v2, &record, &binding()).await.is_err());
    for op in ["put_create", "put_replace", "delete", "delete_ref"] {
        assert!(writes.calls(op).is_empty(), "{op} with a wrong DEK");
    }
    assert_eq!(remote.copies(&mkey), before);
    assert!(opens_vault(&remote, &rk).await);
}

/// D3: a manifest copy that persists (crash between copy and delete) is
/// "a notice, never a deletion by the engine" — the engine never deletes
/// it, but it raised no notice either (`VaultCheck::manifest_copies` was
/// computed and then unused outside tests).
///
/// D3a (leader-approved adjustment, assertion unchanged): the notice is due
/// once the copies persist across two cycles, so one cycle runs first and
/// must stay silent; the second cycle's report carries the notice.
#[tokio::test]
async fn d3e_blocker_persisting_manifest_copy_raises_no_notice() {
    let remote = FakeStore::new(false);
    let hooks = Hooks::default();
    let (mut a, _) = v2_with_a(&remote, &hooks).await;
    let v2 = a.vault.clone();
    let mkey = manifest_key(&v2).unwrap();
    let pending = begin_reissue(&remote, &a.secrets, &v2, &binding()).await.unwrap();
    let new_rk = pending.recovery_key().to_display().to_string();
    confirm_reissue(&mut lock(&a.store), &pending, &new_rk).unwrap();
    let (_, record) = crate::sync::vault::reissue_pending(&lock(&a.store)).unwrap().unwrap();
    let crash = AtomicBool::new(true);
    let tap = Tap::new(&remote, |op, key| {
        if op == "delete_ref" && key.starts_with(mkey.as_str()) && crash.swap(false, Ordering::SeqCst) {
            Act::Fail(ProviderError::Transient("crash".to_owned()))
        } else {
            Act::Pass
        }
    });
    assert!(publish_reissue(&tap, &a.secrets, &v2, &record, &binding()).await.is_err());
    assert_eq!(remote.copies(&mkey).len(), 2, "precondition: both copies");
    let first = a.cycle(&bound(&remote, &hooks)).await;
    assert_ok(&first);
    assert!(
        !first.notices.iter().any(|n| matches!(n, EngineNotice::ManifestCopies { .. })),
        "one cycle is not yet persistence (D3a): {first:?}"
    );
    let report = a.cycle(&bound(&remote, &hooks)).await;
    assert_ok(&report);
    assert!(!report.notices.is_empty(), "no notice for a persisting manifest copy");
    assert!(report.notices.contains(&EngineNotice::ManifestCopies { count: 2 }), "{report:?}");
    assert_eq!(remote.copies(&mkey).len(), 2, "the engine deletes no copy");
}

// -- D4: join through snapshot + tail ------------------------------------------

/// D4: a corrupted snapshot (one flipped byte) fails the join closed, every
/// try, and V1 stays exactly as it was.
#[tokio::test]
async fn d4a_corrupt_snapshot_fails_the_join_closed_on_every_try() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (a, rk) = compacted_v2(&remote, &hooks).await;
    let snap = all_keys(&remote, &format!("v1/{}/snap/", a.vault)).remove(0);
    let mut bytes = remote.copies(&snap).remove(0);
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    remote.delete(&snap).await.unwrap();
    remote.seed(&snap, &bytes);
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let before = v1_state(&b);
    for attempt in 0..2 {
        let joined = join_preview(&remote, &mut b, &b_secrets, &rk).await;
        assert!(matches!(joined, Err(VaultError::JoinIncomplete(_))), "try {attempt}: {:?}", joined.err());
        assert_eq!(v1_state(&b), before);
    }
}

/// D4: a valid snapshot copied under another snapshot name (forged id) is
/// quarantined and the join is held, not silently completed.
#[tokio::test]
async fn d4b_forged_snapshot_name_holds_the_join() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (a, rk) = compacted_v2(&remote, &hooks).await;
    let snap = all_keys(&remote, &format!("v1/{}/snap/", a.vault)).remove(0);
    let address = ObjectAddress::from_remote_key(&snap, EPOCH).unwrap();
    let mut forged_id = address.object_ref.clone();
    let last = forged_id.pop().unwrap();
    forged_id.push(if last == '0' { '1' } else { '0' });
    let forged = ObjectAddress::snapshot(&a.vault, EPOCH, &address.device_id, &forged_id).remote_key().unwrap();
    remote.seed(&forged, &remote.copies(&snap).remove(0));
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let before = v1_state(&b);
    let joined = join_preview(&remote, &mut b, &b_secrets, &rk).await;
    assert!(matches!(joined, Err(VaultError::JoinIncomplete(_))), "{:?}", joined.err());
    assert_eq!(v1_state(&b), before);
}

/// D4: snapshot plus tail where a middle tail segment is missing: the hole is
/// past the snapshot cutoff and nothing covers it, so the join fails closed.
#[tokio::test]
async fn d4c_missing_tail_segment_after_the_snapshot_holds_the_join() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (mut a, rk) = compacted_v2(&remote, &hooks).await;
    a.clock_offset_ms = 0;
    for n in 0..3u64 {
        a.local(create(105 + n, 2_200 + n, &format!("10000000-0000-4000-8000-0000000000e{n}"), "tail"));
        assert_ok(&a.cycle(&bound(&remote, &hooks)).await);
    }
    let logs = all_keys(&remote, &format!("v1/{}/log/", a.vault));
    assert_eq!(logs.len(), 3, "precondition: three tail segments {logs:?}");
    remote.delete(&logs[1]).await.unwrap();
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let before = v1_state(&b);
    let joined = join_preview(&remote, &mut b, &b_secrets, &rk).await;
    assert!(matches!(joined, Err(VaultError::JoinIncomplete(_))), "{:?}", joined.err());
    assert_eq!(v1_state(&b), before);
}

// -- D5: blobs -------------------------------------------------------------------

/// D5: a *validly sealed* object of other content under the blob's name
/// (decrypts fine, HMAC differs) fails closed on every cycle: nothing
/// overwritten, no segment, BlobRejected.
#[tokio::test]
async fn d5a_validly_sealed_other_content_under_the_blob_name_fails_closed() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let row = blob_row_for(&a, b"PNG-d5a");
    let key = blob_key_of(VAULT, &row.blob_id);
    let blob_key = derive_subkey(&SecretKey::from_bytes(DEK), VAULT, EPOCH, Purpose::Blob);
    let forged = seal(&blob_key, &ObjectAddress::blob(VAULT, EPOCH, &row.blob_id), b"another image").unwrap();
    fake.seed(&key, &forged);
    attach_row(&a.store, VAULT, 1, &row);
    for _ in 0..2 {
        let report = a.cycle(&p).await;
        assert!(report.notices.contains(&EngineNotice::BlobRejected), "{report:?}");
        assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0, "segment published over a mismatched blob");
        assert_eq!(fake.copies(&key), vec![forged.clone()], "blob overwritten");
    }
}

/// D5: a blob over the download cap is uploaded by its writer but every
/// reader evicts it with a notice; the message still applies.
/// (Advisory: the upload side has no matching cap, so such a blob can
/// never reach another device.)
#[tokio::test]
async fn d5b_blob_over_the_download_cap_is_evicted_with_a_notice_and_never_blocks_apply() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let row = blob_row_for(&a, &vec![3u8; crate::sync::engine::MAX_BLOB_BYTES + 1]);
    attach_row(&a.store, VAULT, 1, &row);
    assert_ok(&a.cycle(&p).await);
    assert_eq!(fake.copies(&blob_key_of(VAULT, &row.blob_id)).len(), 1);
    let mut b = Dev::new("dev-b");
    let report = b.cycle(&p).await;
    assert_ok(&report);
    assert!(report.notices.contains(&EngineNotice::BlobEvicted), "{report:?}");
    assert_eq!(report.applied_segments, 1);
    assert_eq!(lock(&b.store).blob_bytes(&row.blob_id).unwrap(), None);
}

/// D5 over the object cap on upload: `upload_blob` answers Permanent, the
/// blob step stops all segments, and every cycle repeats it. One oversize
/// attachment silently wedges every later upload of the device (no notice,
/// no eviction, unrelated memos never leave).
#[tokio::test]
async fn d5c_blocker_oversize_blob_silently_wedges_all_uploads() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let row = blob_row_for(&a, &vec![5u8; crate::sync::provider::MAX_OBJECT_BYTES]);
    attach_row(&a.store, VAULT, 1, &row);
    let first = a.cycle(&p).await;
    a.local(create(10, 5_000, "memo-after", "unrelated"));
    let mut reports = vec![first];
    for _ in 0..2 {
        reports.push(a.cycle(&p).await);
    }
    let noticed = reports.iter().any(|r| {
        r.notices.iter().any(|n| matches!(n, EngineNotice::BlobRejected | EngineNotice::BlobEvicted | EngineNotice::SegmentTooLarge))
    });
    assert!(noticed, "no notice for the oversize attachment: {reports:?}");
    assert_eq!(a.state_of(10), UploadState::Published, "an unrelated memo never uploads: {reports:?}");
}

/// D5 ordering race: an attachment added while the cycle is uploading the
/// blobs it listed is sealed by the same cycle's `seal_new` and its segment
/// goes out before its blob. A reader catching up in between evicts the
/// attachment, and eviction is never retried, so the attachment is lost on
/// that reader even after the blob arrives.
#[tokio::test]
async fn d5d_blocker_attachment_added_mid_cycle_publishes_its_segment_before_its_blob() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let a = Dev::new("dev-a");
    let row1 = blob_row_for(&a, b"PNG-race-1");
    let row2 = blob_row_for(&a, b"PNG-race-2");
    attach_row(&a.store, VAULT, 1, &row1);
    let (k1, k2) = (blob_key_of(VAULT, &row1.blob_id), blob_key_of(VAULT, &row2.blob_id));
    let armed = AtomicBool::new(true);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_create" && key == k1.as_str() && armed.swap(false, Ordering::SeqCst) {
            // The user attaches another image while blob 1 is uploading.
            attach_row(&a.store, VAULT, 3, &row2);
        }
        Act::Pass
    }));
    let mut engine = Engine::default();
    let _ = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert!(!armed.load(Ordering::SeqCst), "precondition: blob 1 uploaded");
    let puts = p.inner().calls("put_create");

    // A reader catches up now, then A's next cycle uploads blob 2, then the
    // reader runs again.
    let plain = bound(&fake, &hooks);
    let mut b = Dev::new("dev-b");
    let _ = b.cycle(&plain).await;
    let _ = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    let _ = b.cycle(&plain).await;
    let _ = b.cycle(&plain).await;
    assert_eq!(
        lock(&b.store).blob_bytes(&row2.blob_id).unwrap().map(|(_, bytes)| bytes),
        Some(row2.bytes.clone()),
        "attachment lost on the reader; first cycle puts: {puts:?}"
    );
    let blob2_at = puts.iter().position(|k| *k == k2);
    for (i, key) in puts.iter().enumerate() {
        if let Some((first, last)) = seg_range_of(key).filter(|_| key.contains("/log/")) {
            if (first..=last).contains(&3) {
                assert!(blob2_at.is_some_and(|b| b < i), "segment {key} before its blob: {puts:?}");
            }
        }
    }
}

// -- D6: vault switch -------------------------------------------------------------

/// D6: the generation moves while this cycle's segment put is in flight.
/// The put already sent stays; nothing else goes out (no record, no
/// snapshot), and the cycle ends VaultSwitched.
#[tokio::test]
async fn d6a_generation_moving_mid_cycle_stops_before_the_next_put_batch() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    let current = AtomicBool::new(true);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_create" && key.contains("/log/") {
            current.store(false, Ordering::SeqCst);
        }
        Act::Pass
    }));
    let fence = || current.load(Ordering::SeqCst);
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let report = cycle_raw(&p, &a, &mut engine, &NoPause, Some(&fence)).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::VaultSwitched }, "{report:?}");
    assert!(p.inner().calls("put_replace").is_empty(), "stale cycle wrote the record");
    assert_eq!(p.inner().calls("put_create").len(), 1, "only the put already in flight");
}

/// D6 / CB-B2 at the vault layer: a V1 memo edited after the join preview
/// and before J7 is never lost: either the commit reports PreviewStale and
/// the fresh preview carries it, or the commit republishes the edited state.
#[tokio::test]
async fn d6b_memo_edit_between_preview_and_commit_reaches_the_new_vault() {
    let remote = FakeStore::new(true);
    let hooks = Hooks::default();
    let (_a, rk) = v2_with_a(&remote, &hooks).await;
    let mut b = v1_device();
    let b_secrets = MemoryStore::default();
    let (unlocked, preview) = join_preview(&remote, &mut b, &b_secrets, &rk).await.unwrap();
    b.insert_local_op(V1, edit(20, 1_100, M_MINE, 3, "edited after the preview"), UploadState::Never).unwrap();
    let commit = match commit_join(&mut b, &b_secrets, &unlocked, &preview, None, &mut HlcClock::default(), NOW) {
        Ok(commit) => commit,
        Err(VaultError::PreviewStale(fresh)) => {
            commit_join(&mut b, &b_secrets, &unlocked, &fresh, None, &mut HlcClock::default(), NOW).unwrap()
        }
        Err(e) => panic!("{e:?}"),
    };
    let projection = b.projection(&commit.vault_id).unwrap();
    match projection.entities.get(&(Kind::Memo, M_MINE.to_owned())) {
        Some(crate::sync::merge::EntityState::Memo(m)) => assert_eq!(m.text, "edited after the preview"),
        other => panic!("the edit did not reach V2: {other:?}"),
    }
}

// -- D7: wipe gate -----------------------------------------------------------------

/// D7: the manifest disappears after this cycle's segment put and before
/// its record put (between batches): the cycle stops ManifestMissing, writes
/// no record, and recreates no manifest.
///
/// Updated to D7a (the engine no longer deletes on a manifest miss;
/// `undo_created` is removed): the segment created before the wipe was seen
/// is left for the wiper's final sweep, and its local state is not changed.
#[tokio::test]
async fn d7a_manifest_404_between_batches_stops_before_the_record() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let mkey = manifest_key(VAULT).unwrap();
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    let after_put = AtomicBool::new(false);
    let p = tapped(Tap::new(&fake, |op, key| {
        if after_put.swap(false, Ordering::SeqCst) {
            block(fake.delete(&mkey)).unwrap();
        }
        if op == "put_create" && key.contains("/log/") {
            after_put.store(true, Ordering::SeqCst);
        }
        Act::Pass
    }));
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let report = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
    assert!(p.inner().calls("put_replace").is_empty(), "record written after the 404");
    assert!(fake.copies(&mkey).is_empty(), "manifest recreated");
    assert_eq!(
        fake.copies(&seg_key_of(VAULT, "dev-a", 2, 2)).len(),
        1,
        "D7a: the engine deletes nothing on a manifest miss (orphan left for the wiper's sweep)"
    );
    assert_eq!(a.state_of(2), UploadState::Published, "D7a: local upload state unchanged");
    assert!(p.inner().calls("delete").is_empty() && p.inner().calls("delete_ref").is_empty(), "no remote delete");
}

/// D7 false positive: the manifest is still there but two back-to-back
/// `get_all` reads at the record gate come back empty (a lagging Drive
/// name query). The cycle undoes what it created — including a segment it
/// had already marked Published — and nothing ever re-uploads it: the op
/// never reaches a reader.
#[tokio::test]
async fn d7b_blocker_transient_manifest_miss_deletes_a_published_segment() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let mkey = manifest_key(VAULT).unwrap();
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    let armed = AtomicBool::new(false);
    let misses = AtomicUsize::new(0);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_create" && key.contains("/log/") {
            armed.store(true, Ordering::SeqCst);
        }
        if op == "get_all" && key == mkey.as_str() && armed.load(Ordering::SeqCst) && misses.fetch_add(1, Ordering::SeqCst) < 2 {
            return Act::Miss;
        }
        Act::Pass
    }));
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let first = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert!(fake.copies(&mkey).len() == 1, "precondition: the manifest never went away");
    for _ in 0..3 {
        let _ = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    }
    let mut b = Dev::new("dev-b");
    for _ in 0..2 {
        let _ = b.cycle(&bound(&fake, &hooks)).await;
    }
    assert_eq!(memo_text_of(&b, "memo-2").as_deref(), Some("y"), "op lost after a transient miss: {first:?}");
}

// -- D8: discovery timeouts ----------------------------------------------------------

/// Lists one manifest; every get hangs.
struct HungGets;

impl ObjectStore for HungGets {
    fn create_only(&self) -> bool {
        true
    }

    fn identity(&self) -> String {
        "fake|default".to_owned()
    }

    async fn list(&self, _: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        Ok(manifests(&["vault-x"]))
    }

    async fn get(&self, _: &str) -> Result<Vec<u8>, ProviderError> {
        std::future::pending().await
    }

    async fn put_create(&self, _: &str, _: &[u8]) -> Result<ListedObject, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn put_replace(&self, _: &str, _: &[u8], _: Option<&str>) -> Result<ListedObject, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn delete(&self, _: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn get_all(&self, _: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        std::future::pending().await
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        Err(ProviderError::Permanent("unused".to_owned()))
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// D8: discovery never waits on a get: with every get hanging it still
/// answers (Found) at once.
#[tokio::test]
async fn d8a_hung_get_never_stalls_discovery() {
    let result = tokio::time::timeout(Duration::from_secs(5), discover(&HungGets, &NoPause, now_ms)).await;
    assert_eq!(result.ok(), Some(Discovery::Found(vec!["vault-x".to_owned()])));
}

/// D8: the third listing hangs after two that disagreed: discovery still
/// ends at the 30 s cap as Retry(Timeout), keeping the id it saw. Runs ~30 s.
#[tokio::test]
async fn d8b_hung_third_listing_ends_at_the_cap_keeping_seen_ids() {
    let script = Script::new(vec![Step::Ids(vec!["vault-x"]), Step::Ids(vec![]), Step::Hang]);
    let outcome = tokio::time::timeout(DISCOVERY_CAP + Duration::from_secs(2), discover(&script, &NoPause, now_ms)).await;
    assert_eq!(
        outcome.ok(),
        Some(Discovery::Retry { reason: RetryReason::Timeout, seen: vec!["vault-x".to_owned()] }),
        "discovery not bounded by the cap"
    );
    assert_eq!(script.lists.load(Ordering::SeqCst), 3);
}

// -- D9: flag -------------------------------------------------------------------------

/// D9: on only for a JSON object whose `enabled` is the boolean `true`.
#[test]
fn d9_flag_is_on_only_for_an_object_with_boolean_true() {
    let deep = format!("{}1{}", "{\"a\":".repeat(300), "}".repeat(300));
    let cases: Vec<(Vec<u8>, bool, bool)> = vec![
        (br#"{"enabled":true}"#.to_vec(), true, false),
        (b" \n{\"enabled\":true}\n ".to_vec(), true, false),
        (br#"{"enabled":true,"future":[1,{"x":null}]}"#.to_vec(), true, false),
        (br#"{"enabled":true,"local_only":true}"#.to_vec(), true, true),
        (br#"{"enabled":true,"local_only":"true"}"#.to_vec(), true, false),
        (br#"{"enabled":true,"enabled":false}"#.to_vec(), false, false),
        (b"[true]".to_vec(), false, false),
        (br#"[{"enabled":true}]"#.to_vec(), false, false),
        (b"true".to_vec(), false, false),
        (br#""enabled""#.to_vec(), false, false),
        (br#"{"enabled":"true"}"#.to_vec(), false, false),
        (br#"{"enabled":1}"#.to_vec(), false, false),
        (br#"{"enabled":null}"#.to_vec(), false, false),
        (br#"{"enabled":[true]}"#.to_vec(), false, false),
        (br#"{"enabled":{"enabled":true}}"#.to_vec(), false, false),
        (br#"{"ENABLED":true}"#.to_vec(), false, false),
        (br#"{"enabled ":true}"#.to_vec(), false, false),
        (b"\xef\xbb\xbf{\"enabled\":true}".to_vec(), false, false),
        (br#"{"enabled":true}x"#.to_vec(), false, false),
        (br#"{"enabled":true}{"enabled":true}"#.to_vec(), false, false),
        (b"\xff\xfe{\"enabled\":true}".to_vec(), false, false),
        (deep.into_bytes(), false, false),
        (Vec::new(), false, false),
    ];
    for (i, (raw, enabled, local_only)) in cases.iter().enumerate() {
        let dir = TempDir::new("g7rt-d9");
        std::fs::write(settings_path(&dir.0), raw).unwrap();
        let settings = crate::sync::commands::load_settings(Some(&dir.0));
        assert_eq!((settings.enabled, settings.local_only), (*enabled, *local_only), "case {i}: {:?}", String::from_utf8_lossy(raw));
    }
    // A directory where the file should be reads as off.
    let dir = TempDir::new("g7rt-d9-dir");
    std::fs::create_dir_all(settings_path(&dir.0)).unwrap();
    assert!(!crate::sync::commands::load_settings(Some(&dir.0)).enabled);
    // Observation: duplicate keys follow serde_json (last wins).
    let dir = TempDir::new("g7rt-d9-dup");
    std::fs::write(settings_path(&dir.0), br#"{"enabled":false,"enabled":true}"#).unwrap();
    assert!(crate::sync::commands::load_settings(Some(&dir.0)).enabled);
}

// -- D10: seal split, early failures ------------------------------------------------

/// D10 (A4): ops whose estimated size is exactly the cap share one segment;
/// one byte more splits them. Every produced plaintext stays within the cap
/// and the split segments chain.
#[test]
fn d10a_seal_split_at_exactly_the_cap_and_one_byte_over() {
    use crate::sync::sim_tests::{device, local, VAULT as SIM};
    let build = || {
        let mut store = device("dev-a");
        for n in 1..=2u64 {
            local(&mut store, create(n, 100 * n, &format!("m{n}"), &"q".repeat(700)));
        }
        store
    };
    let mut probe = build();
    let whole = probe.seal_pending_capped(SIM, usize::MAX, 0, |_, plain| Ok(plain.to_vec())).unwrap();
    assert_eq!(whole.len(), 1);
    let segment = crate::sync::ops::Segment::decode(&whole[0].bytes).unwrap();
    let sizes: usize = segment
        .ops
        .iter()
        .map(|op| crate::sync::ops::canonical_json(&serde_json::to_value(op).unwrap()).unwrap().len() + 1)
        .sum();
    let exact = 512 + "dev-a".len() + sizes;
    for (cap, want) in [(exact, vec![(1u64, 2u64)]), (exact - 1, vec![(1, 1), (2, 2)])] {
        let mut store = build();
        let mut plains = Vec::new();
        let segs = store
            .seal_pending_capped(SIM, cap, 0, |_, plain| {
                plains.push(plain.len());
                Ok(plain.to_vec())
            })
            .unwrap();
        assert_eq!(segs.iter().map(|s| (s.first_seq, s.last_seq)).collect::<Vec<_>>(), want, "cap {cap} (exact {exact})");
        assert!(plains.iter().all(|&n| n <= cap), "{plains:?} over {cap}");
        let decoded: Vec<crate::sync::ops::Segment> =
            segs.iter().map(|s| crate::sync::ops::Segment::decode(&s.bytes).unwrap()).collect();
        for pair in decoded.windows(2) {
            assert_eq!(pair[1].prev_seg_hash, crate::sync::crypto::hex(&crate::sync::ops::segment_hash(&pair[0]).unwrap()));
        }
    }
}

/// D10: a sealed segment of exactly MAX_OBJECT_BYTES is refused before any
/// request (SegmentTooLarge, ops stay local); one byte less is sent.
#[tokio::test]
async fn d10b_segment_of_exactly_max_object_bytes_is_refused_and_one_below_is_sent() {
    let max = crate::sync::provider::MAX_OBJECT_BYTES;
    let probe_len = max - 4_096;
    let sealed = {
        let probe = Dev::new("dev-a");
        probe.local(create(1, 100, M, &"y".repeat(probe_len)));
        probe.seal_own().bytes.len()
    };
    let exact = probe_len + (max - sealed);
    for (len, sent) in [(exact, false), (exact - 1, true)] {
        let fake = remote(true);
        let hooks = Hooks::default();
        let p = bound(&fake, &hooks);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, &"y".repeat(len)));
        let report = a.cycle(&p).await;
        let stored = fake.copies(&seg_key_of(VAULT, "dev-a", 1, 1));
        if sent {
            assert_ok(&report);
            assert_eq!(stored.iter().map(Vec::len).collect::<Vec<_>>(), vec![max - 1]);
        } else {
            assert!(report.notices.contains(&EngineNotice::SegmentTooLarge), "{report:?}");
            assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0);
            assert!(stored.is_empty());
            assert_eq!(a.state_of(1), UploadState::Never);
        }
    }
}

/// D10: a 429 on a blob put ends the cycle RateLimited with the provider's
/// Retry-After; no segment goes out; the next cycle uploads both in order.
#[tokio::test]
async fn d10c_blob_put_rate_limited_propagates_retry_after() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let row = blob_row_for(&a, b"PNG-d10c");
    attach_row(&a.store, VAULT, 1, &row);
    fake.fail_next(Op::PutCreate, ProviderError::RateLimited { retry_after: Some(Duration::from_secs(9)) });
    let report = a.cycle(&p).await;
    assert_eq!(report.status, CycleStatus::RateLimited { retry_after: Some(Duration::from_secs(9)) }, "{report:?}");
    assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0);
    assert_ok(&a.cycle(&p).await);
    assert_eq!(fake.copies(&blob_key_of(VAULT, &row.blob_id)).len(), 1);
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 2)).len(), 1);
}
// ===========================================================================
// Final review (story G016): attacks on the delta fixes D5a, D5b, D7a, D1a,
// D1b, D2a, D3a
// ===========================================================================

fn notice_count(reports: &[CycleReport], wanted: &EngineNotice) -> usize {
    reports.iter().map(|r| r.notices.iter().filter(|n| *n == wanted).count()).sum()
}

fn blob_state_of(dev: &Dev, blob_id: &str) -> Option<String> {
    lock(&dev.store).blob_state(&dev.vault, blob_id).unwrap()
}

/// Index of the first `put_create` of `key` in `puts`.
fn put_at(puts: &[String], key: &str) -> Option<usize> {
    puts.iter().position(|k| k == key)
}

/// D5a: one segment (sealed in cycle 1) references two blobs; the second
/// blob's put fails before landing. No segment goes out in that cycle. The
/// next cycle puts the earlier-sealed segment only after the missing blob,
/// never re-puts the published one, and a reader gets both attachments.
#[tokio::test]
async fn f5a1_blob_put_failing_midway_keeps_the_earlier_sealed_segment_unpublished() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let a = Dev::new("dev-a");
    let (r1, r2) = (blob_row_for(&a, b"PNG-f5a1-one"), blob_row_for(&a, b"PNG-f5a1-two"));
    attach_row(&a.store, VAULT, 1, &r1);
    attach_row(&a.store, VAULT, 3, &r2);
    let (k1, k2) = (blob_key_of(VAULT, &r1.blob_id), blob_key_of(VAULT, &r2.blob_id));
    // pending_blobs runs in blob-id order: fail whichever goes second.
    let (ok_key, bad_key) = if r1.blob_id < r2.blob_id { (k1.clone(), k2.clone()) } else { (k2.clone(), k1.clone()) };
    let failed = AtomicBool::new(false);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_create" && key == bad_key.as_str() && !failed.swap(true, Ordering::SeqCst) {
            Act::Fail(ProviderError::Transient("connection reset".to_owned()))
        } else {
            Act::Pass
        }
    }));
    let mut engine = Engine::default();
    let first = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert!(failed.load(Ordering::SeqCst), "precondition: the second blob put failed");
    assert_eq!(fake.copies(&ok_key).len(), 1, "the first blob went up");
    assert!(fake.copies(&bad_key).is_empty());
    assert!(
        p.inner().calls("put_create").iter().all(|k| !k.contains("/log/")),
        "a segment went out with a blob missing: {first:?}"
    );
    assert_eq!(lock(&a.store).segment_states(VAULT, "dev-a").unwrap(), vec![(1, 4, UploadState::Never)]);

    // Next cycle: the segment sealed earlier waits for its blob.
    let p2 = tapped(Tap::new(&fake, |_, _| Act::Pass));
    assert_ok(&cycle_raw(&p2, &a, &mut engine, &NoPause, None).await);
    let puts = p2.inner().calls("put_create");
    let seg = seg_key_of(VAULT, "dev-a", 1, 4);
    assert!(put_at(&puts, &bad_key).is_some_and(|b| put_at(&puts, &seg).is_some_and(|s| b < s)), "{puts:?}");
    assert_eq!(put_at(&puts, &ok_key), None, "a published blob was put again: {puts:?}");
    assert_eq!(a.state_of(3), UploadState::Published);

    let mut b = Dev::new("dev-b");
    assert_ok(&b.cycle(&bound(&fake, &hooks)).await);
    for row in [&r1, &r2] {
        assert_eq!(lock(&b.store).blob_bytes(&row.blob_id).unwrap().map(|(_, bytes)| bytes), Some(row.bytes.clone()));
    }
}

/// D5a + dedupe: the first blob put lands but its response is lost. The cycle
/// sends no segment; the next one meets AlreadyExists, decrypts and compares
/// (dedupe, no second copy, no BlobRejected), uploads the rest, then the
/// segment.
#[tokio::test]
async fn f5a2_blob_landed_with_lost_response_dedupes_and_then_publishes_the_segment() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let (r1, r2) = (blob_row_for(&a, b"PNG-f5a2-one"), blob_row_for(&a, b"PNG-f5a2-two"));
    attach_row(&a.store, VAULT, 1, &r1);
    attach_row(&a.store, VAULT, 3, &r2);
    hooks.lose_put_responses.store(1, Ordering::SeqCst);
    let first = a.cycle(&p).await;
    assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0, "segment put after a lost blob response: {first:?}");
    let landed = [&r1, &r2].iter().filter(|r| !fake.copies(&blob_key_of(VAULT, &r.blob_id)).is_empty()).count();
    assert_eq!(landed, 1, "the blob step stopped at the lost response");
    let second = a.cycle(&p).await;
    assert_ok(&second);
    assert!(!second.notices.contains(&EngineNotice::BlobRejected), "own blob rejected: {second:?}");
    for row in [&r1, &r2] {
        assert_eq!(fake.copies(&blob_key_of(VAULT, &row.blob_id)).len(), 1);
        assert_eq!(blob_state_of(&a, &row.blob_id).as_deref(), Some("published"));
    }
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 1, 4)).len(), 1);
    assert_eq!(a.state_of(3), UploadState::Published);
}

/// D5a fail closed, then recovery: other content sits under the blob name.
/// The segment sealed in cycle 1 stays unpublished across cycles (and the
/// chain waits behind it). Once the foreign object is gone, the blob goes up
/// first, then the segments, in order, and a reader gets the attachment.
#[tokio::test]
async fn f5a3_segment_held_by_a_fail_closed_blob_goes_out_after_its_blob_once_cleared() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let a = Dev::new("dev-a");
    let row = blob_row_for(&a, b"PNG-f5a3");
    let key = blob_key_of(VAULT, &row.blob_id);
    let blob_key = derive_subkey(&SecretKey::from_bytes(DEK), VAULT, EPOCH, Purpose::Blob);
    fake.seed(&key, &seal(&blob_key, &ObjectAddress::blob(VAULT, EPOCH, &row.blob_id), b"not this image").unwrap());
    attach_row(&a.store, VAULT, 1, &row);
    let mut engine = Engine::default();
    for cycle in 0..2 {
        let report = cycle_raw(&bound(&fake, &hooks), &a, &mut engine, &NoPause, None).await;
        assert!(report.notices.contains(&EngineNotice::BlobRejected), "{report:?}");
        if cycle == 0 {
            a.local(create(10, 5_000, "memo-later", "behind the attachment"));
        }
    }
    assert_eq!(hooks.log_puts.load(Ordering::SeqCst), 0, "segment put over a mismatched blob");

    fake.delete(&key).await.unwrap();
    let p = tapped(Tap::new(&fake, |_, _| Act::Pass));
    assert_ok(&cycle_raw(&p, &a, &mut engine, &NoPause, None).await);
    let puts = p.inner().calls("put_create");
    let blob_at = put_at(&puts, &key).expect("blob never re-uploaded after the clear");
    let logs: Vec<usize> = puts.iter().enumerate().filter(|(_, k)| k.contains("/log/")).map(|(i, _)| i).collect();
    assert!(!logs.is_empty() && logs.iter().all(|&i| i > blob_at), "{puts:?}");
    assert_eq!(a.state_of(10), UploadState::Published);
    let mut b = Dev::new("dev-b");
    assert_ok(&b.cycle(&bound(&fake, &hooks)).await);
    assert_eq!(lock(&b.store).blob_bytes(&row.blob_id).unwrap().map(|(_, bytes)| bytes), Some(row.bytes.clone()));
}

/// D5a dedupe across segments and devices: the same attachment in a later
/// segment of A is not uploaded again; B attaching the same bytes before it
/// caught up meets AlreadyExists, verifies and dedupes. One copy, every
/// segment published, no rejection.
#[tokio::test]
async fn f5a4_same_blob_across_segments_and_devices_is_one_copy_and_never_blocks() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mut a = Dev::new("dev-a");
    let mut b = Dev::new("dev-b");
    let row = blob_row_for(&a, b"PNG-f5a4-shared");
    let key = blob_key_of(VAULT, &row.blob_id);
    attach_row(&a.store, VAULT, 1, &row);
    // B attaches the same bytes before it has seen A's anything.
    assert_eq!(blob_row_for(&b, &row.bytes).blob_id, row.blob_id);
    attach_row(&b.store, VAULT, 21, &row);
    assert_ok(&a.cycle(&p).await);
    attach_row(&a.store, VAULT, 3, &row);
    let before = hooks.puts.load(Ordering::SeqCst);
    let a2 = a.cycle(&p).await;
    assert_ok(&a2);
    assert_eq!(fake.copies(&seg_key_of(VAULT, "dev-a", 3, 4)).len(), 1, "second segment held: {a2:?}");
    let report = b.cycle(&p).await;
    assert_ok(&report);
    assert!(!report.notices.contains(&EngineNotice::BlobRejected), "{report:?}");
    assert_eq!(fake.copies(&key).len(), 1);
    assert_eq!(b.state_of(21), UploadState::Published);
    assert_eq!(blob_state_of(&b, &row.blob_id).as_deref(), Some("published"));
    // A's second cycle sent the segment and the record, never the blob.
    assert!(hooks.puts.load(Ordering::SeqCst) > before);
}

/// D5b: an over-cap attachment between two normal ones. The normal blobs and
/// every segment go up in the same cycle; the oversize one is never sent,
/// is `skipped_oversize`, raises BlobEvicted once over several cycles (an
/// engine restart included), and a reader sees it evicted while the other
/// attachments arrive.
#[tokio::test]
async fn f5b_oversize_blob_beside_normal_ones_is_skipped_once_and_blocks_nothing() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let mut a = Dev::new("dev-a");
    let n1 = blob_row_for(&a, b"PNG-f5b-small-one");
    let big = blob_row_for(&a, &vec![7u8; crate::sync::provider::MAX_OBJECT_BYTES]);
    let n2 = blob_row_for(&a, b"PNG-f5b-small-two");
    attach_row(&a.store, VAULT, 1, &n1);
    attach_row(&a.store, VAULT, 3, &big);
    attach_row(&a.store, VAULT, 5, &n2);
    let big_key = blob_key_of(VAULT, &big.blob_id);
    let p = tapped(Tap::new(&fake, |_, _| Act::Pass));
    let mut engine = Engine::default();
    let mut reports = vec![cycle_raw(&p, &a, &mut engine, &NoPause, None).await];
    assert_ok(&reports[0]);
    assert_eq!(notice_count(&reports, &EngineNotice::BlobEvicted), 1, "{:?}", reports[0]);
    for row in [&n1, &n2] {
        assert_eq!(fake.copies(&blob_key_of(VAULT, &row.blob_id)).len(), 1, "a normal blob was held");
    }
    assert_eq!(put_at(&p.inner().calls("put_create"), &big_key), None, "oversize blob sent");
    assert!(fake.copies(&big_key).is_empty());
    assert_eq!(blob_state_of(&a, &big.blob_id).as_deref(), Some("skipped_oversize"));
    assert_eq!(a.state_of(6), UploadState::Published, "a segment was held by the oversize blob");

    a.local(create(10, 5_000, "memo-after", "unrelated"));
    reports.push(cycle_raw(&p, &a, &mut engine, &NoPause, None).await);
    a.engine = Engine::default();
    a.local(create(11, 5_100, "memo-after-restart", "unrelated"));
    reports.push(a.cycle(&bound(&fake, &hooks)).await);
    for report in &reports {
        assert_ok(report);
    }
    assert_eq!(notice_count(&reports, &EngineNotice::BlobEvicted), 1, "notice repeated: {reports:?}");
    assert_eq!(a.state_of(11), UploadState::Published);
    assert_eq!(put_at(&p.inner().calls("put_create"), &big_key), None);

    let mut b = Dev::new("dev-b");
    let report = b.cycle(&bound(&fake, &hooks)).await;
    assert_ok(&report);
    assert!(report.notices.contains(&EngineNotice::BlobEvicted), "{report:?}");
    assert_eq!(lock(&b.store).blob_bytes(&big.blob_id).unwrap(), None);
    for row in [&n1, &n2] {
        assert_eq!(lock(&b.store).blob_bytes(&row.blob_id).unwrap().map(|(_, bytes)| bytes), Some(row.bytes.clone()));
    }
}

/// D7a: every second read of the manifest misses (a lagging name query), so
/// no two consecutive reads miss. Cycles never stop, every op and the record
/// go out, and nothing is deleted.
#[tokio::test]
async fn f7a1_single_read_manifest_misses_never_stop_the_cycle() {
    let fake = remote(true);
    let mkey = manifest_key(VAULT).unwrap();
    let a = Dev::new("dev-a");
    let reads = AtomicUsize::new(0);
    let misses = AtomicUsize::new(0);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "get_all" && key == mkey.as_str() && reads.fetch_add(1, Ordering::SeqCst) % 2 == 0 {
            misses.fetch_add(1, Ordering::SeqCst);
            return Act::Miss;
        }
        Act::Pass
    }));
    let mut engine = Engine::default();
    for n in 1..=3u64 {
        a.local(create(n, 100 * n, &format!("memo-{n}"), "x"));
        let report = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
        assert_ok(&report);
        assert_eq!(a.state_of(n), UploadState::Published);
    }
    assert!(misses.load(Ordering::SeqCst) >= 6, "precondition: the gates met misses");
    assert_eq!(fake.copies(&rec_key(VAULT, "dev-a")).len(), 1);
    assert!(p.inner().calls("delete").is_empty() && p.inner().calls("delete_ref").is_empty());
}

/// Records the settle pauses into the same event log as the tap.
struct LogPause<'a>(&'a Mutex<Vec<String>>);

impl Pause for LogPause<'_> {
    async fn pause(&self, d: Duration) {
        lock(self.0).push(format!("pause:{}", d.as_millis()));
    }
}

/// D7a: after this cycle's segment put the manifest reads miss. The stop
/// comes only after two missed reads with a real (non-zero) settle pause
/// between them; no record, no further create in later cycles, nothing
/// deleted, local state unchanged; once the reads recover the cycle resumes
/// and a reader gets every op.
#[tokio::test]
async fn f7a2_two_misses_across_the_pause_stop_with_no_create_and_no_delete() {
    let fake = remote(true);
    let hooks = Hooks::default();
    let mkey = manifest_key(VAULT).unwrap();
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    let events = Mutex::new(Vec::new());
    let armed = AtomicBool::new(false);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_create" && key.contains("/log/") {
            armed.store(true, Ordering::SeqCst);
        }
        if op == "get_all" && key == mkey.as_str() {
            let miss = armed.load(Ordering::SeqCst);
            lock(&events).push(if miss { "manifest:miss" } else { "manifest:hit" }.to_owned());
            if miss {
                return Act::Miss;
            }
        }
        Act::Pass
    }));
    let pause = LogPause(&events);
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let report = cycle_raw(&p, &a, &mut engine, &pause, None).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
    let log = lock(&events).clone();
    let reads: Vec<usize> = log.iter().enumerate().filter(|(_, e)| e.starts_with("manifest:")).map(|(i, _)| i).collect();
    let (i, j) = (reads[reads.len() - 2], reads[reads.len() - 1]);
    assert!(log[i] == "manifest:miss" && log[j] == "manifest:miss", "{log:?}");
    assert!(
        log[i + 1..j].iter().any(|e| e.strip_prefix("pause:").is_some_and(|ms| ms.parse::<u64>().unwrap() > 0)),
        "the two misses were not separated by a settle pause: {log:?}"
    );
    assert!(p.inner().calls("put_replace").is_empty(), "record written after the confirmed miss");
    assert_eq!(a.state_of(2), UploadState::Published, "local upload state changed");

    let creates = p.inner().calls("put_create").len();
    a.local(create(3, 300, "memo-3", "z"));
    for _ in 0..2 {
        let report = cycle_raw(&p, &a, &mut engine, &pause, None).await;
        assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::ManifestMissing }, "{report:?}");
    }
    assert_eq!(p.inner().calls("put_create").len(), creates, "created after the stop");
    assert!(p.inner().calls("put_replace").is_empty());
    assert!(p.inner().calls("delete").is_empty() && p.inner().calls("delete_ref").is_empty(), "remote delete");
    assert_eq!(fake.copies(&mkey).len(), 1, "manifest touched");

    armed.store(false, Ordering::SeqCst);
    // The tap re-arms on a segment put; read through a plain view instead.
    a.engine = engine;
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    assert_eq!(a.state_of(3), UploadState::Published);
    let mut b = Dev::new("dev-b");
    assert_ok(&b.cycle(&bound(&fake, &hooks)).await);
    for (id, text) in [("memo-2", "y"), ("memo-3", "z")] {
        assert_eq!(memo_text_of(&b, id).as_deref(), Some(text));
    }
}

/// D1a Drive, ref gone: the record was read back (verified); another device
/// removes it between the observation and the media update. The update is
/// NotFound, the cycle stops DeviceRemoved at once, nothing is created then
/// or in a later cycle.
#[tokio::test]
async fn f1a1_drive_record_ref_gone_stops_device_removed_with_no_create() {
    let fake = remote(false);
    let hooks = Hooks::default();
    let rec = rec_key(VAULT, "dev-a");
    let mut a = Dev::new("dev-a");
    a.local(create(1, 100, M, "x"));
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    assert_ok(&a.cycle(&bound(&fake, &hooks)).await);
    let verified = format!("rec_verified:{VAULT}:dev-a");
    assert_eq!(lock(&a.store).meta_get(&verified).unwrap().as_deref(), Some("1"), "precondition: read back");

    let armed = AtomicBool::new(true);
    let p = tapped(Tap::new(&fake, |op, key| {
        if op == "put_replace" && key == rec.as_str() && armed.swap(false, Ordering::SeqCst) {
            block(fake.delete(key)).unwrap();
        }
        Act::Pass
    }));
    a.local(create(2, 200, "memo-2", "y"));
    let mut engine = std::mem::take(&mut a.engine);
    let report = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert!(!armed.load(Ordering::SeqCst), "precondition: the record update ran");
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
    assert!(fake.copies(&rec).is_empty(), "removed record recreated");
    assert_eq!(p.inner().calls("put_replace").len(), 1, "the update was retried");

    let creates = p.inner().calls("put_create").len();
    a.local(create(3, 300, "memo-3", "z"));
    let report = cycle_raw(&p, &a, &mut engine, &NoPause, None).await;
    assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
    assert_eq!(p.inner().calls("put_create").len(), creates, "removed device uploaded");
    assert_eq!(p.inner().calls("put_replace").len(), 1);
    assert!(fake.copies(&rec).is_empty());
}

/// D1a Drive, ref present: every record change is a media update of the same
/// file id (one copy, same provider_ref, new content), never a new upload.
#[tokio::test]
async fn f1a2_drive_record_present_is_updated_in_place() {
    let fake = remote(false);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let rec = rec_key(VAULT, "dev-a");
    let mut a = Dev::new("dev-a");
    let dek = SecretKey::from_bytes(DEK);
    let mut seen: Option<String> = None;
    for n in 1..=3u64 {
        a.local(create(n, 100 * n, &format!("memo-{n}"), "x"));
        assert_ok(&a.cycle(&p).await);
        let copies = fake.get_all(&rec).await.unwrap();
        assert_eq!(copies.len(), 1, "cycle {n}");
        let record = open_record(&dek, VAULT, EPOCH, "dev-a", &copies[0].bytes).unwrap();
        assert_eq!(record.last_seq, n, "cycle {n}: record not updated");
        match &seen {
            Some(r) => assert_eq!(*r, copies[0].provider_ref, "cycle {n}: record re-created under a new file id"),
            None => seen = Some(copies[0].provider_ref.clone()),
        }
    }
}

/// D1b: a validly sealed record with the same content but other bytes (a
/// re-seal, or any version this device did not just write) is not a
/// read-back: `rec_verified` stays unset, so a later disappearance inside the
/// settle window is not a removal. Control: an exact read-back verifies, and
/// then a disappearance stops DeviceRemoved at once.
#[tokio::test]
async fn f1b_reread_with_another_hash_never_verifies_the_record() {
    let dek = SecretKey::from_bytes(DEK);
    let rec = rec_key(VAULT, "dev-a");
    let verified = format!("rec_verified:{VAULT}:dev-a");
    for forge in [true, false] {
        let fake = remote(true);
        let hooks = Hooks::default();
        let p = bound(&fake, &hooks);
        let mut a = Dev::new("dev-a");
        a.local(create(1, 100, M, "x"));
        assert_ok(&a.cycle(&p).await);
        assert_eq!(lock(&a.store).meta_get(&verified).unwrap().as_deref(), Some("0"));
        if forge {
            let ours = fake.copies(&rec).remove(0);
            let resealed =
                crate::sync::engine::seal_record(&dek, VAULT, EPOCH, &open_record(&dek, VAULT, EPOCH, "dev-a", &ours).unwrap())
                    .unwrap();
            assert_ne!(resealed, ours);
            fake.put_replace(&rec, &resealed, None).await.unwrap();
        }
        assert_ok(&a.cycle(&p).await);
        let state = lock(&a.store).meta_get(&verified).unwrap();
        fake.delete(&rec).await.unwrap();
        let report = a.cycle(&p).await;
        if forge {
            assert_eq!(state.as_deref(), Some("0"), "a record with another hash verified the put");
            assert_ne!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
        } else {
            assert_eq!(state.as_deref(), Some("1"), "control: exact read-back");
            assert_eq!(report.status, CycleStatus::Stopped { reason: StopReason::DeviceRemoved }, "{report:?}");
        }
        assert!(fake.copies(&rec).is_empty(), "forge={forge}: record recreated");
    }
}

/// D2a boundary math: deadline = max(backoff, min(now + DEBOUNCE, first +
/// max-wait)). Exact and off-by-one points, then invariants on a grid: never
/// below the backoff, never past max(backoff, first + max-wait), never
/// earlier than the capped debounce, monotone in `now`.
#[test]
fn f2a_backoff_change_deadline_boundaries() {
    use crate::sync::engine::{backoff_change_deadline as f, debounce_deadline, DEBOUNCE_MAX_WAIT};
    let s = Duration::from_secs;
    let ms = Duration::from_millis;
    let (d, w, first) = (DEBOUNCE, DEBOUNCE_MAX_WAIT, s(100));
    assert_eq!((d, w), (s(5), s(30)));
    let cases = [
        (s(100), None, s(105)),
        (s(124), None, s(129)),
        (ms(124_999), None, ms(129_999)),
        (s(125), None, s(130)),
        (ms(125_001), None, s(130)),
        (s(200), None, s(130)),
        (s(100), Some(s(110)), s(110)),
        (s(105), Some(s(110)), s(110)),
        (s(106), Some(s(110)), s(111)),
        (s(129), Some(s(110)), s(130)),
        (s(129), Some(s(130)), s(130)),
        (s(129), Some(s(131)), s(131)),
        (s(101), Some(s(1_900)), s(1_900)),
        (s(100), Some(s(100)), s(105)),
    ];
    for (now, backoff, want) in cases {
        assert_eq!(f(now, backoff, first, d, w), want, "now {now:?} backoff {backoff:?}");
    }
    assert_eq!(debounce_deadline(s(7), None, d, w), s(12));
    let backoffs: Vec<Option<Duration>> = std::iter::once(None).chain((90..=210).step_by(7).map(|b| Some(s(b)))).collect();
    for backoff in backoffs {
        let mut last = Duration::ZERO;
        for now_ms in (100_000..=200_000u64).step_by(250) {
            let now = ms(now_ms);
            let at = f(now, backoff, first, d, w);
            let floor = backoff.unwrap_or(Duration::ZERO);
            assert!(at >= floor, "shortened the backoff: {now:?} {backoff:?} → {at:?}");
            assert!(at <= floor.max(first + w), "starved past max(backoff, max-wait): {now:?} {backoff:?} → {at:?}");
            assert!(at >= (now + d).min(first + w), "earlier than the capped debounce: {now:?} {backoff:?} → {at:?}");
            assert!(at >= last, "not monotone: {now:?} {backoff:?}");
            last = at;
        }
    }
    let mut state = SchedState { backing_off: true, ..SchedState::default() };
    assert_eq!(next_delay(&mut state, SchedEvent::LocalChange, &mut || 0.5), Decision::AtLeast(DEBOUNCE));
}

/// D3a: several manifest copies raise the notice only when seen in two
/// consecutive cycles: one cycle is silent, a cycle without them resets the
/// count, the notice carries the current count, and nothing is deleted.
#[tokio::test]
async fn f3a_manifest_copies_notice_needs_two_consecutive_cycles() {
    let fake = remote(false);
    let hooks = Hooks::default();
    let p = bound(&fake, &hooks);
    let mkey = manifest_key(VAULT).unwrap();
    let mut a = Dev::new("dev-a");
    assert_eq!(copies_step(&mut a, &p).await, None, "one copy");
    fake.seed(&mkey, b"{\"manifest\":true}");
    assert_eq!(copies_step(&mut a, &p).await, None, "first cycle with two copies");
    let extra = fake.get_all(&mkey).await.unwrap()[1].provider_ref.clone();
    fake.delete_ref(&extra).await.unwrap();
    assert_eq!(copies_step(&mut a, &p).await, None, "copy gone");
    fake.seed(&mkey, b"{\"manifest\":true}");
    assert_eq!(copies_step(&mut a, &p).await, None, "the run of cycles restarted after the gap");
    assert_eq!(copies_step(&mut a, &p).await, Some(EngineNotice::ManifestCopies { count: 2 }));
    fake.seed(&mkey, b"{\"manifest\":true}");
    assert_eq!(copies_step(&mut a, &p).await, Some(EngineNotice::ManifestCopies { count: 3 }));
    assert_eq!(fake.copies(&mkey).len(), 3, "the engine deleted a copy");
    assert_eq!(crate::sync::engine::MANIFEST_COPY_CYCLES, 2);
}

/// One cycle of `a` that must be Ok; its ManifestCopies notice, if any.
async fn copies_step<P: ObjectStore>(a: &mut Dev, p: &Bound<P>) -> Option<EngineNotice> {
    let report = a.cycle(p).await;
    assert_ok(&report);
    report.notices.iter().find(|n| matches!(n, EngineNotice::ManifestCopies { .. })).cloned()
}
