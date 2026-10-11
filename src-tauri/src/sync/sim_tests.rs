//! Fake multi-device simulator over real stores, real envelopes, and a fake
//! remote (no network, no keychain): AC 7, 16b, 17, 19, 19b at store level.
//!
//! Contract deviations: none. The remote models both create-only providers
//! (a taken name is `AlreadyExists`, nothing written) and Drive (duplicate
//! names allowed); per-reader list lag is a visibility filter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::{json, Value};

use super::crypto::{derive_subkey, Purpose, SecretKey};
use super::envelope::{seal, ObjectAddress};
use super::identity::DeviceIdentity;
use super::merge::{conflict_copy_id, EntityState, MemoState, Notice};
use super::ops::{body_hash, Hlc, Kind, OpBody};
use super::rotation::UploadState;
use super::store::{open_segment, ApplyOutcome, CursorState, PendingSegment, SealedError, Store};

pub(crate) const VAULT: &str = "vault-1";
pub(crate) const EPOCH: u32 = 1;
pub(crate) const M: &str = "0190a000-0000-7000-8000-0000000000aa";
pub(crate) const K: &str = "0190a000-0000-7000-8000-0000000000bb";

pub(crate) fn seg_key() -> SecretKey {
    derive_subkey(&SecretKey::from_bytes([7; 32]), VAULT, EPOCH, Purpose::Segment)
}

pub(crate) fn oid(n: u64) -> String {
    let mut rand = [0u8; 10];
    rand[2..].copy_from_slice(&n.to_be_bytes());
    uuid::Builder::from_unix_timestamp_millis(1_700_000_000_000, &rand).into_uuid().to_string()
}

pub(crate) fn identity(device: &str) -> DeviceIdentity {
    DeviceIdentity { v: 1, vault_id: VAULT.to_owned(), device_id: device.to_owned(), install_fp: None, generation: 0 }
}

pub(crate) fn device(name: &str) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store.init_identity(&identity(name)).unwrap();
    store.set_active_vault(VAULT).unwrap();
    store
}

pub(crate) fn op(n: u64, wall: u64, kind: Kind, entity: &str, op_type: &str, payload: Value) -> OpBody {
    OpBody {
        v: 1,
        op_id: oid(n),
        origin_device: "unset".to_owned(),
        hlc: Hlc { wall_ms: wall, ctr: 0 },
        ctx: BTreeMap::new(),
        obs: Vec::new(),
        kind,
        entity_id: entity.to_owned(),
        op_type: op_type.to_owned(),
        payload,
    }
}

pub(crate) fn create(n: u64, wall: u64, memo: &str, text: &str) -> OpBody {
    op(n, wall, Kind::Memo, memo, "create", json!({"text": text, "pinned": false}))
}

pub(crate) fn edit(n: u64, wall: u64, memo: &str, base: u64, text: &str) -> OpBody {
    op(n, wall, Kind::Memo, memo, "edit", json!({"text": text, "base_op_id": oid(base)}))
}

pub(crate) fn delete(n: u64, wall: u64, memo: &str) -> OpBody {
    op(n, wall, Kind::Memo, memo, "delete", json!({}))
}

pub(crate) fn local(store: &mut Store, body: OpBody) {
    store.insert_local_op(VAULT, body, UploadState::Never).unwrap();
}

/// Seals the store's unsealed own ops under the segment address.
pub(crate) fn seal_next(store: &mut Store) -> Option<PendingSegment> {
    let key = seg_key();
    store
        .seal_pending(VAULT, |segment, plain| {
            let address =
                ObjectAddress::segment(VAULT, EPOCH, &segment.device_id, segment.first_seq, segment.last_seq);
            seal(&key, &address, plain)
        })
        .unwrap()
}

pub(crate) fn remote_key(device: &str, first: u64, last: u64) -> String {
    ObjectAddress::segment(VAULT, EPOCH, device, first, last).remote_key().unwrap()
}

/// Fake provider. `create_only`: a taken name is `AlreadyExists` and nothing
/// is written; otherwise (Drive) a second object under the name is kept.
pub(crate) struct Remote {
    pub(crate) create_only: bool,
    pub(crate) objects: BTreeMap<String, Vec<Vec<u8>>>,
}

impl Remote {
    pub(crate) fn new(create_only: bool) -> Self {
        Remote { create_only, objects: BTreeMap::new() }
    }

    fn put(&mut self, key: String, bytes: Vec<u8>) -> bool {
        let slot = self.objects.entry(key).or_default();
        if self.create_only && !slot.is_empty() {
            return slot[0] == bytes;
        }
        slot.push(bytes);
        true
    }

    /// Every op id in every decryptable segment.
    pub(crate) fn op_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for (key, copies) in &self.objects {
            let address = ObjectAddress::from_remote_key(key, EPOCH).unwrap();
            for bytes in copies {
                if let Ok(segment) = open_segment(&seg_key(), &address, bytes) {
                    ids.extend(segment.ops.into_iter().map(|op| op.body.op_id));
                }
            }
        }
        ids
    }

    /// Verified body hashes per seq under `log/<device>/` (R2).
    pub(crate) fn hashes_by_seq(&self, device: &str) -> BTreeMap<u64, Vec<[u8; 32]>> {
        let mut out: BTreeMap<u64, Vec<[u8; 32]>> = BTreeMap::new();
        for (key, copies) in self.stream(device, &|_, _| true) {
            let address = ObjectAddress::from_remote_key(&key, EPOCH).unwrap();
            for bytes in copies {
                let segment = open_segment(&seg_key(), &address, &bytes).unwrap();
                for op in &segment.ops {
                    out.entry(op.seq).or_default().push(body_hash(&op.body).unwrap());
                }
            }
        }
        out
    }

    fn stream(&self, device: &str, visible: &dyn Fn(&str, usize) -> bool) -> Vec<(String, Vec<Vec<u8>>)> {
        let prefix = format!("v1/{VAULT}/log/{device}/");
        self.objects
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, copies)| {
                let seen = copies.iter().enumerate().filter(|(i, _)| visible(key, *i)).map(|(_, b)| b.clone());
                (key.clone(), seen.collect::<Vec<_>>())
            })
            .filter(|(_, copies)| !copies.is_empty())
            .collect()
    }
}

/// Seal → `attempted` → put → `published` / `rejected` (§4.2.3).
pub(crate) fn publish(store: &mut Store, remote: &mut Remote) -> Option<(PendingSegment, bool)> {
    let pending = seal_next(store)?;
    let (device, first, last) = (pending.device_id.clone(), pending.first_seq, pending.last_seq);
    store.mark_attempted(VAULT, &device, first).unwrap();
    let created = remote.put(remote_key(&device, first, last), pending.bytes.clone());
    if created {
        store.mark_published(VAULT, &device, first).unwrap();
    }
    Some((pending, created))
}

/// Reads one stream in key order; a name holding two readable objects goes
/// through `apply_fork_variant`.
pub(crate) fn sync_stream(
    store: &mut Store,
    remote: &Remote,
    device: &str,
    visible: &dyn Fn(&str, usize) -> bool,
) -> Vec<ApplyOutcome> {
    let key = seg_key();
    let mut outcomes = Vec::new();
    for (name, copies) in remote.stream(device, visible) {
        let address = ObjectAddress::from_remote_key(&name, EPOCH).unwrap();
        if let [one] = copies.as_slice() {
            outcomes.push(store.apply_sealed(VAULT, &key, &address, one).unwrap());
            continue;
        }
        let mut variants = Vec::new();
        for bytes in &copies {
            match open_segment(&key, &address, bytes) {
                Ok(segment) => variants.push(segment),
                Err(SealedError::Corrupt(_) | SealedError::FutureVersion(_)) => {
                    outcomes.push(store.apply_sealed(VAULT, &key, &address, bytes).unwrap());
                }
            }
        }
        let (first, last) = (variants[0].first_seq, variants[0].last_seq);
        outcomes.push(store.apply_fork_variant(VAULT, device, first, last, &variants).unwrap());
    }
    outcomes
}

pub(crate) fn all(_: &str, _: usize) -> bool {
    true
}

pub(crate) fn hash(store: &Store) -> [u8; 32] {
    store.projection(VAULT).unwrap().hash
}

pub(crate) fn memo(store: &Store, id: &str) -> Option<EntityState> {
    store.projection(VAULT).unwrap().entities.get(&(Kind::Memo, id.to_owned())).cloned()
}

fn memo_state(store: &Store, id: &str) -> MemoState {
    match memo(store, id) {
        Some(EntityState::Memo(m)) => m,
        other => panic!("memo {id} is {other:?}"),
    }
}

fn restored(store: &Store, id: &str) -> bool {
    let notice = Notice::RestoredAfterDeleteConflict { kind: Kind::Memo, entity_id: id.to_owned() };
    store.projection(VAULT).unwrap().notices.contains(&notice)
}

/// I1: every op with a dot on one of `own` has exactly one dot.
pub(crate) fn assert_one_dot_each(store: &Store, own: &[&str]) {
    let conn = store.conn();
    let mut stmt = conn.prepare("SELECT op_id FROM op_dots WHERE vault_id = ?1 AND device_id = ?2").unwrap();
    for device in own {
        let ids: Vec<String> =
            stmt.query_map([VAULT, device], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        for id in ids {
            assert_eq!(store.dots_of(VAULT, &id).unwrap().len(), 1, "op {id} on {device} has one dot");
        }
    }
}

/// A test-scoped directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sayknow-sync-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A VM / backup clone: the same database bytes under the same device id.
fn clone_store(store: &Store, dir: &TempDir) -> Store {
    let path = dir.0.join("clone.db");
    store.conn().execute("VACUUM INTO ?1", [path.to_string_lossy().into_owned()]).unwrap();
    Store::open(&path).unwrap()
}

/// AC 16b (N-1): A and its clone A′ share seq 1. A′ writes x (rejected by
/// the provider) and z = delete M; A publishes y = edit M at (A, 2). After A′
/// rotates, all three devices keep M with a notice, A′ applies A's stream
/// without quarantine or rollback, and only U/X ops moved.
#[test]
fn ac16b_clone_delete_does_not_remove_the_originals_unseen_edit() {
    let mut remote = Remote::new(true);
    let mut a = device("a");
    let mut b = device("b");
    local(&mut a, create(1, 1_000, M, "base"));
    assert!(publish(&mut a, &mut remote).unwrap().1);
    let p1 = oid(1);

    let dir = TempDir::new("ac16b");
    let mut a2 = clone_store(&a, &dir);

    // A: y = edit M at (a, 2), published.
    local(&mut a, edit(3, 3_000, M, 1, "y"));
    assert!(publish(&mut a, &mut remote).unwrap().1);
    sync_stream(&mut b, &remote, "a", &all);

    // A′: x at (a, 2) is attempted and the provider confirms the name is taken.
    local(&mut a2, create(2, 2_000, K, "x"));
    let (x_seg, created) = publish(&mut a2, &mut remote).unwrap();
    assert!(!created, "create-only provider keeps A's object");
    assert_eq!(a2.mark_rejected(VAULT, "a", x_seg.first_seq).unwrap(), 1);
    // z = delete M at (a, 3), never sent.
    local(&mut a2, delete(4, 4_000, M));
    let (z_old_content, z_old_body, _) = a2.op_state(VAULT, &oid(4)).unwrap().unwrap();
    let (p1_content, p1_body, _) = a2.op_state(VAULT, &p1).unwrap().unwrap();
    let before_states: BTreeMap<String, UploadState> = [1, 2, 4]
        .into_iter()
        .map(|n| (oid(n), a2.op_state(VAULT, &oid(n)).unwrap().unwrap().2))
        .collect();

    // R2–R7 on A′ (keychain already names a2).
    let remote_hashes = remote.hashes_by_seq("a");
    let source = a2.rotation_input(VAULT, remote_hashes.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!(plan.verified_prefix, 1);
    let moved: Vec<&str> = plan.rewritten.iter().map(|r| r.op.op_id.as_str()).collect();
    assert_eq!(moved, [oid(99).as_str(), oid(4).as_str()]);
    let restamped_published = plan
        .rewritten
        .iter()
        .filter(|r| {
            let source_id = plan.reissued.iter().find(|(_, n)| **n == r.op.op_id).map_or(&r.op.op_id, |(o, _)| o);
            matches!(before_states[source_id], UploadState::Attempted | UploadState::Published)
        })
        .count();
    assert_eq!(restamped_published, 0, "attempted/published ops are never restamped");
    a2.apply_rotation(VAULT, &plan, &remote_hashes, &identity("a2")).unwrap();

    // Restamp: body_hash recomputed, content_hash unchanged; P untouched.
    let (z_content, z_body, z_state) = a2.op_state(VAULT, &oid(4)).unwrap().unwrap();
    assert_eq!(z_content, z_old_content);
    assert_ne!(z_body, z_old_body);
    let (set, _) = a2.load_set(VAULT).unwrap();
    assert_eq!(z_body, body_hash(&set.get(&oid(4)).unwrap().body).unwrap());
    assert_eq!(set.get(&oid(4)).unwrap().body.ctx, BTreeMap::from([("a".to_owned(), 1), ("a2".to_owned(), 1)]));
    assert_eq!(z_state, UploadState::Never);
    assert_eq!(a2.op_state(VAULT, &p1).unwrap().unwrap(), (p1_content, p1_body, UploadState::Published));
    assert_eq!(a2.dots_of(VAULT, &p1).unwrap(), vec![super::ops::Dot { device: "a".to_owned(), seq: 1 }]);
    assert!(a2.dots_of(VAULT, &oid(2)).unwrap().is_empty(), "the rejected original has no dot");
    assert_eq!(a2.quarantine_rows(VAULT).unwrap().len(), 1);

    // A′ reads its retired id like another device: y applies, no quarantine.
    let outcomes = sync_stream(&mut a2, &remote, "a", &all);
    assert!(outcomes.iter().all(|o| matches!(o, ApplyOutcome::Applied { conflicts: 0, .. })), "{outcomes:?}");
    assert_eq!(a2.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Retired);
    assert!(publish(&mut a2, &mut remote).unwrap().1);
    sync_stream(&mut a, &remote, "a2", &all);
    sync_stream(&mut b, &remote, "a2", &all);

    for store in [&a, &b, &a2] {
        assert_eq!(hash(store), hash(&a), "all three converge");
        assert_eq!(memo_state(store, M).text, "y", "M kept with the original's edit");
        assert!(restored(store, M), "kept with a notice");
        assert_eq!(memo_state(store, K).text, "x", "the reissued x arrived");
        assert!(store.forks(VAULT).unwrap().is_empty());
    }
    assert_one_dot_each(&a2, &["a", "a2"]);
    assert_one_dot_each(&a, &["a"]);
    assert!(!remote.op_ids().contains(&oid(2)), "the rejected original was never uploaded");
    assert!(remote.op_ids().contains(&oid(99)));
    assert_eq!(a2.quarantine_rows(VAULT).unwrap(), vec![(
        format!("op/{}", oid(2)),
        format!("rejected at a/2; reissued as {}", oid(99))
    )]);
}

/// AC 7: a corrupt envelope, an AAD mismatch, and a plaintext/address
/// cross-check mismatch leave state and cursor bytes unchanged and write a
/// quarantine row; the genuine segment still applies afterwards.
#[test]
fn ac7_corrupt_or_mismatched_segments_change_nothing_but_quarantine() {
    let mut remote = Remote::new(true);
    let mut a = device("a");
    let mut b = device("b");
    local(&mut a, create(1, 1_000, M, "one"));
    publish(&mut a, &mut remote).unwrap();
    local(&mut a, edit(2, 2_000, M, 1, "two"));
    let (seg2, _) = publish(&mut a, &mut remote).unwrap();
    sync_stream(&mut b, &remote, "a", &|key, _| key.ends_with("000000000001-000000000001.seg"));

    let key = seg_key();
    let state = b.dump_state().unwrap();
    let cursor = b.cursor(VAULT, "a").unwrap();
    let mut flipped = seg2.bytes.clone();
    let mid = flipped.len() / 2;
    flipped[mid] ^= 1;
    let right = ObjectAddress::segment(VAULT, EPOCH, "a", 2, 2);
    // Forged by a DEK holder: authenticates under device c's address, but the
    // plaintext claims device a.
    let plain = open_segment(&key, &right, &seg2.bytes).unwrap().encode().unwrap();
    let as_c = ObjectAddress::segment(VAULT, EPOCH, "c", 2, 2);
    let forged = seal(&key, &as_c, &plain).unwrap();
    let cases: Vec<(&str, ObjectAddress, Vec<u8>)> = vec![
        ("corrupt", right.clone(), flipped),
        ("aad", ObjectAddress::segment(VAULT, EPOCH, "a", 3, 3), seg2.bytes.clone()),
        ("cross-check", as_c, forged),
    ];
    for (name, address, bytes) in &cases {
        let outcome = b.apply_sealed(VAULT, &key, address, bytes).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Quarantined { .. }), "{name}: {outcome:?}");
        assert_eq!(b.dump_state().unwrap(), state, "{name}: state bytes unchanged");
        assert_eq!(b.cursor(VAULT, "a").unwrap(), cursor, "{name}: cursor unchanged");
        assert_eq!(b.cursor(VAULT, "c").unwrap(), None);
        let qkey = address.remote_key().unwrap();
        assert!(b.quarantine_rows(VAULT).unwrap().iter().any(|(k, _)| *k == qkey), "{name}: quarantined");
    }
    // A decrypted segment whose plaintext range disagrees with its address.
    let segment = open_segment(&key, &right, &seg2.bytes).unwrap();
    let outcome = b.apply_segment(VAULT, "a", 2, 3, &segment).unwrap();
    assert!(matches!(outcome, ApplyOutcome::Quarantined { .. }));
    assert_eq!(b.dump_state().unwrap(), state);
    assert_eq!(b.quarantine_rows(VAULT).unwrap().len(), 4);

    let outcome = b.apply_sealed(VAULT, &key, &right, &seg2.bytes).unwrap();
    assert!(matches!(outcome, ApplyOutcome::Applied { .. }), "{outcome:?}");
    assert_eq!(b.cursor(VAULT, "a").unwrap().unwrap().applied_seq, 2);
    assert_eq!(hash(&b), hash(&a));
}

/// AC 19: a missing segment holds that stream only; a wrong prev hash holds
/// and quarantines. Other devices keep applying.
#[test]
fn ac19_simple_break_holds_one_stream_while_others_continue() {
    let mut remote = Remote::new(true);
    let (mut a, mut b, mut c) = (device("a"), device("b"), device("c"));
    for n in 1..=3 {
        local(&mut a, create(n, n * 1_000, &oid(100 + n), &format!("a{n}")));
        publish(&mut a, &mut remote).unwrap();
    }
    local(&mut b, create(10, 10_000, M, "from b"));
    publish(&mut b, &mut remote).unwrap();

    let lagging = |key: &str, _: usize| !key.ends_with("000000000002-000000000002.seg");
    let outcomes = sync_stream(&mut c, &remote, "a", &lagging);
    assert!(matches!(outcomes[1], ApplyOutcome::Held { .. }), "{outcomes:?}");
    assert!(c.quarantine_rows(VAULT).unwrap().is_empty(), "a gap blames no object");
    sync_stream(&mut c, &remote, "b", &all);
    let cursor = c.cursor(VAULT, "a").unwrap().unwrap();
    assert_eq!((cursor.state, cursor.applied_seq), (CursorState::Held, 1));
    // No automatic recovery in v1: the late segment is not applied either.
    let outcomes = sync_stream(&mut c, &remote, "a", &all);
    assert!(outcomes.iter().all(|o| *o == ApplyOutcome::Skipped(CursorState::Held) || *o == ApplyOutcome::Duplicate));
    assert!(memo(&c, &oid(102)).is_none() && memo(&c, &oid(103)).is_none(), "unapplied ops are not projected");
    assert_eq!(memo_state(&c, &oid(101)).text, "a1");

    // Other devices continue.
    local(&mut b, edit(11, 11_000, M, 10, "b again"));
    publish(&mut b, &mut remote).unwrap();
    sync_stream(&mut c, &remote, "b", &all);
    assert_eq!(memo_state(&c, M).text, "b again");
    assert_eq!(c.cursor(VAULT, "b").unwrap().unwrap().applied_seq, 2);

    // A hash mismatch (wrong prev_seg_hash, sealed correctly) holds and quarantines.
    let mut d = device("d");
    sync_stream(&mut d, &remote, "a", &|key, _| key.ends_with("000000000001-000000000001.seg"));
    let right = ObjectAddress::segment(VAULT, EPOCH, "a", 2, 2);
    let key = seg_key();
    let mut segment = open_segment(&key, &right, &remote.objects[&remote_key("a", 2, 2)][0]).unwrap();
    segment.prev_seg_hash = "0".repeat(64);
    let forged = seal(&key, &right, &segment.encode().unwrap()).unwrap();
    let outcome = d.apply_sealed(VAULT, &key, &right, &forged).unwrap();
    assert!(matches!(outcome, ApplyOutcome::Held { .. }), "{outcome:?}");
    assert_eq!(d.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held);
    assert_eq!(d.quarantine_rows(VAULT).unwrap().len(), 1);
    assert!(memo(&d, &oid(102)).is_none());
}

/// The AC 19b world after the fork is known: readers b, c, d of a Drive
/// stream `a` holding two variants of `1-1` (a and its VM clone).
pub(crate) struct ForkWorld {
    pub(crate) remote: Remote,
    pub(crate) b: Store,
    pub(crate) c: Store,
    pub(crate) d: Store,
    pub(crate) copy_id: String,
}

pub(crate) fn fork_world() -> ForkWorld {
    let mut remote = Remote::new(false);
    let mut b = device("b");
    local(&mut b, create(1, 100, M, "base"));
    publish(&mut b, &mut remote).unwrap();
    let mut a = device("a");
    sync_stream(&mut a, &remote, "b", &all);
    let dir = TempDir::new("ac19b");
    let mut a_clone = clone_store(&a, &dir);
    local(&mut a, edit(2, 200, M, 1, "variant one"));
    local(&mut a_clone, edit(3, 210, M, 1, "variant two"));
    assert!(publish(&mut a, &mut remote).unwrap().1);
    assert!(publish(&mut a_clone, &mut remote).unwrap().1, "Drive keeps a second object under the name");
    assert_eq!(remote.objects[&remote_key("a", 1, 1)].len(), 2);

    // b's listing shows only the first variant; it deletes M "having seen" (a,1).
    sync_stream(&mut b, &remote, "a", &|_, i| i == 0);
    local(&mut b, delete(4, 300, M));
    publish(&mut b, &mut remote).unwrap();
    let blind = b.load_set(VAULT).unwrap().0.get(&oid(4)).unwrap().body.clone();
    assert!(blind.obs.is_empty() && blind.ctx["a"] == 1);

    // c sees both variants first; d reads b's stream first.
    let mut c = device("c");
    sync_stream(&mut c, &remote, "a", &all);
    sync_stream(&mut c, &remote, "b", &all);
    let mut d = device("d");
    sync_stream(&mut d, &remote, "b", &all);
    sync_stream(&mut d, &remote, "a", &all);
    // b's listing catches up.
    let outcomes = sync_stream(&mut b, &remote, "a", &all);
    assert!(matches!(outcomes[..], [ApplyOutcome::Applied { fork_started: true, .. }]), "{outcomes:?}");
    ForkWorld { remote, b, c, d, copy_id: conflict_copy_id(M, &oid(2)) }
}

/// AC 19b (N-2): readers see the two variants in different orders; after
/// fork detection all hashes converge, no edit is lost, the blind delete
/// counts as not having seen fork-range ops, and a re-delete carrying obs
/// converges to deleted.
#[test]
fn ac19b_fork_variants_converge_and_a_redelete_with_obs_wins() {
    let ForkWorld { mut remote, mut b, mut c, mut d, copy_id } = fork_world();
    for store in [&b, &c, &d] {
        assert_eq!(hash(store), hash(&b), "readers converge");
        assert_eq!(store.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]));
        assert_eq!(store.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);
        assert!(restored(store, M), "a delete without obs did not see the fork range");
        assert_eq!(memo_state(store, M).text, "variant two");
        assert_eq!(memo_state(store, &copy_id).text, "variant one", "no edit lost");
        assert!(store.quarantine_rows(VAULT).unwrap().is_empty());
    }

    // b re-deletes knowing the fork: obs is filled from the fork range.
    local(&mut b, delete(5, 400, M));
    local(&mut b, delete(6, 410, &copy_id));
    let (set, _) = b.load_set(VAULT).unwrap();
    assert_eq!(set.get(&oid(5)).unwrap().body.obs, vec![oid(2), oid(3)]);
    assert_eq!(set.get(&oid(6)).unwrap().body.obs, vec![oid(2)]);
    publish(&mut b, &mut remote).unwrap();
    sync_stream(&mut c, &remote, "b", &all);
    sync_stream(&mut d, &remote, "b", &all);
    for store in [&b, &c, &d] {
        assert_eq!(hash(store), hash(&b));
        assert_eq!(memo(store, M), Some(EntityState::Deleted));
        assert_eq!(memo(store, &copy_id), Some(EntityState::Deleted));
        assert!(!restored(store, M));
    }
}

/// AC 17: a crash inside the R5–R7 transaction applies nothing; the next
/// cycle rotates again (to yet another id) and converges to the same state as
/// a device that never crashed. A stale plan also rolls back completely.
#[test]
fn ac17_crash_during_rotation_applies_nothing_and_rerotation_converges() {
    fn world() -> (Store, BTreeMap<u64, Vec<[u8; 32]>>) {
        let mut a = device("a");
        local(&mut a, create(1, 1_000, M, "p1"));
        let seg = seal_next(&mut a).unwrap();
        a.mark_attempted(VAULT, "a", seg.first_seq).unwrap();
        a.mark_published(VAULT, "a", seg.first_seq).unwrap();
        local(&mut a, create(2, 2_000, K, "x"));
        let seg = seal_next(&mut a).unwrap();
        a.mark_attempted(VAULT, "a", seg.first_seq).unwrap();
        a.mark_rejected(VAULT, "a", seg.first_seq).unwrap();
        local(&mut a, edit(3, 3_000, K, 2, "u"));
        let p1 = a.op_state(VAULT, &oid(1)).unwrap().unwrap().1;
        (a, BTreeMap::from([(1, vec![p1]), (2, vec![[0x42; 32]])]))
    }
    let plan_for = |store: &Store, remote: &BTreeMap<u64, Vec<[u8; 32]>>, new: &str| {
        let source = store.rotation_input(VAULT, remote.clone()).unwrap();
        Store::plan_rotation(&source, new, || oid(99)).unwrap()
    };

    let (mut a, remote) = world();
    let before = a.dump_state().unwrap();
    let plan = plan_for(&a, &remote, "a2");
    a.apply_rotation_then_crash(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert_eq!(a.dump_state().unwrap(), before, "nothing of the crashed rotation is applied");
    assert!(a.quarantine_rows(VAULT).unwrap().is_empty());
    assert_eq!(a.device_id(VAULT).unwrap(), "a");

    let mut stale = plan_for(&a, &remote, "a3");
    stale.rewritten[1].op.body_hash = [0; 32];
    assert!(a.apply_rotation(VAULT, &stale, &remote, &identity("a3")).is_err());
    assert_eq!(a.dump_state().unwrap(), before, "a failed rotation rolls back");

    // Next cycle: T1 (keychain a3 vs SQLite a) → rotate again.
    let plan = plan_for(&a, &remote, "a3");
    a.apply_rotation(VAULT, &plan, &remote, &identity("a3")).unwrap();
    let (mut clean, remote2) = world();
    let plan2 = plan_for(&clean, &remote2, "a3");
    clean.apply_rotation(VAULT, &plan2, &remote2, &identity("a3")).unwrap();
    assert_eq!(a.dump_state().unwrap(), clean.dump_state().unwrap(), "converges to the uncrashed state");
    assert_eq!(a.quarantine_rows(VAULT).unwrap(), clean.quarantine_rows(VAULT).unwrap());
    assert!(!a.dump_state().unwrap().contains("a2"), "the abandoned id left no trace");

    // X reissued; U's reference repointed, which recomputes its content hash.
    assert_eq!(plan.reissued, BTreeMap::from([(oid(2), oid(99))]));
    let (set, _) = a.load_set(VAULT).unwrap();
    let u = &set.get(&oid(3)).unwrap().body;
    assert_eq!(u.payload["base_op_id"], json!(oid(99)));
    let (u_content, _, _) = a.op_state(VAULT, &oid(3)).unwrap().unwrap();
    assert_eq!(u_content, super::ops::content_hash(u).unwrap());
    assert_eq!(memo_state(&a, K).text, "u");
    assert_one_dot_each(&a, &["a", "a3"]);
    let cursor = a.cursor(VAULT, "a").unwrap().unwrap();
    assert_eq!((cursor.state, cursor.applied_seq), (CursorState::Retired, 1));
}

/// AC 16c (I6): a common op u, unsent when A was cloned, is published by the
/// original at (a, 2) and, after the clone rotates (u is U → restamped), by
/// the clone at (a2, 1). Every reader, in either order, and both devices
/// after cross-sync hold 1 logical op with both dots, no projection
/// duplicate, no quarantine, no fork, and one hash.
#[test]
fn ac16c_common_op_published_by_original_and_clone_is_one_logical_op() {
    let mut remote = Remote::new(true);
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "base"));
    assert!(publish(&mut a, &mut remote).unwrap().1);
    local(&mut a, edit(2, 2_000, M, 1, "common"));
    let dir = TempDir::new("ac16c");
    let mut a2 = clone_store(&a, &dir);

    // A′ notices the clone (T2) before A uploads u: u is U and is restamped.
    let remote_hashes = remote.hashes_by_seq("a");
    let source = a2.rotation_input(VAULT, remote_hashes.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!(plan.verified_prefix, 1);
    assert_eq!(plan.rewritten.iter().map(|r| r.op.op_id.clone()).collect::<Vec<_>>(), [oid(2)]);
    assert!(plan.reissued.is_empty());
    a2.apply_rotation(VAULT, &plan, &remote_hashes, &identity("a2")).unwrap();
    let (u_content, _, _) = a.op_state(VAULT, &oid(2)).unwrap().unwrap();
    assert_eq!(a2.op_state(VAULT, &oid(2)).unwrap().unwrap().0, u_content, "content_hash survives the restamp");

    // Both publish u under their own name.
    let (seg_a, created_a) = publish(&mut a, &mut remote).unwrap();
    assert!(created_a);
    assert_eq!((seg_a.device_id.as_str(), seg_a.first_seq), ("a", 2));
    let (seg_a2, created_a2) = publish(&mut a2, &mut remote).unwrap();
    assert!(created_a2);
    assert_eq!((seg_a2.device_id.as_str(), seg_a2.first_seq), ("a2", 1));

    let mut b = device("b");
    let mut c = device("c");
    let mut outcomes = sync_stream(&mut b, &remote, "a", &all);
    outcomes.extend(sync_stream(&mut b, &remote, "a2", &all));
    outcomes.extend(sync_stream(&mut c, &remote, "a2", &all));
    outcomes.extend(sync_stream(&mut c, &remote, "a", &all));
    outcomes.extend(sync_stream(&mut a, &remote, "a2", &all));
    outcomes.extend(sync_stream(&mut a2, &remote, "a", &all));
    assert!(outcomes.iter().all(|o| matches!(o, ApplyOutcome::Applied { conflicts: 0, .. })), "{outcomes:?}");

    let dots = vec![super::ops::Dot { device: "a".to_owned(), seq: 2 }, super::ops::Dot { device: "a2".to_owned(), seq: 1 }];
    for (name, store) in [("a", &a), ("a2", &a2), ("b", &b), ("c", &c)] {
        let (set, _) = store.load_set(VAULT).unwrap();
        assert_eq!(set.iter().count(), 2, "{name}: 2 logical ops (base + u)");
        assert_eq!(set.iter().filter(|e| e.body.op_id == oid(2)).count(), 1, "{name}: u is one logical op");
        assert_eq!(store.dots_of(VAULT, &oid(2)).unwrap(), dots, "{name}: alias dots");
        let projection = store.projection(VAULT).unwrap();
        assert_eq!(projection.entities.len(), 1, "{name}: no duplicate entity");
        assert_eq!(memo_state(store, M).text, "common", "{name}");
        assert!(
            !projection.notices.iter().any(|n| matches!(n, Notice::MemoConflictCopy { .. })),
            "{name}: no conflict copy"
        );
        assert!(store.quarantine_rows(VAULT).unwrap().is_empty(), "{name}: 0 quarantine");
        assert!(store.forks(VAULT).unwrap().is_empty(), "{name}: no fork");
        assert_eq!(hash(store), hash(&b), "{name}: converges");
    }
}

/// Amendment A5 world: own ops (a, 1..=3); 1 rejected (create-only name lost
/// to other content), 2 and 3 published. S = 0, so 2 and 3 are kept past S.
fn a5_world() -> (Store, [[u8; 32]; 3]) {
    let mut a = device("a");
    local(&mut a, create(1, 1_000, K, "lost"));
    let seg = seal_next(&mut a).unwrap();
    a.mark_attempted(VAULT, "a", seg.first_seq).unwrap();
    a.mark_rejected(VAULT, "a", seg.first_seq).unwrap();
    for n in [2, 3] {
        local(&mut a, create(n, n * 1_000, &oid(100 + n), "kept"));
        let seg = seal_next(&mut a).unwrap();
        a.mark_attempted(VAULT, "a", seg.first_seq).unwrap();
        a.mark_published(VAULT, "a", seg.first_seq).unwrap();
    }
    let h = |n: u64| a.op_state(VAULT, &oid(n)).unwrap().unwrap().1;
    let hashes = [h(1), h(2), h(3)];
    (a, hashes)
}

fn rotate_a5(a: &mut Store, remote: &BTreeMap<u64, Vec<[u8; 32]>>) {
    let source = a.rotation_input(VAULT, remote.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!(plan.verified_prefix, 0, "seq 1 holds other content");
    a.apply_rotation(VAULT, &plan, remote, &identity("a2")).unwrap();
}

/// Amendment A5 (critic B3): inside the R5–R7 transaction, a kept published
/// op past S whose remote set also holds other content records a fork and
/// clamps the retired cursor; list lag clamps without a fork; exactly ours
/// records nothing; no kept op but two remote contents is a fork; a crash
/// leaves no forks row.
#[test]
fn a5_rotation_records_forks_and_clamps_the_retired_cursor() {
    let other = [0x42; 32];
    let retired = |a: &Store| {
        let c = a.cursor(VAULT, "a").unwrap().unwrap();
        assert_eq!(c.state, CursorState::Retired);
        c.applied_seq
    };

    // (1) Remote seq 2 = {ours, other}: fork at 2, applied clamped to 1.
    let (mut a, [_, h2, h3]) = a5_world();
    let remote = BTreeMap::from([(1, vec![other]), (2, vec![h2, other]), (3, vec![h3])]);
    let before = a.dump_state().unwrap();
    let source = a.rotation_input(VAULT, remote.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!(plan.retired.applied_seq, 0);
    a.apply_rotation_then_crash(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert_eq!(a.dump_state().unwrap(), before, "a crash leaves no forks row");
    assert!(a.forks(VAULT).unwrap().is_empty());
    rotate_a5(&mut a, &remote);
    assert_eq!(a.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]));
    assert_eq!(retired(&a), 0, "clamped below the fork (and never above the planner's applied_seq)");

    // The clamp itself: the planner would extend applied past S over kept
    // ops; with seq 1 lagging but 2 = {ours, other}, the cursor is held at 1.
    let (mut a, [h1, h2, h3]) = a5_world();
    a.conn().execute("UPDATE ops SET upload_state = 'published' WHERE op_id = ?1", [oid(1)]).unwrap();
    let remote = BTreeMap::from([(1, vec![h1]), (2, vec![h2, other]), (3, vec![h3])]);
    let source = a.rotation_input(VAULT, remote.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!((plan.verified_prefix, plan.retired.applied_seq), (1, 3));
    a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert_eq!(a.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]), "fork at the mixed seq");
    assert_eq!(retired(&a), 1, "applied_seq clamped to seq - 1");

    // (2) List lag: remote misses seq 2 → no fork, applied clamped to 1.
    let (mut a, [h1, _, h3]) = a5_world();
    a.conn().execute("UPDATE ops SET upload_state = 'published' WHERE op_id = ?1", [oid(1)]).unwrap();
    let remote = BTreeMap::from([(1, vec![h1]), (3, vec![h3])]);
    let source = a.rotation_input(VAULT, remote.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!((plan.verified_prefix, plan.retired.applied_seq), (1, 3));
    a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert!(a.forks(VAULT).unwrap().is_empty(), "lag is not a fork");
    assert_eq!(retired(&a), 1, "the reread covers the lagging seq");

    // (3) Exactly ours past S (byte-identical copies count once): nothing.
    let (mut a, [h1, h2, h3]) = a5_world();
    a.conn().execute("UPDATE ops SET upload_state = 'published' WHERE op_id = ?1", [oid(1)]).unwrap();
    let remote = BTreeMap::from([(1, vec![h1]), (2, vec![h2, h2]), (3, vec![h3])]);
    let source = a.rotation_input(VAULT, remote.clone()).unwrap();
    let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
    assert_eq!((plan.verified_prefix, plan.retired.applied_seq), (3, 3));
    a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert!(a.forks(VAULT).unwrap().is_empty());
    assert_eq!(retired(&a), 3, "no clamp");
    let (mut a, [_, h2, h3]) = a5_world();
    let remote = BTreeMap::from([(1, vec![other]), (2, vec![h2]), (3, vec![h3, h3])]);
    rotate_a5(&mut a, &remote);
    assert!(a.forks(VAULT).unwrap().is_empty(), "exactly ours past S records nothing");
    assert_eq!(retired(&a), 0, "the planner's applied_seq (S) is kept");

    // (4) No kept op at seq 4 but two remote contents there: a fork at 4.
    let (mut a, [_, h2, h3]) = a5_world();
    let remote = BTreeMap::from([(1, vec![other]), (2, vec![h2]), (3, vec![h3]), (4, vec![[1; 32], [2; 32]])]);
    rotate_a5(&mut a, &remote);
    assert_eq!(a.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 4)]));
}

/// Architect B1 at store level: reader C applies b's edit E (base X) before
/// a's segment holding X. No conflict copy or notice appears in between, and
/// once X arrives C equals a reader that got X first.
#[test]
fn orphan_base_edit_applied_before_its_base_makes_no_copy() {
    let mut remote = Remote::new(true);
    let mut b = device("b");
    let mut a = device("a");
    local(&mut b, create(1, 1_000, M, "base"));
    publish(&mut b, &mut remote).unwrap();
    sync_stream(&mut a, &remote, "b", &all);
    local(&mut a, edit(2, 2_000, M, 1, "x"));
    publish(&mut a, &mut remote).unwrap();
    sync_stream(&mut b, &remote, "a", &all);
    local(&mut b, edit(3, 3_000, M, 2, "e"));
    publish(&mut b, &mut remote).unwrap();

    let mut c = device("c");
    sync_stream(&mut c, &remote, "b", &all);
    let partial = c.projection(VAULT).unwrap();
    assert!(partial.notices.is_empty(), "lineage unknown: no copy, no notice");
    assert_eq!(partial.entities.len(), 1, "no copy entity");
    let copies: i64 =
        c.conn().query_row("SELECT COUNT(*) FROM entities WHERE id <> ?1", [M], |r| r.get(0)).unwrap();
    assert_eq!(copies, 0, "no copy persisted");
    sync_stream(&mut c, &remote, "a", &all);

    let mut d = device("d");
    sync_stream(&mut d, &remote, "a", &all);
    sync_stream(&mut d, &remote, "b", &all);
    for store in [&c, &d, &b] {
        assert_eq!(hash(store), hash(&d), "converges once the base arrives");
        assert_eq!(memo_state(store, M).text, "e");
        assert!(store.projection(VAULT).unwrap().notices.is_empty());
    }
}

/// Architect B1 / AC 19b list lag on a forked stream: `a` and its VM clone
/// both publish seq 1 (Drive keeps both objects), then the clone continues
/// at 2 and 3. Reader r1's listing shows 3 before 2: the child is deferred
/// (no quarantine, cursor still forked, nothing written); the next full
/// relist applies 2 and then 3. Every reader order ends with the same hash
/// and no edit lost.
#[test]
fn ac19b_forked_stream_child_listed_before_its_parent_is_deferred_then_applied() {
    let mut remote = Remote::new(false);
    let mut b = device("b");
    local(&mut b, create(1, 100, M, "base"));
    publish(&mut b, &mut remote).unwrap();
    let mut a = device("a");
    sync_stream(&mut a, &remote, "b", &all);
    let dir = TempDir::new("ac19b-lag");
    let mut clone = clone_store(&a, &dir);
    local(&mut a, edit(2, 200, M, 1, "original"));
    local(&mut clone, edit(3, 210, M, 1, "clone"));
    publish(&mut a, &mut remote).unwrap();
    publish(&mut clone, &mut remote).unwrap();
    local(&mut clone, create(4, 300, K, "clone two"));
    publish(&mut clone, &mut remote).unwrap();
    local(&mut clone, edit(5, 310, K, 4, "clone three"));
    publish(&mut clone, &mut remote).unwrap();
    assert_eq!(remote.objects[&remote_key("a", 1, 1)].len(), 2);
    let parent = remote_key("a", 2, 2);

    // r2 lists in order; r3 sees only the clone's objects first.
    let mut r2 = device("r2");
    sync_stream(&mut r2, &remote, "b", &all);
    sync_stream(&mut r2, &remote, "a", &all);
    let mut r3 = device("r3");
    sync_stream(&mut r3, &remote, "a", &|key, i| key != remote_key("a", 1, 1) || i == 1);
    sync_stream(&mut r3, &remote, "a", &all);
    sync_stream(&mut r3, &remote, "b", &all);

    let mut r1 = device("r1");
    sync_stream(&mut r1, &remote, "b", &all);
    let first = remote_key("a", 1, 1);
    let forked = sync_stream(&mut r1, &remote, "a", &|key, _| key == first);
    assert!(matches!(forked[..], [ApplyOutcome::Applied { fork_started: true, .. }]), "{forked:?}");
    let before = r1.dump_state().unwrap();
    let lagged = sync_stream(&mut r1, &remote, "a", &|key, _| key != parent);
    assert!(matches!(lagged[1], ApplyOutcome::Deferred { .. }), "{lagged:?}");
    assert!(r1.quarantine_rows(VAULT).unwrap().is_empty());
    assert_eq!(r1.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);
    assert!(r1.dots_of(VAULT, &oid(5)).unwrap().is_empty(), "the child wrote nothing");
    assert_eq!(r1.dump_state().unwrap(), before, "a deferral changes nothing");
    // Next cycle: the engine relists the forked stream in full.
    let relisted = sync_stream(&mut r1, &remote, "a", &all);
    assert!(
        relisted.iter().all(|o| matches!(o, ApplyOutcome::Applied { .. } | ApplyOutcome::Duplicate)),
        "{relisted:?}"
    );

    for store in [&r1, &r2, &r3] {
        assert_eq!(hash(store), hash(&r2), "readers converge");
        for n in 1..=5 {
            assert!(!store.dots_of(VAULT, &oid(n)).unwrap().is_empty(), "op {n} applied");
        }
        assert_eq!(store.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);
        assert_eq!(store.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]));
        assert!(store.quarantine_rows(VAULT).unwrap().is_empty());
        assert_eq!(memo_state(store, K).text, "clone three", "no edit lost");
    }
}
