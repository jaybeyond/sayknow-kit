//! G10 3-device simulator (design contract §A): full devices — `Engine` +
//! `Store` + in-memory keychain + an `HlcClock` ticked from one shared test
//! clock — over one shared [`FakeStore`], each seen through its own lens:
//! per-reader list lag (by the test clock), hidden Drive copies, stale reads,
//! queued faults (429 / 5xx / quota), a sticky put failure, and lost put
//! responses (upload outcome unknown). No network and no real keychain.
//!
//! Scenarios 1–7 assert convergence (identical `projection_hash` on every
//! device), zero loss, and each scenario's own invariants.
//!
//! Seeds: the soak runs [`SEEDS`] by default; `SAYKNOW_SIM_SEED=<n>` runs only
//! seed `n`. A failing seed prints `SAYKNOW_SIM_SEED=<n>`. A replay repeats
//! the decision sequence; vault and device ids minted by the real create,
//! join, and rotation paths are random, so tie-breaks between concurrent
//! writers may differ between replays.
//!
//! Perf probes print `G10-PERF` lines and assert no thresholds. They run in
//! the test profile (unoptimized).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::clipboard::{make_preview, save_to, ClipEntry, EntryKind};
use crate::sync::compaction::MAX_SEGMENT_AGE_MS;
use crate::sync::crypto::{derive_subkey, hex, Purpose, SecretKey};
use crate::sync::engine::{open_record, CycleCtx, CycleReport, CycleStatus, Engine, EngineNotice, A3_NOTICE_CYCLES};
use crate::sync::envelope::{seal, ObjectAddress, ObjectKind};
use crate::sync::identity::{genesis_chain_head, save_identity, DeviceIdentity};
use crate::sync::memo_bridge::{MemoSink, StoreMemoSink};
use crate::sync::merge::{project, EntityState, Notice, Projection};
use crate::sync::migration::{self, MigrationDir, WebStore, WsConv, WsMsg, WsTr};
use crate::sync::ops::{HlcClock, Kind, OpBody};
use crate::sync::provider::fake::FakeStore;
use crate::sync::provider::{binding_fp, Bound, ListQuery, ListedObject, ObjectStore, ProviderError, Quota, RemoteKind};
use crate::sync::publish::{Pause, RemoteObject};
use crate::sync::recovery::INITIAL_KEY_EPOCH;
use crate::sync::rotation::UploadState;
use crate::sync::secrets::{load_vault_key, store_vault_key, MemoryStore};
use crate::sync::sim_tests::TempDir;
use crate::sync::store::{now_ms, open_segment, ApplyOutcome, CursorState, Store};
use crate::sync::telemetry::SyncStats;
use crate::sync::vault::{
    activate_created, apply_target, begin_create, commit_join, confirm_create, discover, fetch_candidates,
    fetch_target, manifest_key, preview_join, publish_created, record_discovery, unlock_join, Discovery, JoinAction,
    VaultError, CRASH_BEFORE_COMMIT,
};

const EPOCH: u32 = INITIAL_KEY_EPOCH;
const IDENTITY: &str = "fake|default";
/// The junk-manifest vault of the fixed-id scenarios (1–3, 5, 6).
const VAULT: &str = "vault-sim3";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn binding() -> String {
    binding_fp(IDENTITY)
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
// Seeded randomness

/// splitmix64: small, seedable, and stable across platforms.
struct SimRng(u64);

impl SimRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            items.get(self.below(items.len() as u64) as usize)
        }
    }
}

/// Prints the replay command when the soak panics.
struct SeedGuard(u64);

impl Drop for SeedGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("sim3 soak failed: replay with SAYKNOW_SIM_SEED={}", self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// The shared remote and the per-reader lenses

/// One shared remote and the test clock every device reads.
struct World {
    fake: FakeStore,
    now: Arc<AtomicU64>,
    /// When each copy (by provider_ref) was written, on the test clock.
    born: Mutex<HashMap<String, u64>>,
    deletes: AtomicUsize,
    next_op: AtomicU64,
}

impl World {
    fn new(create_only: bool) -> World {
        World {
            fake: FakeStore::new(create_only),
            now: Arc::new(AtomicU64::new(now_ms())),
            born: Mutex::new(HashMap::new()),
            deletes: AtomicUsize::new(0),
            next_op: AtomicU64::new(0),
        }
    }

    /// A world holding a (junk) manifest for [`VAULT`]: the engine only
    /// checks its presence.
    fn with_vault(create_only: bool) -> World {
        let w = World::new(create_only);
        w.fake.seed(&manifest_key(VAULT).unwrap(), b"{\"manifest\":true}");
        w
    }

    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn keys(&self, prefix: &str) -> Vec<String> {
        let listed = if self.fake.create_only() {
            block(self.fake.list(ListQuery::Prefix("v1/")))
        } else {
            block(self.fake.list(ListQuery::Props { vault_id: None, kind: None, device_id: None }))
        };
        let mut keys: Vec<String> =
            listed.unwrap().into_iter().map(|o| o.key).filter(|k| k.starts_with(prefix)).collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Every decrypted segment copy of `device` in `vault`.
    fn segments(&self, dek: &SecretKey, vault: &str, device: &str) -> Vec<crate::sync::ops::Segment> {
        let key = derive_subkey(dek, vault, EPOCH, Purpose::Segment);
        let mut out = Vec::new();
        for name in self.keys(&format!("v1/{vault}/log/{device}/")) {
            let address = ObjectAddress::from_remote_key(&name, EPOCH).unwrap();
            for copy in self.fake.copies(&name) {
                out.push(open_segment(&key, &address, &copy).unwrap());
            }
        }
        out
    }

    fn remote_ops(&self, dek: &SecretKey, vault: &str, device: &str) -> Vec<OpBody> {
        self.segments(dek, vault, device).into_iter().flat_map(|s| s.ops.into_iter().map(|o| o.body)).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultOp {
    List,
    Get,
    Put,
}

/// One reader's view of the shared remote.
#[derive(Default)]
struct View {
    /// New copies stay out of this reader's listings for this long.
    lag_ms: AtomicU64,
    hidden_prefixes: Mutex<Vec<String>>,
    /// Drive copies this reader does not see yet (listings and `get_all`).
    hidden_refs: Mutex<BTreeSet<String>>,
    /// `get` answers these bytes instead (a stale read).
    stale: Mutex<BTreeMap<String, Vec<u8>>>,
    faults: Mutex<VecDeque<(FaultOp, ProviderError)>>,
    /// Every put fails with this until cleared (a persistent quota error).
    sticky_put: Mutex<Option<ProviderError>>,
    /// The next N puts land but their response is lost.
    lose_put_responses: AtomicUsize,
    log_puts: AtomicUsize,
}

impl View {
    fn with_lag(ms: u64) -> Arc<View> {
        let view = View::default();
        view.lag_ms.store(ms, Ordering::SeqCst);
        Arc::new(view)
    }

    fn clear(&self) {
        self.lag_ms.store(0, Ordering::SeqCst);
        lock(&self.hidden_prefixes).clear();
        lock(&self.hidden_refs).clear();
        lock(&self.stale).clear();
        lock(&self.faults).clear();
        *lock(&self.sticky_put) = None;
        self.lose_put_responses.store(0, Ordering::SeqCst);
    }

    fn fault(&self, op: FaultOp) -> Result<(), ProviderError> {
        let mut faults = lock(&self.faults);
        match faults.iter().position(|(o, _)| *o == op) {
            Some(i) => Err(faults.remove(i).unwrap().1),
            None => Ok(()),
        }
    }
}

struct Lens<'a> {
    world: &'a World,
    view: &'a View,
}

impl Lens<'_> {
    fn visible(&self, o: &ListedObject) -> bool {
        if lock(&self.view.hidden_prefixes).iter().any(|p| o.key.starts_with(p.as_str())) {
            return false;
        }
        if lock(&self.view.hidden_refs).contains(&o.provider_ref) {
            return false;
        }
        let lag = self.view.lag_ms.load(Ordering::SeqCst);
        let born = lock(&self.world.born).get(&o.provider_ref).copied();
        born.is_none_or(|at| at + lag <= self.world.now())
    }

    fn before_put(&self, key: &str) -> Result<(), ProviderError> {
        if let Some(err) = lock(&self.view.sticky_put).clone() {
            return Err(err);
        }
        self.view.fault(FaultOp::Put)?;
        if key.contains("/log/") {
            self.view.log_puts.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn after_put(&self, created: ListedObject) -> Result<ListedObject, ProviderError> {
        lock(&self.world.born).insert(created.provider_ref.clone(), self.world.now());
        let lose = self.view.lose_put_responses.load(Ordering::SeqCst);
        if lose > 0 {
            self.view.lose_put_responses.store(lose - 1, Ordering::SeqCst);
            return Err(ProviderError::Transient("response lost".to_owned()));
        }
        Ok(created)
    }
}

impl ObjectStore for Lens<'_> {
    fn create_only(&self) -> bool {
        self.world.fake.create_only()
    }

    fn identity(&self) -> String {
        self.world.fake.identity()
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        self.view.fault(FaultOp::List)?;
        let listed = self.world.fake.list(q).await?;
        Ok(listed.into_iter().filter(|o| self.visible(o)).collect())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        self.view.fault(FaultOp::Get)?;
        let stale = lock(&self.view.stale).get(key).cloned();
        match stale {
            Some(bytes) => Ok(bytes),
            None => self.world.fake.get(key).await,
        }
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.before_put(key)?;
        let created = self.world.fake.put_create(key, bytes).await?;
        self.after_put(created)
    }

    async fn put_create_unchecked(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.before_put(key)?;
        let created = self.world.fake.put_create_unchecked(key, bytes).await?;
        self.after_put(created)
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        self.before_put(key)?;
        let replaced = self.world.fake.put_replace(key, bytes, if_match).await?;
        self.after_put(replaced)
    }

    async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        self.before_put(key)?;
        let replaced = self.world.fake.put_replace_ref(key, provider_ref, bytes).await?;
        self.after_put(replaced)
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        self.world.deletes.fetch_add(1, Ordering::SeqCst);
        self.world.fake.delete(key).await
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        self.view.fault(FaultOp::Get)?;
        let all = self.world.fake.get_all(key).await?;
        let hidden = lock(&self.view.hidden_refs).clone();
        Ok(all.into_iter().filter(|c| !hidden.contains(&c.provider_ref)).collect())
    }

    async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
        self.world.deletes.fetch_add(1, Ordering::SeqCst);
        self.world.fake.delete_ref(provider_ref).await
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        self.world.fake.quota().await
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.world.fake.ensure_container().await
    }
}

/// Every pause (settle, discovery) moves the shared test clock forward.
struct ClockPause<'a>(&'a AtomicU64);

impl Pause for ClockPause<'_> {
    async fn pause(&self, duration: Duration) {
        self.0.fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// One device

struct Dev {
    label: String,
    store: Mutex<Store>,
    clock: Mutex<HlcClock>,
    secrets: MemoryStore,
    engine: Engine,
    stats: SyncStats,
    vault: String,
    install_fp: Option<String>,
    view: Arc<View>,
}

impl Dev {
    /// A member of `vault` under a fixed device id, the DEK in its keychain.
    fn member(label: &str, vault: &str, device: &str, dek: &SecretKey) -> Dev {
        let secrets = MemoryStore::default();
        store_vault_key(&secrets, vault, dek).unwrap();
        let identity =
            DeviceIdentity { v: 1, vault_id: vault.to_owned(), device_id: device.to_owned(), install_fp: None, generation: 0 };
        save_identity(&secrets, &identity).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store.init_identity(&identity).unwrap();
        store.set_active_vault(vault).unwrap();
        Dev::from_parts(label, store, secrets, Arc::new(View::default()), HlcClock::default())
    }

    fn from_parts(label: &str, store: Store, secrets: MemoryStore, view: Arc<View>, clock: HlcClock) -> Dev {
        let vault = store.active_vault().unwrap().expect("an active vault");
        Dev {
            label: label.to_owned(),
            store: Mutex::new(store),
            clock: Mutex::new(clock),
            secrets,
            engine: Engine::default(),
            stats: SyncStats::default(),
            vault,
            install_fp: None,
            view,
        }
    }

    /// A VM / backup clone: same database bytes, same keychain, same clock.
    fn clone_into(&self, dir: &TempDir, label: &str) -> Dev {
        let path = dir.0.join(format!("{label}-{}.db", uuid::Uuid::new_v4()));
        lock(&self.store).conn().execute("VACUUM INTO ?1", [path.to_string_lossy().into_owned()]).unwrap();
        let secrets = MemoryStore::default();
        for account in self.secrets.accounts() {
            secrets.put_raw(&account, &self.secrets.raw(&account).unwrap());
        }
        let mut dev =
            Dev::from_parts(label, Store::open(&path).unwrap(), secrets, Arc::new(View::default()), *lock(&self.clock));
        dev.install_fp = self.install_fp.clone();
        dev
    }

    async fn cycle(&mut self, w: &World) -> CycleReport {
        let view = Arc::clone(&self.view);
        let provider = Bound::new(Lens { world: w, view: &view }, binding());
        let clock = Arc::clone(&w.now);
        let now = move || clock.load(Ordering::SeqCst);
        let pause = ClockPause(&w.now);
        let mut ctx = CycleCtx {
            provider: &provider,
            secrets: &self.secrets,
            store: &self.store,
            clock: &self.clock,
            pause: &pause,
            now: &now,
            stats: &self.stats,
            engine: &mut self.engine,
            vault: &self.vault,
            key_epoch: EPOCH,
            install_fp: self.install_fp.as_deref(),
            device_label: "sim device",
            app_version: "0.0.0-sim",
            checkpoint_dir: None,
            fence: None,
        };
        Engine::cycle(&mut ctx).await
    }

    /// A local op stamped by this device's HLC at the test clock.
    fn write(&self, w: &World, kind: Kind, entity: &str, op_type: &str, payload: Value) -> String {
        let now = w.now();
        let hlc = lock(&self.clock).tick(now);
        let n = w.next_op.fetch_add(1, Ordering::SeqCst) + 1;
        let mut rand = [0u8; 10];
        rand[2..].copy_from_slice(&n.to_be_bytes());
        let op_id = uuid::Builder::from_unix_timestamp_millis(now, &rand).into_uuid().to_string();
        let body = OpBody {
            v: 1,
            op_id,
            origin_device: "unset".to_owned(),
            hlc,
            ctx: BTreeMap::new(),
            obs: Vec::new(),
            kind,
            entity_id: entity.to_owned(),
            op_type: op_type.to_owned(),
            payload,
        };
        lock(&self.store).insert_local_op(&self.vault, body, UploadState::Never).unwrap().body.op_id
    }

    fn create_memo(&self, w: &World, id: &str, text: &str) -> String {
        self.write(w, Kind::Memo, id, "create", json!({"text": text, "pinned": false}))
    }

    /// An edit on top of the memo's current head here.
    fn edit_memo(&self, w: &World, id: &str, text: &str) -> String {
        let head = match self.entity(Kind::Memo, id) {
            Some(EntityState::Memo(m)) => m.head_op_id,
            other => panic!("{}: memo {id} is {other:?}", self.label),
        };
        self.write(w, Kind::Memo, id, "edit", json!({"text": text, "base_op_id": head}))
    }

    fn delete_memo(&self, w: &World, id: &str) -> String {
        self.write(w, Kind::Memo, id, "delete", json!({}))
    }

    fn device(&self) -> String {
        lock(&self.store).device_id(&self.vault).unwrap()
    }

    fn projection(&self) -> Projection {
        lock(&self.store).projection(&self.vault).unwrap()
    }

    fn hash(&self) -> [u8; 32] {
        self.projection().hash
    }

    fn entity(&self, kind: Kind, id: &str) -> Option<EntityState> {
        self.projection().entities.get(&(kind, id.to_owned())).cloned()
    }

    fn memo_text(&self, id: &str) -> Option<String> {
        match self.entity(Kind::Memo, id) {
            Some(EntityState::Memo(m)) => Some(m.text),
            _ => None,
        }
    }

    fn restored(&self, id: &str) -> bool {
        let notice = Notice::RestoredAfterDeleteConflict { kind: Kind::Memo, entity_id: id.to_owned() };
        self.projection().notices.contains(&notice)
    }

    fn quarantine(&self) -> Vec<(String, String)> {
        lock(&self.store).quarantine_rows(&self.vault).unwrap()
    }

    fn held_cursors(&self) -> Vec<String> {
        let store = lock(&self.store);
        let mut stmt = store.conn().prepare("SELECT remote_device_id FROM cursors WHERE vault_id = ?1").unwrap();
        let devices: Vec<String> =
            stmt.query_map([&self.vault], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        devices
            .into_iter()
            .filter(|d| store.cursor(&self.vault, d).unwrap().is_some_and(|c| c.state == CursorState::Held))
            .collect()
    }

    fn dek(&self) -> SecretKey {
        load_vault_key(&self.secrets, &self.vault).unwrap().unwrap()
    }

    /// I1: every op with a dot on one of this device's own ids has exactly one.
    fn assert_one_own_dot_each(&self, own: &[String]) {
        let store = lock(&self.store);
        let mut stmt = store.conn().prepare("SELECT op_id FROM op_dots WHERE vault_id = ?1 AND device_id = ?2").unwrap();
        for device in own {
            let ids: Vec<String> =
                stmt.query_map([&self.vault, device], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
            for id in ids {
                let dots = store.dots_of(&self.vault, &id).unwrap();
                let own_dots = dots.iter().filter(|d| own.contains(&d.device)).count();
                assert_eq!(own_dots, 1, "{}: op {id} has one own dot ({dots:?})", self.label);
            }
        }
    }
}

fn assert_ok(dev: &Dev, report: &CycleReport) {
    assert_eq!(report.status, CycleStatus::Ok, "{}: {report:?}", dev.label);
}

async fn ok_cycle(dev: &mut Dev, w: &World) -> CycleReport {
    let report = dev.cycle(w).await;
    assert_ok(dev, &report);
    report
}

fn rotation(report: &CycleReport) -> Option<Vec<&'static str>> {
    report.notices.iter().find_map(|n| match n {
        EngineNotice::Rotated { triggers } => Some(triggers.clone()),
        _ => None,
    })
}

/// Cycles every device round-robin (clock +1 s per cycle) until all
/// projection hashes agree and a whole round neither uploaded, failed, nor
/// changed any projection. (`applied_segments` is not used: a forked name is
/// re-applied as a no-op `Applied` every cycle, see `s3`.)
async fn quiesce(w: &World, devs: &mut [&mut Dev], rounds: usize) -> Vec<CycleReport> {
    let mut reports = Vec::new();
    for _ in 0..rounds {
        let mut moved = false;
        for dev in devs.iter_mut() {
            w.advance(1_000);
            let before = dev.hash();
            let report = dev.cycle(w).await;
            moved |= report.uploaded_segments > 0 || report.status != CycleStatus::Ok || dev.hash() != before;
            reports.push(report);
        }
        let first = devs[0].hash();
        if !moved && devs.iter().all(|d| d.hash() == first) {
            return reports;
        }
    }
    let summary: Vec<String> = devs.iter().map(|d| format!("{}={}", d.label, hex(&d.hash()))).collect();
    let tail: Vec<&CycleReport> = reports.iter().rev().take(devs.len()).collect();
    panic!("no quiescence after {rounds} rounds: {summary:?}; last reports {tail:?}");
}

fn assert_converged(devs: &[&Dev]) {
    let first = devs[0].hash();
    for dev in devs {
        assert_eq!(hex(&dev.hash()), hex(&first), "{} converged with {}", dev.label, devs[0].label);
    }
}

const M: &str = "memo-m";
const K: &str = "memo-k";

// ===========================================================================
// 1. AC 16b (N-1) clone-delete
// ===========================================================================

/// A′ is cloned from A and shares seq N = 1. A′ writes x (N+1) and z = delete
/// M (N+2); A publishes y = edit M at (A, N+1). A′ rotates before uploading;
/// z.ctx[A] = N; every device keeps M (with y) plus the restored notice; A′
/// applies A's stream with no quarantine and no rollback.
#[tokio::test]
async fn s1_ac16b_clone_delete_keeps_the_originals_unseen_edit_on_all_three() {
    let w = World::with_vault(true);
    let dek = SecretKey::from_bytes([5; 32]);
    let mut a = Dev::member("A", VAULT, "dev-a", &dek);
    let mut b = Dev::member("B", VAULT, "dev-b", &dek);
    let mut c = Dev::member("C", VAULT, "dev-c", &dek);
    a.create_memo(&w, M, "base");
    ok_cycle(&mut a, &w).await;
    ok_cycle(&mut b, &w).await;
    ok_cycle(&mut c, &w).await;

    let dir = TempDir::new("sim3-s1");
    let mut a2 = a.clone_into(&dir, "A'");

    w.advance(1_000);
    let y = a.edit_memo(&w, M, "y");
    ok_cycle(&mut a, &w).await;
    ok_cycle(&mut b, &w).await;

    w.advance(1_000);
    let x = a2.create_memo(&w, K, "x");
    let z = a2.delete_memo(&w, M);
    let report = a2.cycle(&w).await;
    assert_ok(&a2, &report);
    let triggers = rotation(&report).unwrap_or_else(|| panic!("A' rotates before upload: {report:?}"));
    assert!(!triggers.is_empty());
    let a2_id = a2.device();
    assert_ne!(a2_id, "dev-a");
    let mut devs = [&mut a, &mut b, &mut c, &mut a2];
    quiesce(&w, &mut devs, 8).await;
    assert_converged(&[&a, &b, &c, &a2]);
    assert!(w.remote_ops(&dek, VAULT, "dev-a").iter().all(|o| o.op_id != x && o.op_id != z), "nothing under the shared id");
    let z_body = w.remote_ops(&dek, VAULT, &a2_id).into_iter().find(|o| o.op_id == z).expect("z uploaded");
    assert_eq!(z_body.ctx.get("dev-a"), Some(&1), "z.ctx[A] = N");
    assert!(!z_body.obs.contains(&y), "z never observed y");
    for dev in [&a, &b, &c, &a2] {
        assert_eq!(dev.memo_text(M).as_deref(), Some("y"), "{}: M kept with the original's edit", dev.label);
        assert!(dev.restored(M), "{}: kept with a notice", dev.label);
        assert_eq!(dev.memo_text(K).as_deref(), Some("x"), "{}", dev.label);
        assert!(dev.quarantine().is_empty(), "{}: {:?}", dev.label, dev.quarantine());
        assert!(dev.held_cursors().is_empty(), "{}", dev.label);
    }
    let retired = lock(&a2.store).cursor(VAULT, "dev-a").unwrap().unwrap();
    assert_eq!((retired.state, retired.applied_seq), (CursorState::Retired, 2), "A' applied A's stream through y");
    a2.assert_one_own_dot_each(&["dev-a".to_owned(), a2_id]);
}

// -- Retired-stream re-read (G10 blocker found by s1/s5/s6, fixed in
// `store::apply_in`): the engine relists a retired own stream in full, so a
// covered range must be a no-op, not a chain break that holds the stream.

struct Rotated {
    w: World,
    dek: SecretKey,
    a: Dev,
    a2: Dev,
}

/// AC 16b setup: A′ (clone of A) rotated, then cycled twice more, so its
/// retired `dev-a` cursor has a head past seq 1.
async fn rotated_clone() -> Rotated {
    let w = World::with_vault(true);
    let dek = SecretKey::from_bytes([4; 32]);
    let mut a = Dev::member("A", VAULT, "dev-a", &dek);
    a.create_memo(&w, M, "base");
    ok_cycle(&mut a, &w).await;
    let dir = TempDir::new("sim3-retired");
    let mut a2 = a.clone_into(&dir, "A'");
    w.advance(1_000);
    a.edit_memo(&w, M, "y");
    ok_cycle(&mut a, &w).await;
    w.advance(1_000);
    a2.create_memo(&w, K, "x");
    a2.delete_memo(&w, M);
    let report = ok_cycle(&mut a2, &w).await;
    assert!(rotation(&report).is_some(), "{report:?}");
    for _ in 0..2 {
        w.advance(1_000);
        ok_cycle(&mut a2, &w).await;
    }
    Rotated { w, dek, a, a2 }
}

fn seg_bytes(w: &World, first: u64, last: u64) -> (ObjectAddress, Vec<u8>) {
    let address = ObjectAddress::segment(VAULT, EPOCH, "dev-a", first, last);
    let bytes = w.fake.copies(&address.remote_key().unwrap()).remove(0);
    (address, bytes)
}

/// (a) Re-reading the retired own stream after a rotation is a Duplicate:
/// no quarantine, the cursor is unchanged, nothing is held.
#[tokio::test]
async fn retired_stream_reread_after_rotation_is_a_noop() {
    let r = rotated_clone().await;
    assert!(r.a2.quarantine().is_empty(), "{:?}", r.a2.quarantine());
    assert!(r.a2.held_cursors().is_empty());
    let seg_key = derive_subkey(&r.dek, VAULT, EPOCH, Purpose::Segment);
    let before = lock(&r.a2.store).cursor(VAULT, "dev-a").unwrap().unwrap();
    assert_eq!((before.state, before.applied_seq), (CursorState::Retired, 2));
    assert!(before.chain_head.is_some(), "the head moved past seq 1");
    for (first, last) in [(1, 1), (2, 2)] {
        let (address, bytes) = seg_bytes(&r.w, first, last);
        let outcome = lock(&r.a2.store).apply_sealed(VAULT, &seg_key, &address, &bytes).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Duplicate), "seq {first}: {outcome:?}");
    }
    assert_eq!(lock(&r.a2.store).cursor(VAULT, "dev-a").unwrap().unwrap(), before, "cursor unchanged");
    assert!(r.a2.quarantine().is_empty());
}

/// (b) Other content under an already covered retired range is not a
/// Duplicate: it takes the existing chain path (held + quarantined).
#[tokio::test]
async fn retired_stream_other_content_at_a_covered_range_is_not_a_duplicate() {
    let r = rotated_clone().await;
    let seg_key = derive_subkey(&r.dek, VAULT, EPOCH, Purpose::Segment);
    let (address, bytes) = seg_bytes(&r.w, 1, 1);
    let mut forged = open_segment(&seg_key, &address, &bytes).unwrap();
    forged.ops[0].body.payload["text"] = json!("forged text");
    let forged = seal(&seg_key, &address, &forged.encode().unwrap()).unwrap();
    let state = lock(&r.a2.store).dump_state().unwrap();
    let outcome = lock(&r.a2.store).apply_sealed(VAULT, &seg_key, &address, &forged).unwrap();
    assert!(matches!(outcome, ApplyOutcome::Held { .. } | ApplyOutcome::Quarantined { .. }), "{outcome:?}");
    assert_ne!(lock(&r.a2.store).dump_state().unwrap(), state, "the existing path recorded it");
    assert!(r.a2.quarantine().iter().any(|(k, _)| *k == address.remote_key().unwrap()), "{:?}", r.a2.quarantine());
    assert_eq!(r.a2.memo_text(M).as_deref(), Some("y"), "the forged text never applied");
}

/// (c) A segment the original publishes later under the shared id reaches
/// the rotated clone (AC 16b continuation).
#[tokio::test]
async fn retired_stream_later_original_segments_apply_on_the_rotated_clone() {
    let mut r = rotated_clone().await;
    r.w.advance(1_000);
    r.a.edit_memo(&r.w, M, "y2 from the original");
    ok_cycle(&mut r.a, &r.w).await;
    assert_eq!(r.w.keys(&format!("v1/{VAULT}/log/dev-a/")).len(), 3);
    for _ in 0..2 {
        r.w.advance(1_000);
        ok_cycle(&mut r.a2, &r.w).await;
    }
    assert_eq!(r.a2.memo_text(M).as_deref(), Some("y2 from the original"));
    let cursor = lock(&r.a2.store).cursor(VAULT, "dev-a").unwrap().unwrap();
    assert_eq!((cursor.state, cursor.applied_seq), (CursorState::Retired, 3));
    assert!(r.a2.quarantine().is_empty(), "{:?}", r.a2.quarantine());
    let mut devs = [&mut r.a, &mut r.a2];
    quiesce(&r.w, &mut devs, 6).await;
    assert_converged(&[&r.a, &r.a2]);
}

// ===========================================================================
// 2. AC 16c
// ===========================================================================

/// An op written before the clone and never uploaded is published by both:
/// the clone (Migration Assistant, new install fingerprint) rotates on T2
/// before the original uploads and publishes it at (A′, 1); the original then
/// publishes it at (dev-a, 1). Every reader holds one logical op with two
/// dots: no copy, no duplicate, no quarantine. (When the original's copy is
/// already verified at rotation, S covers it and nothing is restamped.)
#[tokio::test]
async fn s2_ac16c_op_published_by_original_and_clone_is_one_logical_op() {
    let w = World::with_vault(true);
    let dek = SecretKey::from_bytes([6; 32]);
    let mut a = Dev::member("A", VAULT, "dev-a", &dek);
    let mut b = Dev::member("B", VAULT, "dev-b", &dek);
    let mut c = Dev::member("C", VAULT, "dev-c", &dek);
    let p = a.create_memo(&w, M, "shared before the clone");

    let dir = TempDir::new("sim3-s2");
    let mut a2 = a.clone_into(&dir, "A'");
    a2.install_fp = Some("install-new-mac".to_owned());
    let report = a2.cycle(&w).await;
    assert_ok(&a2, &report);
    assert_eq!(rotation(&report), Some(vec!["t2"]), "{report:?}");
    let a2_id = a2.device();
    w.advance(1_000);
    let report = ok_cycle(&mut a, &w).await;
    assert!(rotation(&report).is_none(), "the original keeps its id: {report:?}");
    let mut devs = [&mut a, &mut b, &mut c, &mut a2];
    quiesce(&w, &mut devs, 8).await;
    assert_converged(&[&a, &b, &c, &a2]);
    assert!(w.remote_ops(&dek, VAULT, "dev-a").iter().any(|o| o.op_id == p));
    assert!(w.remote_ops(&dek, VAULT, &a2_id).iter().any(|o| o.op_id == p), "the clone published it too");
    for dev in [&a, &b, &c, &a2] {
        let store = lock(&dev.store);
        let (set, _) = store.load_set(VAULT).unwrap();
        assert_eq!(set.len(), 1, "{}: one logical op", dev.label);
        let dots: BTreeSet<(String, u64)> =
            store.dots_of(VAULT, &p).unwrap().into_iter().map(|d| (d.device, d.seq)).collect();
        assert_eq!(dots, BTreeSet::from([("dev-a".to_owned(), 1), (a2_id.clone(), 1)]), "{}: aliased", dev.label);
        drop(store);
        let projection = dev.projection();
        assert!(projection.notices.iter().all(|n| !matches!(n, Notice::MemoConflictCopy { .. })), "{}", dev.label);
        assert_eq!(projection.entities.len(), 1, "{}: no duplicate", dev.label);
        assert!(dev.quarantine().is_empty(), "{}", dev.label);
    }
}

// ===========================================================================
// 3. AC 19b (N-2) VM fork on Drive
// ===========================================================================

/// On Drive, a VM clone passes T1–T4 (stale record, lagging listing) and
/// uploads its own (dev-a, 2) next to the original's. B sees one variant
/// first (per-reader lag) and deletes blindly; C sees both at once. The fork
/// is found and re-projected; all devices converge, no edit is lost, the
/// blind delete is restored; a re-delete that saw both then wins everywhere.
#[tokio::test]
async fn s3_ac19b_vm_fork_on_drive_converges_with_per_reader_lag() {
    let w = World::with_vault(false);
    let dek = SecretKey::from_bytes([7; 32]);
    let mut a = Dev::member("A", VAULT, "dev-a", &dek);
    let mut b = Dev::member("B", VAULT, "dev-b", &dek);
    let mut c = Dev::member("C", VAULT, "dev-c", &dek);
    b.create_memo(&w, M, "base");
    ok_cycle(&mut b, &w).await;
    ok_cycle(&mut a, &w).await;
    ok_cycle(&mut c, &w).await;
    let rec_key = ObjectAddress::device_record(VAULT, EPOCH, "dev-a").remote_key().unwrap();
    let stale_rec = w.fake.copies(&rec_key).remove(0);

    let dir = TempDir::new("sim3-s3");
    let mut a2 = a.clone_into(&dir, "A'");
    w.advance(1_000);
    a.edit_memo(&w, M, "variant one");
    w.advance(1_000);
    a2.edit_memo(&w, M, "variant two");
    ok_cycle(&mut a, &w).await;

    // A′ races A: its listing lags and its record read is stale (T1–T4 pass).
    lock(&a2.view.hidden_prefixes).push(format!("v1/{VAULT}/log/dev-a/"));
    lock(&a2.view.stale).insert(rec_key, stale_rec);
    let report = a2.cycle(&w).await;
    assert_ok(&a2, &report);
    assert!(rotation(&report).is_none(), "T1–T4 pass: {report:?}");
    let seg = ObjectAddress::segment(VAULT, EPOCH, "dev-a", 1, 1).remote_key().unwrap();
    assert_eq!(w.fake.copies(&seg).len(), 2, "two objects under one name");

    // B lags: it sees only the original's copy, then deletes M blindly.
    let refs: Vec<String> = block(w.fake.list(ListQuery::Props {
        vault_id: Some(VAULT),
        kind: Some(RemoteKind::Seg),
        device_id: Some("dev-a"),
    }))
    .unwrap()
    .into_iter()
    .filter(|o| o.key == seg)
    .map(|o| o.provider_ref)
    .collect();
    assert_eq!(refs.len(), 2);
    lock(&b.view.hidden_refs).insert(refs[1].clone());
    ok_cycle(&mut b, &w).await;
    assert_eq!(b.memo_text(M).as_deref(), Some("variant one"), "B applied one variant first");
    w.advance(1_000);
    b.delete_memo(&w, M);
    ok_cycle(&mut b, &w).await;

    // C reads everything at once.
    ok_cycle(&mut c, &w).await;

    // B's view catches up: fork detected, re-projected.
    b.view.clear();
    let report = b.cycle(&w).await;
    assert_ok(&b, &report);
    assert!(report.notices.contains(&EngineNotice::ForkDetected), "{report:?}");
    assert!(report.notices.contains(&EngineNotice::Reprojected), "{report:?}");

    a2.view.clear();
    let mut devs = [&mut a, &mut b, &mut c, &mut a2];
    quiesce(&w, &mut devs, 10).await;
    assert_converged(&[&a, &b, &c, &a2]);
    let texts = |dev: &Dev| -> BTreeSet<String> {
        dev.projection()
            .entities
            .values()
            .filter_map(|s| match s {
                EntityState::Memo(m) => Some(m.text.clone()),
                _ => None,
            })
            .collect()
    };
    for dev in [&b, &c] {
        assert!(lock(&dev.store).forks(VAULT).unwrap().contains_key("dev-a"), "{}: forked cursor", dev.label);
        assert!(dev.quarantine().is_empty(), "{}: {:?}", dev.label, dev.quarantine());
    }
    for dev in [&a, &b, &c, &a2] {
        assert_eq!(dev.memo_text(M).as_deref(), Some("variant two"), "{}: blind delete restored", dev.label);
        assert!(texts(dev).contains("variant one"), "{}: no edit lost", dev.label);
        assert!(dev.restored(M), "{}", dev.label);
    }

    // B re-deletes, having seen both variants: the delete converges.
    let copies: Vec<String> = b
        .projection()
        .entities
        .iter()
        .filter_map(|((kind, id), s)| match s {
            EntityState::Memo(_) if *kind == Kind::Memo => Some(id.clone()),
            _ => None,
        })
        .collect();
    w.advance(1_000);
    for id in &copies {
        b.delete_memo(&w, id);
    }
    let mut devs = [&mut a, &mut b, &mut c, &mut a2];
    quiesce(&w, &mut devs, 10).await;
    assert_converged(&[&a, &b, &c, &a2]);
    for dev in [&a, &b, &c, &a2] {
        for id in &copies {
            assert_eq!(dev.entity(Kind::Memo, id), Some(EntityState::Deleted), "{}: {id}", dev.label);
        }
    }
}

// ===========================================================================
// 4. AC 20 (+N-3) concurrent first setup, then join
// ===========================================================================

const M_SAME: &str = "memo-same";
const M_CONFLICT: &str = "memo-conflict";
const M_ONLY_B: &str = "memo-only-b";
const M_DEAD: &str = "memo-dead";
const CONV_B: &str = "conv-b";
const TR_B: &str = "tr-b";

struct Created {
    store: Store,
    secrets: MemoryStore,
    clock: HlcClock,
    vault: String,
    rk: String,
}

/// The whole create flow through `view`: discovery must answer None.
async fn create_vault(w: &World, view: &View) -> Created {
    let lens = Lens { world: w, view };
    let mut store = Store::open_in_memory().unwrap();
    let secrets = MemoryStore::default();
    let found = discover(&lens, &ClockPause(&w.now), || w.now()).await;
    assert_eq!(found, Discovery::None, "discovery sees no vault yet");
    record_discovery(&mut store, &found, w.now()).unwrap();
    let mut pending = begin_create(&store, &binding(), w.now()).unwrap();
    let rk = pending.recovery_key().to_display().to_string();
    confirm_create(&mut pending, &secrets, &rk).unwrap();
    publish_created(&lens, &secrets, &pending).await.unwrap();
    let mut clock = HlcClock::default();
    activate_created(&mut store, &secrets, &pending, None, &mut clock, w.now()).unwrap();
    let vault = pending.vault_id().to_owned();
    Created { store, secrets, clock, vault, rk }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Two devices set up concurrently under a 15 s list lag: two isolated
/// vaults, no automatic winner, zero deletes, the two-vault banner. B then
/// joins A's vault: a crash before J7 leaves V1 intact; after the join B's
/// V2 id differs, its V2 stream starts at seq 1 on the genesis chain, V2
/// holds no V1 op id / device id / tombstone, every V1 live entity is in V2,
/// no V1 tombstone target reappears, and all three devices converge.
#[tokio::test]
async fn s4_ac20_concurrent_first_setup_two_vaults_then_join_without_v1_leak() {
    for create_only in [true, false] {
        let w = World::new(create_only);
        let a_view = View::with_lag(15_000);
        let b_view = View::with_lag(15_000);

        // A: discovery at t0 and t0+5 s finds nothing; A creates V_A.
        let ca = create_vault(&w, &a_view).await;
        // B starts 1 s later: its listings at +0 and +5 s miss A's manifest.
        w.advance(1_000);
        let cb = create_vault(&w, &b_view).await;
        assert_ne!(ca.vault, cb.vault, "two isolated vaults");
        let (va, vb, rk_a) = (ca.vault.clone(), cb.vault.clone(), ca.rk.clone());
        let mut a = Dev::from_parts("A", ca.store, ca.secrets, Arc::clone(&a_view), ca.clock);
        let mut b = Dev::from_parts("B", cb.store, cb.secrets, Arc::clone(&b_view), cb.clock);
        let mut c = Dev::member("C", &va, "dev-c", &a.dek());
        c.view = View::with_lag(15_000);

        a.create_memo(&w, M_SAME, "same everywhere");
        a.create_memo(&w, M_CONFLICT, "A's version");
        b.create_memo(&w, M_SAME, "same everywhere");
        b.create_memo(&w, M_CONFLICT, "B's version");
        b.create_memo(&w, M_ONLY_B, "only on B");
        b.create_memo(&w, M_DEAD, "deleted on B");
        b.delete_memo(&w, M_DEAD);
        b.write(&w, Kind::Conv, CONV_B, "create", json!({"title": "chat"}));
        b.write(&w, Kind::Conv, CONV_B, "msg.add", json!({"conv_id": CONV_B, "msg_id": "m-1", "role": "user", "content": "hi", "attachments": [], "ts": 1}));
        b.write(&w, Kind::Conv, CONV_B, "msg.add", json!({"conv_id": CONV_B, "msg_id": "m-2", "parent_id": "m-1", "role": "assistant", "content": "yo", "attachments": [], "ts": 2}));
        b.write(&w, Kind::Tr, TR_B, "tr.add", json!({"source": "가", "target": "a", "pinned": true}));

        for dev in [&mut a, &mut b, &mut c] {
            let report = dev.cycle(&w).await;
            assert_ok(dev, &report);
            assert!(!report.notices.iter().any(|n| matches!(n, EngineNotice::OtherVaults { .. })), "lagging: {report:?}");
        }
        // Past the lag: every device sees the other vault, nothing picks a winner.
        w.advance(20_000);
        for dev in [&mut a, &mut b, &mut c] {
            let report = dev.cycle(&w).await;
            assert_ok(dev, &report);
            assert!(report.notices.contains(&EngineNotice::OtherVaults { count: 1 }), "{}: banner {report:?}", dev.label);
        }
        assert_eq!(lock(&a.store).active_vault().unwrap().as_deref(), Some(va.as_str()));
        assert_eq!(lock(&b.store).active_vault().unwrap().as_deref(), Some(vb.as_str()));
        assert_eq!(w.deletes.load(Ordering::SeqCst), 0, "zero deletes");
        assert_eq!(w.keys(&manifest_key(&va).unwrap()).len(), 1);
        assert_eq!(w.keys(&manifest_key(&vb).unwrap()).len(), 1);

        // B joins V_A with A's RK.
        let v1_device = b.device();
        let v1_ops: Vec<String> = {
            let store = lock(&b.store);
            let mut stmt = store.conn().prepare("SELECT op_id FROM ops WHERE vault_id = ?1").unwrap();
            stmt.query_map([&vb], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        let v1_live: Vec<((Kind, String), EntityState)> = lock(&b.store)
            .projection(&vb)
            .unwrap()
            .entities
            .into_iter()
            .filter(|(_, s)| *s != EntityState::Deleted)
            .collect();
        assert_eq!(v1_live.len(), 5, "three live memos, the conversation, the translation: {v1_live:?}");
        let lens = Lens { world: &w, view: &b_view };
        let found = discover(&lens, &ClockPause(&w.now), || w.now()).await;
        let Discovery::Found(ids) = found else { panic!("B finds both vaults: {found:?}") };
        assert_eq!(ids.len(), 2);
        let candidates = fetch_candidates(&lens, &ids).await.unwrap();
        let unlocked = {
            let store = lock(&b.store);
            unlock_join(&store, &b.secrets, &candidates, &rk_a, &binding()).unwrap()
        };
        assert_eq!(unlocked.vault_id, va);
        let target = fetch_target(&lens, &va).await.unwrap();
        let preview = {
            let mut store = lock(&b.store);
            apply_target(&mut store, &b.secrets, &unlocked, &target).unwrap();
            preview_join(&store, &b.secrets, &unlocked).unwrap()
        };

        // A crash before J7 leaves V1 exactly as it was.
        let before = lock(&b.store).dump_state().unwrap();
        CRASH_BEFORE_COMMIT.with(|crash| crash.set(true));
        let crashed = {
            let mut store = lock(&b.store);
            let mut clock = lock(&b.clock);
            commit_join(&mut store, &b.secrets, &unlocked, &preview, None, &mut clock, w.now())
        };
        assert!(matches!(crashed, Err(VaultError::JoinRejected)), "{crashed:?}");
        assert_eq!(lock(&b.store).dump_state().unwrap(), before, "V1 intact after a crash before J7");
        assert_eq!(lock(&b.store).active_vault().unwrap().as_deref(), Some(vb.as_str()));

        let commit = {
            let mut store = lock(&b.store);
            let mut clock = lock(&b.clock);
            commit_join(&mut store, &b.secrets, &unlocked, &preview, None, &mut clock, w.now()).unwrap()
        };
        assert_ne!(commit.device_id, v1_device, "V2 device id differs from V1");
        b.vault = va.clone();
        b.engine = Engine::default();
        let v2_device = b.device();
        assert_eq!(v2_device, commit.device_id);

        w.advance(20_000);
        let mut devs = [&mut a, &mut b, &mut c];
        let reports = quiesce(&w, &mut devs, 8).await;
        assert!(reports.iter().all(|r| r.status == CycleStatus::Ok), "{reports:?}");
        assert_converged(&[&a, &b, &c]);

        // V2 stream: seq 1, genesis chain; readers apply without hold.
        let segments = w.segments(&a.dek(), &va, &v2_device);
        let first = segments.iter().find(|s| s.first_seq == 1).expect("B's V2 stream starts at seq 1");
        assert_eq!(first.prev_seg_hash, hex(&genesis_chain_head(&va, &v2_device)));
        for dev in [&a, &c] {
            let cursor = lock(&dev.store).cursor(&va, &v2_device).unwrap().unwrap();
            assert_eq!(cursor.state, CursorState::Ok, "{}", dev.label);
            assert!(dev.held_cursors().is_empty());
        }

        // Byte scan of every V2 object (raw and decrypted).
        let mut forbidden = v1_ops.clone();
        forbidden.extend([v1_device.clone(), M_DEAD.to_owned(), vb.clone()]);
        let dek = a.dek();
        let seg_key = derive_subkey(&dek, &va, EPOCH, Purpose::Segment);
        for key in w.keys(&format!("v1/{va}/")) {
            for bytes in w.fake.copies(&key) {
                let mut texts = vec![bytes.clone()];
                if let Some(address) = ObjectAddress::from_remote_key(&key, EPOCH) {
                    match address.kind {
                        ObjectKind::Segment => texts.push(open_segment(&seg_key, &address, &bytes).unwrap().encode().unwrap()),
                        ObjectKind::DeviceRecord => {
                            let record = open_record(&dek, &va, EPOCH, &address.device_id, &bytes).unwrap();
                            texts.push(serde_json::to_vec(&record).unwrap());
                        }
                        _ => {}
                    }
                }
                for text in &texts {
                    for needle in &forbidden {
                        assert!(!contains(text, needle), "{key} leaks V1 value {needle}");
                    }
                }
            }
        }
        for table in ["ops", "op_dots", "cursors", "forks", "retired_devices", "remote_index"] {
            let store = lock(&b.store);
            let mut stmt = store.conn().prepare(&format!("SELECT * FROM {table} WHERE vault_id = ?1")).unwrap();
            let n = stmt.column_count();
            let rows: Vec<String> = stmt
                .query_map([&va], |r| {
                    (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| format!("{v:?}"))).collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .map(|row| row.unwrap().join("|"))
                .collect();
            for row in rows {
                for needle in &forbidden {
                    assert!(!row.contains(needle.as_str()), "{table} V2 row leaks {needle}");
                }
            }
        }

        // Zero loss: every V1 live entity is in V2 on every device (a
        // conflicting id as a copy); no V1 tombstone target reappears.
        for dev in [&a, &b, &c] {
            let v2 = dev.projection().entities;
            for ((kind, id), state) in &v1_live {
                let item = preview.items.iter().find(|i| i.kind == *kind && &i.source_id == id).unwrap();
                let there = v2.get(&(*kind, item.target_id.clone())).unwrap_or_else(|| panic!("{}: {id} lost", dev.label));
                match (state, there) {
                    (EntityState::Memo(src), EntityState::Memo(dst)) => assert_eq!(src.text, dst.text, "{}: {id}", dev.label),
                    (EntityState::Conv(src), EntityState::Conv(dst)) => {
                        assert_eq!(src.messages.keys().collect::<Vec<_>>(), dst.messages.keys().collect::<Vec<_>>())
                    }
                    (EntityState::Tr(_), EntityState::Tr(_)) => {}
                    other => panic!("{}: {id} changed kind {other:?}", dev.label),
                }
                if item.action == JoinAction::Copy {
                    assert_ne!(item.target_id, *id);
                }
            }
            assert!(!v2.contains_key(&(Kind::Memo, M_DEAD.to_owned())), "{}: tombstone target reappeared", dev.label);
            assert_eq!(dev.memo_text(M_CONFLICT).as_deref(), Some("A's version"), "V2 content never overwritten");
        }
        assert_eq!(w.deletes.load(Ordering::SeqCst), 0, "zero deletes");
        assert!(w.keys(&manifest_key(&vb).unwrap()).len() == 1, "V1 is left for the user");
    }
}

// ===========================================================================
// 5. Rollback (backup restore)
// ===========================================================================

/// A device restored from an older local store (the keychain is current)
/// detects the rollback triggers, rotates, re-reads what it published after
/// the backup from its retired stream, keeps its post-restore edits, and
/// nothing is lost on any device.
#[tokio::test]
async fn s5_rollback_backup_restore_rotates_and_loses_nothing() {
    let w = World::with_vault(true);
    let dek = SecretKey::from_bytes([8; 32]);
    let mut a = Dev::member("A", VAULT, "dev-a", &dek);
    let mut b = Dev::member("B", VAULT, "dev-b", &dek);
    let mut c = Dev::member("C", VAULT, "dev-c", &dek);
    a.create_memo(&w, M, "one");
    ok_cycle(&mut a, &w).await;

    let dir = TempDir::new("sim3-s5");
    let backup = dir.0.join("backup.db");
    lock(&a.store).conn().execute("VACUUM INTO ?1", [backup.to_string_lossy().into_owned()]).unwrap();

    w.advance(1_000);
    a.edit_memo(&w, M, "two");
    ok_cycle(&mut a, &w).await;
    w.advance(1_000);
    a.create_memo(&w, K, "after the backup");
    ok_cycle(&mut a, &w).await;
    ok_cycle(&mut b, &w).await;
    ok_cycle(&mut c, &w).await;

    // Restore: the old database comes back; keychain and clock are current.
    let secrets = std::mem::take(&mut a.secrets);
    let clock = *lock(&a.clock);
    let mut a = Dev::from_parts("A(restored)", Store::open(&backup).unwrap(), secrets, Arc::new(View::default()), clock);
    w.advance(1_000);
    a.create_memo(&w, "memo-after-restore", "typed after the restore");

    let report = a.cycle(&w).await;
    assert_ok(&a, &report);
    let triggers = rotation(&report).unwrap_or_else(|| panic!("restore detected: {report:?}"));
    assert!(triggers.iter().any(|t| ["t3", "t4", "t5"].contains(t)), "{triggers:?}");
    let new_id = a.device();
    assert_ne!(new_id, "dev-a");

    let mut devs = [&mut a, &mut b, &mut c];
    quiesce(&w, &mut devs, 8).await;
    assert_converged(&[&a, &b, &c]);
    for dev in [&a, &b, &c] {
        assert_eq!(dev.memo_text(M).as_deref(), Some("two"), "{}", dev.label);
        assert_eq!(dev.memo_text(K).as_deref(), Some("after the backup"), "{}", dev.label);
        assert_eq!(dev.memo_text("memo-after-restore").as_deref(), Some("typed after the restore"), "{}", dev.label);
        assert!(dev.quarantine().is_empty(), "{}: {:?}", dev.label, dev.quarantine());
        assert!(dev.held_cursors().is_empty(), "{}", dev.label);
    }
    assert_eq!(w.remote_ops(&dek, VAULT, "dev-a").len(), 3, "the retired stream is untouched");
    a.assert_one_own_dot_each(&["dev-a".to_owned(), new_id]);
}

// ===========================================================================
// 6. W-2: persistent put failure during a pending T2
// ===========================================================================

/// A persistent quota error on every put while a T2 rotation is pending: no
/// new upload, no restamp, no rotation, and the "upload outcome unknown"
/// notice; the other devices keep syncing. Once the quota clears, the
/// attempted segment settles under the old id, the device rotates (T2), and
/// all three converge with nothing lost.
#[tokio::test]
async fn s6_w2_persistent_quota_during_pending_t2_pauses_notices_then_recovers() {
    let w = World::with_vault(true);
    let dek = SecretKey::from_bytes([9; 32]);
    let mut a = Dev::member("A", VAULT, "dev-a", &dek);
    let mut b = Dev::member("B", VAULT, "dev-b", &dek);
    let mut c = Dev::member("C", VAULT, "dev-c", &dek);
    let first = a.create_memo(&w, M, "x");
    *lock(&a.view.sticky_put) = Some(ProviderError::QuotaExceeded);
    let report = a.cycle(&w).await;
    assert!(matches!(report.status, CycleStatus::Failed { .. }), "{report:?}");
    assert_eq!(lock(&a.store).op_state(VAULT, &first).unwrap().unwrap().2, UploadState::Attempted);

    a.install_fp = Some("install-after-migration".to_owned());
    w.advance(1_000);
    let later = a.create_memo(&w, "memo-later", "later");
    b.create_memo(&w, "memo-b", "from B");
    let mut noticed = false;
    for _ in 0..=A3_NOTICE_CYCLES {
        w.advance(1_000);
        ok_cycle(&mut b, &w).await;
        ok_cycle(&mut c, &w).await;
        let report = a.cycle(&w).await;
        assert!(rotation(&report).is_none(), "no rotation while an outcome is unknown: {report:?}");
        noticed |= report.notices.contains(&EngineNotice::UploadOutcomeUnknown);
    }
    assert!(noticed, "paused notice after {A3_NOTICE_CYCLES} cycles");
    assert_eq!(a.device(), "dev-a");
    assert!(w.keys(&format!("v1/{VAULT}/log/dev-a/")).is_empty(), "no upload landed");
    {
        let store = lock(&a.store);
        assert_eq!(store.dots_of(VAULT, &first).unwrap().len(), 1, "no restamp");
        assert_eq!(store.pending_segments(VAULT, "dev-a").unwrap().len(), 1, "nothing new sealed");
    }
    assert_eq!(a.memo_text("memo-b").as_deref(), Some("from B"), "catch-up continues");
    assert_eq!(b.hash(), c.hash());

    // The quota clears.
    *lock(&a.view.sticky_put) = None;
    let mut rotated = None;
    for _ in 0..3 {
        w.advance(1_000);
        let report = a.cycle(&w).await;
        assert!(!matches!(report.status, CycleStatus::Stopped { .. }), "{report:?}");
        if let Some(t) = rotation(&report) {
            rotated = Some(t);
            break;
        }
    }
    assert_eq!(rotated, Some(vec!["t2"]), "the pending T2 rotation runs once settled");
    let new_id = a.device();
    let mut devs = [&mut a, &mut b, &mut c];
    quiesce(&w, &mut devs, 8).await;
    assert_converged(&[&a, &b, &c]);
    assert!(w.remote_ops(&dek, VAULT, "dev-a").iter().any(|o| o.op_id == first), "settled under the old id");
    assert!(w.remote_ops(&dek, VAULT, &new_id).iter().any(|o| o.op_id == later), "restamped under the new id");
    for dev in [&a, &b, &c] {
        assert_eq!(dev.memo_text(M).as_deref(), Some("x"));
        assert_eq!(dev.memo_text("memo-later").as_deref(), Some("later"));
        assert_eq!(dev.memo_text("memo-b").as_deref(), Some("from B"));
        assert!(dev.quarantine().is_empty(), "{}: {:?}", dev.label, dev.quarantine());
    }
}

// ===========================================================================
// 7. Seeded random soak
// ===========================================================================

const SEEDS: [u64; 20] = [
    0x5EED_0001, 0x5EED_0002, 0x5EED_0003, 0x5EED_0004, 0x5EED_0005, 0x5EED_0006, 0x5EED_0007, 0x5EED_0008,
    0x5EED_0009, 0x5EED_000A, 0x5EED_000B, 0x5EED_000C, 0x5EED_000D, 0x5EED_000E, 0x5EED_000F, 0x5EED_0010,
    0x5EED_0011, 0x5EED_0012, 0x5EED_0013, 0x5EED_0014,
];
const STEPS: usize = 200;

/// An op's content without its op id, ctx, or base reference (a reissue
/// keeps the payload but may take a fresh op id and a repointed base).
fn signature(body: &OpBody) -> String {
    let mut payload = body.payload.clone();
    if let Some(map) = payload.as_object_mut() {
        map.remove("base_op_id");
    }
    format!("{}/{}/{}/{}", body.kind.as_str(), body.entity_id, body.op_type, payload)
}

async fn join_fresh(w: &World, label: &str, rk: &str) -> Result<Dev, String> {
    let view = Arc::new(View::default());
    let lens = Lens { world: w, view: &view };
    let mut store = Store::open_in_memory().unwrap();
    let secrets = MemoryStore::default();
    let mut clock = HlcClock::default();
    let ids = match discover(&lens, &ClockPause(&w.now), || w.now()).await {
        Discovery::Found(ids) => ids,
        other => return Err(format!("discovery {other:?}")),
    };
    let candidates = fetch_candidates(&lens, &ids).await.map_err(|e| format!("{e:?}"))?;
    let unlocked = unlock_join(&store, &secrets, &candidates, rk, &binding()).map_err(|e| format!("{e:?}"))?;
    let target = fetch_target(&lens, &unlocked.vault_id).await.map_err(|e| format!("{e:?}"))?;
    apply_target(&mut store, &secrets, &unlocked, &target).map_err(|e| format!("{e:?}"))?;
    let preview = preview_join(&store, &secrets, &unlocked).map_err(|e| format!("{e:?}"))?;
    commit_join(&mut store, &secrets, &unlocked, &preview, None, &mut clock, w.now()).map_err(|e| format!("{e:?}"))?;
    Ok(Dev::from_parts(label, store, secrets, Arc::clone(&view), clock))
}

struct Soak {
    seed: u64,
    rng: SimRng,
    w: World,
    rk: String,
    devs: Vec<Dev>,
    signatures: BTreeSet<String>,
    created: BTreeSet<(Kind, String)>,
    deleted: BTreeSet<(Kind, String)>,
    messages: BTreeMap<String, BTreeSet<String>>,
    next_id: u64,
    compactions: u32,
    bad: Vec<String>,
    dir: TempDir,
}

impl Soak {
    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{:x}-{}", self.seed, self.next_id)
    }

    fn record(&mut self, dev: usize, op_id: &str) {
        let store = lock(&self.devs[dev].store);
        let (set, _) = store.load_set(&self.devs[dev].vault).unwrap();
        self.signatures.insert(signature(&set.get(op_id).unwrap().body));
    }

    fn write_random(&mut self, d: usize) {
        let roll = self.rng.below(100);
        let projection = self.devs[d].projection();
        let live = |kind: Kind| -> Vec<String> {
            projection
                .entities
                .iter()
                .filter(|((k, _), s)| *k == kind && **s != EntityState::Deleted)
                .map(|((_, id), _)| id.clone())
                .collect()
        };
        let (memos, convs, trs) = (live(Kind::Memo), live(Kind::Conv), live(Kind::Tr));
        let op_id = if roll < 25 || (roll < 50 && memos.is_empty()) {
            let id = self.id("memo");
            let text = format!("text {}", self.rng.next() % 1000);
            self.created.insert((Kind::Memo, id.clone()));
            self.devs[d].create_memo(&self.w, &id, &text)
        } else if roll < 50 {
            let id = self.rng.pick(&memos).unwrap().clone();
            let text = format!("edit {}", self.rng.next() % 1000);
            self.devs[d].edit_memo(&self.w, &id, &text)
        } else if roll < 56 && !memos.is_empty() {
            let id = self.rng.pick(&memos).unwrap().clone();
            self.deleted.insert((Kind::Memo, id.clone()));
            self.devs[d].delete_memo(&self.w, &id)
        } else if roll < 66 || (roll < 85 && convs.is_empty()) {
            let id = self.id("conv");
            self.created.insert((Kind::Conv, id.clone()));
            self.devs[d].write(&self.w, Kind::Conv, &id, "create", json!({"title": "t"}))
        } else if roll < 85 {
            let conv = self.rng.pick(&convs).unwrap().clone();
            let msg = self.id("msg");
            let parent = self.messages.get(&conv).and_then(|m| m.iter().next_back().cloned());
            let mut payload = json!({"conv_id": conv, "msg_id": msg, "role": "user", "content": "c", "attachments": [], "ts": self.next_id});
            if let Some(parent) = parent {
                payload["parent_id"] = json!(parent);
            }
            self.messages.entry(conv.clone()).or_default().insert(msg);
            self.devs[d].write(&self.w, Kind::Conv, &conv, "msg.add", payload)
        } else if roll < 95 || trs.is_empty() {
            let id = self.id("tr");
            self.created.insert((Kind::Tr, id.clone()));
            self.devs[d].write(&self.w, Kind::Tr, &id, "tr.add", json!({"source": "s", "target": "t", "pinned": false}))
        } else {
            let id = self.rng.pick(&trs).unwrap().clone();
            self.deleted.insert((Kind::Tr, id.clone()));
            self.devs[d].write(&self.w, Kind::Tr, &id, "tr.delete", json!({}))
        };
        self.record(d, &op_id);
    }

    fn inject_fault(&mut self, d: usize) {
        let view = Arc::clone(&self.devs[d].view);
        match self.rng.below(6) {
            0 => lock(&view.faults).push_back((FaultOp::List, ProviderError::Transient("503".to_owned()))),
            1 => lock(&view.faults).push_back((FaultOp::Get, ProviderError::Transient("500".to_owned()))),
            2 => lock(&view.faults)
                .push_back((FaultOp::Put, ProviderError::RateLimited { retry_after: Some(Duration::from_secs(2)) })),
            3 => lock(&view.faults).push_back((FaultOp::Put, ProviderError::Transient("502".to_owned()))),
            4 => lock(&view.faults).push_back((FaultOp::Put, ProviderError::QuotaExceeded)),
            _ => {
                view.lose_put_responses.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    async fn cycle(&mut self, d: usize) {
        let report = self.devs[d].cycle(&self.w).await;
        if report.notices.contains(&EngineNotice::StreamHeld) {
            let dev = &self.devs[d];
            let all: Vec<String> = self.w.keys(&format!("v1/{}/", dev.vault)).into_iter().filter(|k| !k.contains("/devices/")).collect();
            let copies: Vec<(String, usize)> = all.iter().map(|k| (k.clone(), self.w.fake.copies(k).len())).collect();
            eprintln!("DEBUG held at {} by {}: {report:?} held={:?} remote={copies:?} lag={}", self.w.now(), dev.label, dev.held_cursors(), dev.view.lag_ms.load(Ordering::SeqCst));
        }
        match report.status {
            CycleStatus::Stopped { .. } | CycleStatus::Failed { class: crate::sync::telemetry::FailureClass::Local } => {
                self.bad.push(format!("{}: {report:?}", self.devs[d].label));
            }
            _ => {}
        }
    }

    async fn run(seed: u64) -> Soak {
        let mut rng = SimRng(seed);
        let w = World::new(rng.below(2) == 0);
        let dir = TempDir::new("sim3-soak");
        let a_view = Arc::new(View::default());
        let created = create_vault(&w, &a_view).await;
        let rk = created.rk.clone();
        let a = Dev::from_parts("A", created.store, created.secrets, a_view, created.clock);
        let mut devs = vec![a];
        for label in ["B", "C"] {
            devs.push(join_fresh(&w, label, &rk).await.unwrap_or_else(|e| panic!("seed {seed}: {label} joins: {e}")));
        }
        let clone_at = 40 + rng.below(80) as usize;
        let join_at = 80 + rng.below(80) as usize;
        let mut soak = Soak {
            seed,
            rng,
            w,
            rk,
            devs,
            signatures: BTreeSet::new(),
            created: BTreeSet::new(),
            deleted: BTreeSet::new(),
            messages: BTreeMap::new(),
            next_id: 0,
            compactions: 0,
            bad: Vec::new(),
            dir,
        };
        let mut cloned = false;
        let mut joined = false;
        for step in 0..STEPS {
            soak.w.advance(200 + soak.rng.below(1_800));
            if step >= clone_at && !cloned {
                let from = soak.rng.below(soak.devs.len() as u64) as usize;
                let label = format!("{}'", soak.devs[from].label);
                let mut clone = soak.devs[from].clone_into(&soak.dir, &label);
                if soak.rng.below(2) == 0 {
                    // Migration Assistant: another install fingerprint (T2).
                    clone.install_fp = Some(format!("install-{label}"));
                }
                soak.devs.push(clone);
                cloned = true;
            }
            if step >= join_at && !joined {
                match join_fresh(&soak.w, "D", &soak.rk).await {
                    Ok(dev) => {
                        soak.devs.push(dev);
                        joined = true;
                    }
                    Err(_) if step + 1 < STEPS => {}
                    Err(e) => panic!("seed {seed}: join never succeeded: {e}"),
                }
            }
            let d = soak.rng.below(soak.devs.len() as u64) as usize;
            match soak.rng.below(100) {
                0..=29 => soak.write_random(d),
                30..=64 => soak.cycle(d).await,
                65..=71 => {
                    let lag = [0, 15_000, 60_000][soak.rng.below(3) as usize];
                    soak.devs[d].view.lag_ms.store(lag, Ordering::SeqCst);
                }
                72..=78 => soak.inject_fault(d),
                79..=85 => soak.w.advance(1_000 + soak.rng.below(29_000)),
                86..=87 if soak.compactions < 2 => {
                    soak.compactions += 1;
                    soak.w.advance(MAX_SEGMENT_AGE_MS + 24 * 60 * 60 * 1000);
                }
                _ => soak.cycle(d).await,
            }
        }
        if !joined {
            for dev in &soak.devs {
                dev.view.clear();
            }
            let dev = join_fresh(&soak.w, "D", &soak.rk).await.unwrap_or_else(|e| panic!("seed {seed}: join: {e}"));
            soak.devs.push(dev);
        }
        soak
    }

    async fn settle(&mut self) {
        for dev in &self.devs {
            dev.view.clear();
        }
        let mut devs: Vec<&mut Dev> = self.devs.iter_mut().collect();
        let reports = quiesce(&self.w, &mut devs, 40).await;
        for report in reports {
            if matches!(report.status, CycleStatus::Stopped { .. }) {
                self.bad.push(format!("settle: {report:?}"));
            }
        }
    }

    fn check(&self) {
        let seed = self.seed;
        assert!(self.bad.is_empty(), "seed {seed}: stopped or local failures: {:?}", self.bad);
        assert_converged(&self.devs.iter().collect::<Vec<_>>());
        for dev in &self.devs {
            let store = lock(&dev.store);
            let (set, _) = store.load_set(&dev.vault).unwrap();
            drop(store);
            let held: BTreeSet<String> = set.iter().map(|e| signature(&e.body)).collect();
            let lost: Vec<&String> = self.signatures.difference(&held).collect();
            assert!(lost.is_empty(), "seed {seed}: {} lost ops {lost:?}", dev.label);
            let projection = dev.projection();
            for key in self.created.difference(&self.deleted) {
                let state = projection.entities.get(key);
                assert!(
                    state.is_some_and(|s| *s != EntityState::Deleted),
                    "seed {seed}: {} lost {key:?}: {state:?}",
                    dev.label
                );
            }
            for (conv, msgs) in &self.messages {
                if let Some(EntityState::Conv(c)) = projection.entities.get(&(Kind::Conv, conv.clone())) {
                    for msg in msgs {
                        assert!(c.messages.contains_key(msg), "seed {seed}: {} lost message {msg}", dev.label);
                    }
                }
            }
            let ids: Vec<(String, String)> = self.devs.iter().map(|d| (d.label.clone(), d.device())).collect();
            let held = dev.held_cursors();
            let detail: Vec<(String, Option<crate::sync::store::Cursor>, Vec<String>)> = held.iter().map(|h| (h.clone(), lock(&dev.store).cursor(&dev.vault, h).unwrap(), self.w.keys(&format!("v1/{}/log/{h}/", dev.vault)))).collect();
            assert!(held.is_empty(), "seed {seed}: {} held {detail:?}; quarantine {:?}; devices {ids:?}", dev.label, dev.quarantine());
        }
    }
}

#[tokio::test]
async fn s7_seeded_random_soak_converges_after_quiescence() {
    let seeds: Vec<u64> = match std::env::var("SAYKNOW_SIM_SEED") {
        Ok(text) => vec![text.trim().parse().expect("SAYKNOW_SIM_SEED is a u64")],
        Err(_) => SEEDS.to_vec(),
    };
    for seed in seeds {
        let _guard = SeedGuard(seed);
        let started = Instant::now();
        let mut soak = Soak::run(seed).await;
        soak.settle().await;
        soak.check();
        println!(
            "sim3 soak seed={seed} drive={} devices={} ops={} ms={}",
            !soak.w.fake.create_only(),
            soak.devs.len(),
            soak.signatures.len(),
            started.elapsed().as_millis()
        );
    }
}

// ===========================================================================
// Performance probes (print only)
// ===========================================================================

fn perf_store(device: &str) -> Store {
    let identity =
        DeviceIdentity { v: 1, vault_id: VAULT.to_owned(), device_id: device.to_owned(), install_fp: None, generation: 0 };
    let mut store = Store::open_in_memory().unwrap();
    store.init_identity(&identity).unwrap();
    store.set_active_vault(VAULT).unwrap();
    store
}

fn clip_memo(id: &str, text: &str) -> ClipEntry {
    ClipEntry {
        id: id.to_owned(),
        text: text.to_owned(),
        preview: make_preview(text),
        ts: 1,
        pinned: false,
        note: None,
        kind: EntryKind::Memo,
        conflict_of: None,
    }
}

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

/// A15: one memo command (the `StoreMemoSink` path: full projection, then
/// one `insert_local_batch` that re-projects) against 1k and 10k ops.
#[test]
fn perf_a15_memo_command_latency_at_1k_and_10k_ops() {
    for n in [1_000usize, 10_000] {
        let w = World::new(true);
        let mut store = perf_store("dev-perf");
        let mut clock = HlcClock::default();
        let memos = n / 4;
        let mut heads: Vec<String> = Vec::with_capacity(memos);
        let mut batch = Vec::new();
        let body = |clock: &mut HlcClock, entity: String, op_type: &str, payload: Value| -> OpBody {
            let next = w.next_op.fetch_add(1, Ordering::SeqCst) + 1;
            let mut rand = [0u8; 10];
            rand[2..].copy_from_slice(&next.to_be_bytes());
            OpBody {
                v: 1,
                op_id: uuid::Builder::from_unix_timestamp_millis(w.now(), &rand).into_uuid().to_string(),
                origin_device: "unset".to_owned(),
                hlc: clock.tick(w.now()),
                ctx: BTreeMap::new(),
                obs: Vec::new(),
                kind: Kind::Memo,
                entity_id: entity,
                op_type: op_type.to_owned(),
                payload,
            }
        };
        let build = Instant::now();
        for i in 0..n {
            let op = if i < memos {
                body(&mut clock, format!("memo-{i}"), "create", json!({"text": format!("memo {i}"), "pinned": false}))
            } else {
                let m = i % memos;
                body(&mut clock, format!("memo-{m}"), "edit", json!({"text": format!("edit {i}"), "base_op_id": heads[m]}))
            };
            if i < memos {
                heads.push(op.op_id.clone());
            } else {
                heads[i % memos] = op.op_id.clone();
            }
            batch.push(op);
            if batch.len() == 1_000 {
                store.insert_local_batch(VAULT, std::mem::take(&mut batch), Vec::new(), Vec::new()).unwrap();
            }
        }
        if !batch.is_empty() {
            store.insert_local_batch(VAULT, batch, Vec::new(), Vec::new()).unwrap();
        }
        let built = build.elapsed();
        let sink = StoreMemoSink {
            store: Arc::new(Mutex::new(store)),
            vault: VAULT.to_owned(),
            clock: Arc::new(Mutex::new(clock)),
        };
        let mut edits = Vec::new();
        for round in 0..5 {
            let before = clip_memo("memo-0", "before");
            let after = clip_memo("memo-0", &format!("after {round}"));
            let started = Instant::now();
            sink.changed(Some(&before), Some(&after)).unwrap();
            edits.push(millis(started.elapsed()));
        }
        let started = Instant::now();
        sink.changed(None, Some(&clip_memo("memo-new", "brand new"))).unwrap();
        let create = millis(started.elapsed());
        let started = Instant::now();
        sink.delete_all(&["memo-1".to_owned()]).unwrap();
        let delete = millis(started.elapsed());
        edits.sort_by(f64::total_cmp);
        let ops = lock(&sink.store).load_set(VAULT).unwrap().0.len();
        assert!(ops > n);
        println!(
            "G10-PERF a15_memo_command ops={n} build_ms={:.1} edit_ms_median={:.2} edit_ms_max={:.2} create_ms={create:.2} delete_ms={delete:.2}",
            millis(built),
            edits[edits.len() / 2],
            edits[edits.len() - 1]
        );
    }
}

/// A web store of `convs` × `per_conv` messages plus some translations.
fn web_store(convs: usize, per_conv: usize) -> WebStore {
    WebStore {
        conversations: (0..convs)
            .map(|c| WsConv {
                id: format!("c{c}"),
                title: format!("conversation {c}"),
                messages: (0..per_conv)
                    .map(|m| WsMsg {
                        id: format!("c{c}-m{m}"),
                        role: if m % 2 == 0 { "user" } else { "assistant" }.to_owned(),
                        content: format!("message {m} of conversation {c}: {}", "lorem ipsum ".repeat(8)),
                        ts: (c * per_conv + m) as u64,
                        ..Default::default()
                    })
                    .collect(),
            })
            .collect(),
        history: (0..200)
            .map(|t| WsTr {
                id: format!("t{t}"),
                ts: t,
                source: format!("source {t}"),
                target: format!("target {t}"),
                from: "ko".to_owned(),
                to: "en".to_owned(),
                model: "m".to_owned(),
                pinned: t % 7 == 0,
            })
            .collect(),
    }
}

/// A12: boot import of a 10k-message web store (backup push, M4 import, M5
/// verify), then `sync_chat_load` over the same 10k messages (the command's
/// pipeline: load_set, project, source index, chat store).
#[test]
fn perf_a12_migration_import_and_chat_load_with_10k_messages() {
    let dir = TempDir::new("sim3-perf-a12");
    let clipboard_src = dir.0.join("clipboard.json");
    save_to(&clipboard_src, &[clip_memo("memo-a", "a memo")], 500, false).unwrap();
    let mdir = MigrationDir::new(dir.0.join("sync-migration"), clipboard_src);
    let mut store = Store::open(&dir.0.join("sync").join("sync.db")).unwrap();
    let identity =
        DeviceIdentity { v: 1, vault_id: VAULT.to_owned(), device_id: "dev-perf".to_owned(), install_fp: None, generation: 0 };
    store.init_identity(&identity).unwrap();
    store.set_active_vault(VAULT).unwrap();
    let ws = web_store(100, 100);
    let bytes = serde_json::to_vec(&ws).unwrap();
    let chunk = bytes.len().div_ceil(4);
    let chunks: Vec<&[u8]> = bytes.chunks(chunk).collect();

    let started = Instant::now();
    for (i, part) in chunks.iter().enumerate() {
        migration::push_webstore_chunk(&mut store, &mdir, VAULT, i as u32, chunks.len() as u32, part, "snap-perf").unwrap();
    }
    let push = started.elapsed();
    let name_key = derive_subkey(&SecretKey::from_bytes([3; 32]), VAULT, EPOCH, Purpose::BlobName);
    let mut clock = HlcClock::default();
    let started = Instant::now();
    let report = migration::import(&mut store, &mdir, VAULT, &name_key, &mut clock, now_ms()).unwrap();
    let import = started.elapsed();
    assert_eq!(report.messages, 10_000);
    let started = Instant::now();
    let verified = migration::verify(&mut store, &mdir, VAULT).unwrap();
    let verify = started.elapsed();
    assert!(matches!(verified, migration::Verify::Passed(_)), "{verified:?}");

    let mut loads = Vec::new();
    let mut messages = 0;
    for _ in 0..3 {
        let started = Instant::now();
        let (set, forks) = store.load_set(VAULT).unwrap();
        let projection = project(&set, &forks).unwrap();
        let index = migration::SrcIndex::of(&set);
        let out = migration::chat_store(&projection, &index, |id| Ok(store.blob_bytes(id)?.map(|(_, b)| b))).unwrap();
        loads.push(millis(started.elapsed()));
        messages = out.conversations.iter().map(|c| c.messages.len()).sum::<usize>();
    }
    assert_eq!(messages, 10_000);
    loads.sort_by(f64::total_cmp);
    println!(
        "G10-PERF a12_migration messages=10000 bytes={} push_ms={:.1} import_ms={:.1} verify_ms={:.1}",
        bytes.len(),
        millis(push),
        millis(import),
        millis(verify)
    );
    println!("G10-PERF sync_chat_load messages=10000 ms_median={:.1} ms_max={:.1}", loads[loads.len() / 2], loads[loads.len() - 1]);
}
