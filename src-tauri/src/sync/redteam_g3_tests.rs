//! G3 red team (ultragoal G003): adversarial tests for the local store, ops,
//! causal merge, and compaction, derived from the approved plan's acceptance
//! criteria (§4.1, §4.3–§4.5, §4.6.1; AC 1–3b, 7, 8, 16b, 16c, 19, 19b, 24)
//! rather than from the implementation.
//!
//! A defect found by a review pass is first reproduced as
//! `#[ignore = "G3 blocker: ..."]`, then un-ignored when fixed; the doc keeps a
//! "History:" note. No test in this file is ignored now.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};

use super::causal::{eff_ctx, saw, Forks, Insert, OpSet};
use super::compaction::{
    build_snapshot, checkpoint, restore_equivalent, segments_deletable, verify_snapshot, Snapshot, KEEP_CHECKPOINTS,
};
use super::crypto::{derive_subkey, hex, Purpose, SecretKey};
use super::envelope::{seal, ObjectAddress};
use super::identity::{genesis_chain_head, DeviceIdentity};
use super::merge::{conflict_copy_id, invisible_memo_edits, project, EntityState, MemoState, Notice, Projection};
use super::ops::{
    canonical_json, check_plaintext, Dot, Hlc, HlcClock, Kind, OpBody, OpError, SegOp, Segment, MAX_CLOCK_SKEW_MS,
    MAX_STORED,
};
use super::rotation::UploadState;
use super::sim_tests::{
    all, assert_one_dot_each, create, delete, device, edit, fork_world, hash, identity, local, memo, oid, op, publish,
    remote_key, seal_next, seg_key, sync_stream, ForkWorld, Remote, TempDir, EPOCH, K, M, VAULT,
};
use super::store::{open_segment, ApplyOutcome, CursorState, Store};

const T0: u64 = 1_800_000_000_000;
const MIN: u64 = 60 * 1000;
const HOUR: u64 = 60 * MIN;
const V2: &str = "vault-2";
const CV: &str = "conv-1";
const TRX: &str = "tr-1";

// ---------------------------------------------------------------- helpers

type Pub = (OpBody, Dot);

fn ctx_of(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs.iter().map(|(d, s)| ((*d).to_owned(), *s)).collect()
}

fn body(n: u64, origin: &str, wall: u64, ctx: &[(&str, u64)], what: (Kind, &str, &str), payload: Value) -> OpBody {
    let body = OpBody {
        v: 1,
        op_id: oid(n),
        origin_device: origin.to_owned(),
        hlc: Hlc { wall_ms: wall, ctr: 0 },
        ctx: ctx_of(ctx),
        obs: Vec::new(),
        kind: what.0,
        entity_id: what.1.to_owned(),
        op_type: what.2.to_owned(),
        payload,
    };
    assert_eq!(body.validate(), Ok(()), "fixture op {n} must be valid");
    body
}

/// Published at `(origin, seq)`.
fn at(body: OpBody, seq: u64) -> Pub {
    let device = body.origin_device.clone();
    (body, Dot { device, seq })
}

fn dot(device: &str, seq: u64) -> Dot {
    Dot { device: device.to_owned(), seq }
}

fn mc(n: u64, origin: &str, wall: u64, ctx: &[(&str, u64)], memo: &str, text: &str) -> OpBody {
    body(n, origin, wall, ctx, (Kind::Memo, memo, "create"), json!({"text": text, "pinned": false}))
}

fn me(n: u64, origin: &str, wall: u64, ctx: &[(&str, u64)], memo: &str, base: u64, text: &str) -> OpBody {
    body(n, origin, wall, ctx, (Kind::Memo, memo, "edit"), json!({"text": text, "base_op_id": oid(base)}))
}

fn md(n: u64, origin: &str, wall: u64, ctx: &[(&str, u64)], memo: &str) -> OpBody {
    body(n, origin, wall, ctx, (Kind::Memo, memo, "delete"), json!({}))
}

fn mp(n: u64, origin: &str, wall: u64, ctx: &[(&str, u64)], memo: &str, pinned: bool) -> OpBody {
    body(n, origin, wall, ctx, (Kind::Memo, memo, "pin"), json!({"pinned": pinned}))
}

fn msg(n: u64, origin: &str, ctx: &[(&str, u64)], msg_id: &str, parent: Option<&str>, ts: u64) -> OpBody {
    let payload = json!({
        "conv_id": CV, "msg_id": msg_id, "parent_id": parent, "role": "assistant",
        "content": format!("content of {msg_id}"), "attachments": [], "ts": ts,
    });
    body(n, origin, T0 + ts, ctx, (Kind::Conv, CV, "msg.add"), payload)
}

fn with_obs(mut body: OpBody, obs: &[u64]) -> OpBody {
    let mut ids: Vec<String> = obs.iter().map(|n| oid(*n)).collect();
    ids.sort();
    body.obs = ids;
    body
}

fn project_with(pubs: &[Pub], forks: &Forks) -> Projection {
    let mut set = OpSet::new();
    for (body, dot) in pubs {
        let got = set.insert(body.clone(), dot.clone()).unwrap();
        assert_ne!(got.outcome, Insert::Conflict, "fixture op {} conflicts", body.op_id);
    }
    project(&set, forks).unwrap()
}

fn project_all(pubs: &[Pub]) -> Projection {
    project_with(pubs, &Forks::new())
}

/// Deterministic shuffles without a rand dependency.
struct Lcg(u64);

impl Lcg {
    fn step(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.step() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

/// Projects `pubs` in `rounds` shuffled orders and asserts one outcome.
fn order_free(pubs: &[Pub], rounds: usize, seed: u64) -> Projection {
    let base = project_all(pubs);
    let mut rng = Lcg(seed);
    let mut order = pubs.to_vec();
    for round in 0..rounds {
        rng.shuffle(&mut order);
        let p = project_all(&order);
        assert_eq!(p.hash, base.hash, "round {round}: projection hash depends on order");
        assert_eq!(p.notices, base.notices, "round {round}: notices depend on order");
        assert_eq!(p.memo_lineage, base.memo_lineage, "round {round}: lineage depends on order");
    }
    base
}

fn memo_of(p: &Projection, id: &str) -> Option<MemoState> {
    match p.entities.get(&(Kind::Memo, id.to_owned())) {
        Some(EntityState::Memo(m)) => Some(m.clone()),
        _ => None,
    }
}

fn is_deleted(p: &Projection, kind: Kind, id: &str) -> bool {
    p.entities.get(&(kind, id.to_owned())) == Some(&EntityState::Deleted)
}

fn is_restored(p: &Projection, kind: Kind, id: &str) -> bool {
    p.notices.contains(&Notice::RestoredAfterDeleteConflict { kind, entity_id: id.to_owned() })
}

fn copies(p: &Projection) -> Vec<(String, String)> {
    p.notices
        .iter()
        .filter_map(|n| match n {
            Notice::MemoConflictCopy { copy_id, loser_op_id, .. } => Some((copy_id.clone(), loser_op_id.clone())),
            _ => None,
        })
        .collect()
}

fn addr(device: &str, first: u64, last: u64) -> ObjectAddress {
    ObjectAddress::segment(VAULT, EPOCH, device, first, last)
}

fn seg_of(device: &str, first: u64, prev: String, ops: Vec<SegOp>) -> Segment {
    let last = first + (ops.len() as u64 - 1);
    Segment { v: 1, device_id: device.to_owned(), first_seq: first, last_seq: last, prev_seg_hash: prev, ops }
}

fn genesis(vault: &str, device: &str) -> String {
    hex(&genesis_chain_head(vault, device))
}

fn sealed(segment: &Segment) -> Vec<u8> {
    seal(&seg_key(), &addr(&segment.device_id, segment.first_seq, segment.last_seq), &segment.encode().unwrap()).unwrap()
}

fn clone_store(store: &Store, dir: &TempDir, name: &str) -> Store {
    let path = dir.0.join(format!("{name}.db"));
    store.conn().execute("VACUUM INTO ?1", [path.to_string_lossy().into_owned()]).unwrap();
    Store::open(&path).unwrap()
}

fn table_names(store: &Store) -> Vec<String> {
    let mut stmt = store.conn().prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

fn has_vault_column(store: &Store, table: &str) -> bool {
    let mut stmt = store.conn().prepare(&format!("SELECT name FROM pragma_table_info('{table}')")).unwrap();
    let cols: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    cols.iter().any(|c| c == "vault_id")
}

fn dump_rows(store: &Store, sql: &str) -> Vec<String> {
    let mut stmt = store.conn().prepare(sql).unwrap();
    let n = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        let mut line = String::new();
        for i in 0..n {
            line.push_str(&format!("{:?}|", row.get_ref(i).unwrap()));
        }
        out.push(line);
    }
    out.sort();
    out
}

/// Every row of every table (wall-clock columns included), except `skip`.
fn dump_all(store: &Store, skip: &[&str]) -> String {
    let mut out = String::new();
    for table in table_names(store) {
        if skip.contains(&table.as_str()) {
            continue;
        }
        out.push_str(&format!("## {table}\n{}\n", dump_rows(store, &format!("SELECT * FROM \"{table}\"")).join("\n")));
    }
    out
}

/// Every vault-scoped row of `vault`, plus the global projection tables.
fn dump_vault(store: &Store, vault: &str) -> String {
    let mut out = String::new();
    for table in table_names(store) {
        let sql = if has_vault_column(store, &table) {
            format!("SELECT * FROM \"{table}\" WHERE vault_id = '{vault}'")
        } else if table == "entities" || table == "msg_tree" {
            format!("SELECT * FROM \"{table}\"")
        } else {
            continue;
        };
        out.push_str(&format!("## {table}\n{}\n", dump_rows(store, &sql).join("\n")));
    }
    out
}

fn count(store: &Store, sql: &str) -> i64 {
    store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn store_memo(store: &Store, id: &str) -> Option<MemoState> {
    match memo(store, id) {
        Some(EntityState::Memo(m)) => Some(m),
        _ => None,
    }
}

fn store_restored(store: &Store, kind: Kind, id: &str) -> bool {
    is_restored(&store.projection(VAULT).unwrap(), kind, id)
}

fn key2() -> SecretKey {
    derive_subkey(&SecretKey::from_bytes([7; 32]), V2, EPOCH, Purpose::Segment)
}

// ---------------------------------------------------------- determinism

/// Every op kind, a concurrent memo edit with a follow-up in the losing
/// lineage, a restamp alias, a redelivery, a two-op fork, a blind delete in
/// the fork range, a delete with obs, a reissued op, and register races.
fn rich_world() -> Vec<Pub> {
    let restamp = {
        let mut b = me(2, "a2", T0 + 20, &[("a", 1), ("b", 1)], M, 1, "from a");
        b.hlc = Hlc { wall_ms: T0 + 20, ctr: 0 };
        b
    };
    vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 20, &[("a", 1)], M, 1, "from a"), 2),
        at(me(2, "a", T0 + 20, &[("a", 1)], M, 1, "from a"), 2),
        (restamp, dot("a2", 1)),
        at(me(3, "b", T0 + 10, &[("a", 1)], M, 1, "from b"), 1),
        at(me(4, "b", T0 + 11, &[("a", 1), ("b", 1)], M, 3, "b follow-up"), 2),
        at(mp(5, "b", T0 + 30, &[("a", 2), ("b", 2)], M, true), 3),
        at(mp(6, "a", T0 + 5, &[("a", 2)], M, false), 3),
        at(mc(7, "c", T0 + 1, &[], K, "c one"), 1),
        at(mc(8, "c", T0 + 2, &[], K, "c two"), 1),
        at(md(9, "d", T0 + 40, &[("c", 1)], K), 1),
        at(body(10, "a", T0 + 50, &[("a", 3)], (Kind::Conv, CV, "create"), json!({"title": "t"})), 4),
        at(msg(11, "a", &[("a", 4)], "m1", None, 60), 5),
        at(msg(12, "b", &[("a", 5), ("b", 3)], "m2", Some("m1"), 70), 4),
        at(msg(13, "c", &[("a", 5)], "m3", Some("m1"), 70), 2),
        at(body(14, "d", T0 + 80, &[("a", 5), ("b", 4), ("d", 1)], (Kind::Conv, CV, "set_active"), json!({"leaf": "m2"})), 2),
        at(body(15, "a", T0 + 90, &[("a", 5)], (Kind::Tr, TRX, "tr.add"), json!({"src": "x", "pinned": false})), 6),
        at(body(16, "b", T0 + 91, &[("a", 6), ("b", 4)], (Kind::Tr, TRX, "tr.pin"), json!({"pinned": true})), 5),
        at(body(17, "d", T0 + 92, &[("a", 6), ("d", 2)], (Kind::Tr, TRX, "tr.delete"), json!({})), 3),
        at(mc(18, "a2", T0 + 93, &[("a", 1)], "memo-z", "reissued"), 2),
        at(body(19, "a", T0 + 94, &[("a", 6)], (Kind::Policy, "retention", "policy.retention"), json!({"days": 30})), 7),
        at(mc(20, "c", T0 + 95, &[("c", 2)], "memo-l", "in fork range"), 3),
        at(with_obs(md(21, "d", T0 + 96, &[("c", 3), ("d", 3)], "memo-l"), &[20]), 4),
    ]
}

#[test]
fn g3_r01_shuffled_ops_aliases_forks_restamps_project_identically() {
    let p = order_free(&rich_world(), 1000, 0x5eed_0001);
    assert_eq!(memo_of(&p, M).unwrap().text, "from a");
    assert!(memo_of(&p, M).unwrap().pinned, "concurrent pins: max (hlc, origin, op_id) wins");
    let copy = conflict_copy_id(M, &oid(3));
    assert_eq!(memo_of(&p, &copy).unwrap().text, "b follow-up", "losing lineage routed to its copy");
    assert!(is_restored(&p, Kind::Memo, K), "a blind delete does not see fork-range ops");
    assert!(is_deleted(&p, Kind::Memo, "memo-l"), "a delete with obs sees the fork-range op");
    assert!(is_deleted(&p, Kind::Tr, TRX));
    match p.entities.get(&(Kind::Conv, CV.to_owned())) {
        Some(EntityState::Conv(c)) => {
            assert_eq!(c.active_leaf.as_deref(), Some("m2"));
            assert_eq!(c.messages.len(), 3);
        }
        other => panic!("conv is {other:?}"),
    }
}

/// AC 19b with three readers that see the two variants of one Drive name in
/// three different orders (variant one first, variant two first, both).
fn forked_remote(continued: bool) -> Remote {
    let mut remote = Remote::new(false);
    let mut b = device("b");
    local(&mut b, create(1, 100, M, "base"));
    publish(&mut b, &mut remote).unwrap();
    let mut a = device("a");
    sync_stream(&mut a, &remote, "b", &all);
    let dir = TempDir::new("g3-fork");
    let mut a_clone = clone_store(&a, &dir, "clone");
    local(&mut a, edit(2, 200, M, 1, "variant one"));
    local(&mut a_clone, edit(3, 210, M, 1, "variant two"));
    assert!(publish(&mut a, &mut remote).unwrap().1);
    assert!(publish(&mut a_clone, &mut remote).unwrap().1);
    if continued {
        // The original has not noticed the clone yet (its own verify saw one
        // object) and keeps writing on its chain.
        local(&mut a, create(7, 300, K, "written after the fork"));
        assert!(publish(&mut a, &mut remote).unwrap().1);
    }
    remote
}

fn three_readers(remote: &Remote) -> [Store; 3] {
    let k11 = remote_key("a", 1, 1);
    let only = |idx: usize| {
        let k11 = k11.clone();
        move |key: &str, i: usize| key != k11 || i == idx
    };
    let mut r1 = device("r1");
    sync_stream(&mut r1, remote, "b", &all);
    sync_stream(&mut r1, remote, "a", &only(0));
    sync_stream(&mut r1, remote, "a", &all);
    let mut r2 = device("r2");
    sync_stream(&mut r2, remote, "a", &only(1));
    sync_stream(&mut r2, remote, "b", &all);
    sync_stream(&mut r2, remote, "a", &all);
    let mut r3 = device("r3");
    sync_stream(&mut r3, remote, "a", &all);
    sync_stream(&mut r3, remote, "b", &all);
    [r1, r2, r3]
}

#[test]
fn g3_r02a_fork_variants_in_three_orders_converge() {
    let remote = forked_remote(false);
    let readers = three_readers(&remote);
    for r in &readers {
        assert_eq!(hash(r), hash(&readers[0]), "readers converge");
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]));
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
        let texts: BTreeSet<String> = r
            .projection(VAULT)
            .unwrap()
            .entities
            .values()
            .filter_map(|e| match e {
                EntityState::Memo(m) => Some(m.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, BTreeSet::from(["variant one".to_owned(), "variant two".to_owned()]), "no edit lost");
    }
}

#[test]
fn g3_r02b_fork_then_continued_writes_converge_on_all_readers() {
    let remote = forked_remote(true);
    let readers = three_readers(&remote);
    let has_k: Vec<bool> = readers.iter().map(|r| store_memo(r, K).is_some()).collect();
    assert_eq!(has_k, [true, true, true], "the post-fork op reaches every reader (no edit lost)");
    for r in &readers {
        assert_eq!(hash(r), hash(&readers[0]), "readers converge");
    }
}

/// §4.4 re-projection when `fork_start` moves down: the result depends only
/// on the final fork map, never on when each fork was discovered.
#[test]
fn g3_r03_reprojection_after_fork_start_decreases() {
    let pubs = vec![
        at(mc(1, "a", T0, &[], "memo-p", "p"), 1),
        at(mc(2, "a", T0 + 1, &[], "memo-q", "q"), 1),
        at(mc(3, "a", T0 + 2, &[("a", 1)], "memo-r", "r"), 2),
        at(mc(4, "a", T0 + 3, &[("a", 2)], "memo-s", "s"), 3),
        at(mc(5, "a", T0 + 4, &[("a", 2)], "memo-t", "t"), 3),
        at(md(6, "b", T0 + 5, &[("a", 3)], "memo-r"), 1),
    ];
    // Without the (a,1) collision the fork starts at 3 and the delete saw r.
    let late = project_all(&[pubs[2].clone(), pubs[3].clone(), pubs[4].clone(), pubs[5].clone()]);
    assert!(is_deleted(&late, Kind::Memo, "memo-r"));
    // Once (a,1) collides, eff_ctx[a] = 0: the delete never saw r.
    let full = order_free(&pubs, 300, 3);
    assert!(is_restored(&full, Kind::Memo, "memo-r"));
    // A stale caller fork map (fork_start 3) cannot raise the set's own minimum.
    let mut set = OpSet::new();
    for (b, d) in &pubs {
        set.insert(b.clone(), d.clone()).unwrap();
    }
    assert_eq!(set.forks(), &Forks::from([("a".to_owned(), 1)]));
    let stale = project(&set, &Forks::from([("a".to_owned(), 3)])).unwrap();
    assert_eq!(stale.hash, full.hash);
    // A caller fork lower than the set's wins (min).
    let lower = project(&set, &Forks::from([("b".to_owned(), 1)])).unwrap();
    assert!(is_restored(&lower, Kind::Memo, "memo-r"));
    assert_eq!(eff_ctx(&ctx_of(&[("a", 9), ("b", 4)]), &Forks::from([("a".to_owned(), 3)])), ctx_of(&[("a", 2), ("b", 4)]));
}

// --------------------------------------------------------------- AC 3b / 24

fn skewed(base: u64, skew: i64) -> u64 {
    base.checked_add_signed(skew).unwrap()
}

const SKEWS: [i64; 4] = [30 * MIN as i64, -(30 * MIN as i64), 48 * HOUR as i64, -(48 * HOUR as i64)];

#[test]
fn g3_r04_clock_skew_never_lets_a_blind_delete_win() {
    for skew in SKEWS {
        let c = at(mc(1, "a", T0, &[], M, "base"), 1);
        let blind = at(md(2, "b", skewed(T0, skew), &[("a", 1)], M), 1);
        let e = at(me(3, "a", T0 + 1_000, &[("a", 1)], M, 1, "kept"), 2);
        let p = order_free(&[c.clone(), blind, e.clone()], 50, skew.unsigned_abs());
        assert_eq!(memo_of(&p, M).map(|m| m.text), Some("kept".to_owned()), "skew {skew}");
        assert!(is_restored(&p, Kind::Memo, M), "skew {skew}");
        // The same skew on the edit side.
        let e_skew = at(me(3, "a", skewed(T0, skew).max(1), &[("a", 1)], M, 1, "kept"), 2);
        let blind_now = at(md(2, "b", T0, &[("a", 1)], M), 1);
        let p = order_free(&[c.clone(), blind_now, e_skew], 50, 7);
        assert!(is_restored(&p, Kind::Memo, M), "edit skew {skew}");
        // A delete that saw the edit deletes, whatever the clocks say.
        let seen = at(md(2, "b", skewed(T0, skew), &[("a", 2)], M), 1);
        let p = order_free(&[c, seen, e], 50, 11);
        assert!(is_deleted(&p, Kind::Memo, M), "skew {skew}");
    }
}

#[test]
fn g3_r04b_three_stores_with_skewed_delete_keep_the_edit_and_store_hlc_verbatim() {
    for skew in SKEWS {
        let mut remote = Remote::new(true);
        let (mut a, mut b, mut c) = (device("a"), device("b"), device("c"));
        local(&mut a, create(1, T0, M, "base"));
        publish(&mut a, &mut remote).unwrap();
        sync_stream(&mut b, &remote, "a", &all);
        let wall = skewed(T0, skew);
        local(&mut b, delete(2, wall, M));
        publish(&mut b, &mut remote).unwrap();
        local(&mut a, edit(3, T0 + 1_000, M, 1, "kept"));
        publish(&mut a, &mut remote).unwrap();
        sync_stream(&mut c, &remote, "a", &all);
        sync_stream(&mut c, &remote, "b", &all);
        sync_stream(&mut a, &remote, "b", &all);
        sync_stream(&mut b, &remote, "a", &all);
        for s in [&a, &b, &c] {
            assert_eq!(hash(s), hash(&a), "skew {skew}: three devices converge");
            assert_eq!(store_memo(s, M).map(|m| m.text), Some("kept".to_owned()), "skew {skew}");
            assert!(store_restored(s, Kind::Memo, M), "skew {skew}");
            let stored: u64 = s
                .conn()
                .query_row("SELECT hlc_wall FROM ops WHERE vault_id = ?1 AND op_id = ?2", [VAULT, &oid(2)], |r| r.get(0))
                .unwrap();
            assert_eq!(stored, wall, "skew {skew}: HLC stored verbatim");
        }
    }
}

#[test]
fn g3_r05_register_causality_beats_a_skewed_hlc() {
    for skew in SKEWS {
        let c = at(mc(1, "a", T0, &[], M, "base"), 1);
        let pin_b = at(mp(2, "b", skewed(T0, skew), &[("a", 1)], M, true), 1);
        // a saw b's pin and unpinned with a clock far behind it (or ahead).
        let unpin_seen = at(mp(3, "a", skewed(T0, -skew), &[("a", 1), ("b", 1)], M, false), 2);
        let p = order_free(&[c.clone(), pin_b.clone(), unpin_seen], 50, 5);
        assert!(!memo_of(&p, M).unwrap().pinned, "skew {skew}: causally later wins");
        // Concurrent: the tie key (hlc first) decides.
        let unpin_conc = at(mp(3, "a", skewed(T0, -skew), &[("a", 1)], M, false), 2);
        let p = order_free(&[c, pin_b, unpin_conc], 50, 6);
        assert_eq!(memo_of(&p, M).unwrap().pinned, skew > 0, "skew {skew}: concurrent max hlc wins");
    }
}

#[test]
fn g3_r06_ac24_hlc_clamp_and_monotonic_tick() {
    let now = T0;
    let mut clock = HlcClock::default();
    clock.observe(Hlc { wall_ms: now + 48 * HOUR, ctr: 9 }, now);
    assert_eq!(clock.last, Hlc { wall_ms: now + MAX_CLOCK_SKEW_MS, ctr: 0 });
    assert_eq!(clock.clamp_warnings, 1);
    let t = clock.tick(now);
    assert!(t.wall_ms <= now + MAX_CLOCK_SKEW_MS && t > Hlc { wall_ms: now + MAX_CLOCK_SKEW_MS, ctr: 0 });
    clock.observe(Hlc { wall_ms: now + HOUR, ctr: 3 }, now);
    assert_eq!(clock.clamp_warnings, 1, "an older stamp is no warning");
    assert_eq!(clock.last, t, "an older stamp never moves the clock back");
    // Exactly +24h is not clamped.
    let mut edge = HlcClock::default();
    edge.observe(Hlc { wall_ms: now + MAX_CLOCK_SKEW_MS, ctr: 4 }, now);
    assert_eq!((edge.last, edge.clamp_warnings), (Hlc { wall_ms: now + MAX_CLOCK_SKEW_MS, ctr: 4 }, 0));
    // Counter overflow moves wall forward instead of wrapping back.
    let mut full = HlcClock { last: Hlc { wall_ms: now, ctr: u32::MAX }, clamp_warnings: 0 };
    let next = full.tick(now);
    assert!(next > Hlc { wall_ms: now, ctr: u32::MAX });
    // Repeated ticks are strictly increasing under a frozen or receding clock.
    let mut prev = full.tick(now - HOUR);
    for i in 0..1000 {
        let t = full.tick(now - i);
        assert!(t > prev);
        prev = t;
    }
    // observe near u64::MAX does not overflow.
    let mut huge = HlcClock::default();
    huge.observe(Hlc { wall_ms: u64::MAX, ctr: u32::MAX }, u64::MAX - 1);
    assert_eq!(huge.last.wall_ms, u64::MAX);
}

// ------------------------------------------------------- delete correctness

#[test]
fn g3_r07_delete_that_saw_only_an_alias_dot_deletes() {
    let x = mc(1, "a", T0, &[], M, "x");
    let mut alias = x.clone();
    alias.origin_device = "a2".to_owned();
    alias.ctx = ctx_of(&[("q", 4)]);
    let del = at(md(2, "b", T0 + 1, &[("a2", 1)], M), 1);
    let p = order_free(&[(x.clone(), dot("a", 5)), (alias.clone(), dot("a2", 1)), del.clone()], 100, 1);
    assert!(is_deleted(&p, Kind::Memo, M), "a2:1 is an alias dot of x");
    // With a fork on a covering x's original dot, the alias still counts.
    let other = (mc(3, "a", T0, &[], "memo-o", "o"), dot("a", 5));
    let p = order_free(&[(x.clone(), dot("a", 5)), (alias, dot("a2", 1)), other.clone(), del], 100, 2);
    assert!(is_deleted(&p, Kind::Memo, M));
    // Without the alias, a delete that names only a's forked range keeps x.
    let del_a = at(md(2, "b", T0 + 1, &[("a", 9)], M), 1);
    let p = order_free(&[(x, dot("a", 5)), other, del_a], 100, 3);
    assert!(is_restored(&p, Kind::Memo, M));
}

#[test]
fn g3_r08_r09_fork_range_delete_needs_obs_for_every_variant() {
    let c = at(mc(1, "b", T0, &[], M, "base"), 1);
    let x1 = at(me(2, "a", T0 + 1, &[("b", 1)], M, 1, "variant one"), 2);
    let x2 = at(me(3, "a", T0 + 2, &[("b", 1)], M, 1, "variant two"), 2);
    let blind = at(md(4, "c", T0 + 3, &[("a", 9), ("b", 1)], M), 1);
    let p = order_free(&[c.clone(), x1.clone(), x2.clone(), blind], 200, 8);
    assert!(is_restored(&p, Kind::Memo, M), "ctx alone never sees fork-range ops");
    // A re-delete that observed only one variant still keeps the memo.
    let partial = at(with_obs(md(5, "c", T0 + 4, &[("a", 9), ("b", 1), ("c", 1)], M), &[2]), 2);
    let p = order_free(&[c.clone(), x1.clone(), x2.clone(), partial], 200, 9);
    assert!(!is_deleted(&p, Kind::Memo, M), "variant two was not observed");
    // ...and the copy of the losing variant is a separate memo.
    let copy = conflict_copy_id(M, &oid(2));
    assert_eq!(memo_of(&p, &copy).map(|m| m.text), Some("variant one".to_owned()));
    // obs naming both variants deletes M (the copy has its own lifecycle).
    let both = at(with_obs(md(5, "c", T0 + 4, &[("a", 9), ("b", 1), ("c", 1)], M), &[2, 3]), 2);
    let p = order_free(&[c, x1, x2, both], 200, 10);
    assert!(is_deleted(&p, Kind::Memo, M));
    assert!(!is_restored(&p, Kind::Memo, M));
}

#[test]
fn g3_r10_register_ops_after_delete_never_resurrect() {
    let c = at(mc(1, "a", T0, &[], M, "base"), 1);
    let del = at(md(2, "b", T0 + 10, &[("a", 1)], M), 1);
    let pin_conc = at(mp(3, "a", T0 + 20, &[("a", 1)], M, true), 2);
    let pin_after = at(mp(4, "a", T0 + 30, &[("a", 2), ("b", 1)], M, false), 3);
    let edit_after = at(me(5, "a", T0 + 40, &[("a", 3), ("b", 1)], M, 1, "edited after the delete"), 4);
    let p = order_free(&[c.clone(), del.clone(), pin_conc, pin_after, edit_after], 200, 12);
    assert!(is_deleted(&p, Kind::Memo, M), "registers never block; ops that saw the delete are ignored");
    assert!(!is_restored(&p, Kind::Memo, M));
    // A concurrent note is a content op (C includes note) and restores.
    let note = at(body(6, "c", T0 + 5, &[("a", 1)], (Kind::Memo, M, "note"), json!({"note": "n"})), 1);
    let p = order_free(&[c, del, note], 50, 13);
    assert!(is_restored(&p, Kind::Memo, M));

    let conv = at(body(10, "a", T0, &[], (Kind::Conv, CV, "create"), json!({"title": "t"})), 1);
    let cdel = at(body(11, "b", T0 + 1, &[("a", 1)], (Kind::Conv, CV, "delete"), json!({})), 1);
    let active = at(body(12, "a", T0 + 2, &[("a", 1)], (Kind::Conv, CV, "set_active"), json!({"leaf": "m9"})), 2);
    let p = order_free(&[conv.clone(), cdel.clone(), active], 50, 14);
    assert!(is_deleted(&p, Kind::Conv, CV), "set_active is a register");
    let rename = at(body(13, "c", T0 + 3, &[("a", 1)], (Kind::Conv, CV, "rename"), json!({"title": "u"})), 1);
    let late_msg = at(msg(14, "d", &[("a", 1)], "m1", None, 5), 1);
    let p = order_free(&[conv.clone(), cdel.clone(), rename], 50, 15);
    assert!(is_restored(&p, Kind::Conv, CV), "rename is content");
    let p = order_free(&[conv, cdel, late_msg], 50, 16);
    assert!(is_restored(&p, Kind::Conv, CV), "msg.add is content");

    let tadd = at(body(20, "a", T0, &[], (Kind::Tr, TRX, "tr.add"), json!({"src": "s"})), 1);
    let tdel = at(body(21, "b", T0 + 1, &[("a", 1)], (Kind::Tr, TRX, "tr.delete"), json!({})), 1);
    let tpin = at(body(22, "a", T0 + 2, &[("a", 1)], (Kind::Tr, TRX, "tr.pin"), json!({"pinned": true})), 2);
    let p = order_free(&[tadd, tdel, tpin], 50, 17);
    assert!(is_deleted(&p, Kind::Tr, TRX), "tr.pin is a register");
    // A delete of an entity never created is a tombstone, and a later
    // unseen create restores it.
    let ghost_del = at(md(30, "b", T0, &[], "memo-ghost"), 1);
    assert!(is_deleted(&project_all(std::slice::from_ref(&ghost_del)), Kind::Memo, "memo-ghost"));
    let ghost_create = at(mc(31, "a", T0 + 1, &[], "memo-ghost", "late"), 1);
    assert!(is_restored(&project_all(&[ghost_del, ghost_create]), Kind::Memo, "memo-ghost"));
}

// ---------------------------------------------------- memo conflict copies

#[test]
fn g3_r11_three_way_concurrent_edits_one_winner_two_copies() {
    let pubs = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 1, &[("a", 1)], M, 1, "from a"), 2),
        at(me(3, "b", T0 + 2, &[("a", 1)], M, 1, "from b"), 1),
        at(me(4, "c", T0 + 3, &[("a", 1)], M, 1, "from c"), 1),
    ];
    let p = order_free(&pubs, 500, 21);
    assert_eq!(memo_of(&p, M).unwrap().text, "from c");
    let mut got = copies(&p);
    got.sort();
    let mut want = vec![(conflict_copy_id(M, &oid(2)), oid(2)), (conflict_copy_id(M, &oid(3)), oid(3))];
    want.sort();
    assert_eq!(got, want);
    for (copy, loser) in &want {
        let m = memo_of(&p, copy).unwrap();
        assert_eq!(m.conflict_of.as_deref(), Some(M));
        assert_eq!(m.head_op_id, *loser);
    }
    assert_eq!(p.entities.len(), 3);
}

#[test]
fn g3_r12_identical_text_produces_no_copy() {
    let pubs = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 1, &[("a", 1)], M, 1, "same"), 2),
        at(me(3, "b", T0 + 2, &[("a", 1)], M, 1, "same"), 1),
        at(me(4, "c", T0 + 3, &[("a", 1)], M, 1, "same"), 1),
    ];
    let p = order_free(&pubs, 300, 22);
    assert!(copies(&p).is_empty());
    assert_eq!(memo_of(&p, M).unwrap().text, "same");
    assert_eq!(p.entities.len(), 1);
    // A follow-up on a same-text loser continues the main memo.
    let mut more = pubs;
    more.push(at(me(5, "b", T0 + 4, &[("a", 1), ("b", 1)], M, 3, "next"), 2));
    let p = order_free(&more, 300, 23);
    assert!(copies(&p).is_empty());
    assert_eq!(memo_of(&p, M).unwrap().text, "next");
}

#[test]
fn g3_r13_identical_losing_texts_share_one_copy() {
    let pubs = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 1, &[("a", 1)], M, 1, "dup"), 2),
        at(me(3, "b", T0 + 2, &[("a", 1)], M, 1, "dup"), 1),
        at(me(4, "c", T0 + 3, &[("a", 1)], M, 1, "winner"), 1),
    ];
    let p = order_free(&pubs, 300, 24);
    assert_eq!(memo_of(&p, M).unwrap().text, "winner");
    let texts: Vec<String> = copies(&p).iter().map(|(id, _)| memo_of(&p, id).unwrap().text).collect();
    assert_eq!(texts, ["dup"], "two losers with the same text are one conflict, not two identical copies");
}

#[test]
fn g3_r14_follow_up_edits_in_a_losing_lineage_route_to_the_copy() {
    let copy = conflict_copy_id(M, &oid(2));
    let pubs = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 1, &[("a", 1)], M, 1, "a1"), 2),
        at(me(3, "b", T0 + 2, &[("a", 1)], M, 1, "b1"), 1),
        // a keeps editing its (losing) text, still addressing M.
        at(me(4, "a", T0 + 3, &[("a", 2)], M, 2, "a2"), 3),
        // a's UI already shows the copy and addresses it.
        at(body(5, "a", T0 + 4, &[("a", 3), ("b", 1)], (Kind::Memo, &copy, "edit"), json!({"text": "a3", "base_op_id": oid(4)})), 4),
        // b edits the copy concurrently: a nested copy.
        at(body(6, "b", T0 + 5, &[("a", 3), ("b", 1)], (Kind::Memo, &copy, "edit"), json!({"text": "b-on-copy", "base_op_id": oid(4)})), 2),
        // b continues M.
        at(me(7, "b", T0 + 6, &[("a", 1), ("b", 2)], M, 3, "b2"), 3),
    ];
    let p = order_free(&pubs, 500, 25);
    assert_eq!(memo_of(&p, M).unwrap().text, "b2");
    assert_eq!(memo_of(&p, &copy).unwrap().text, "b-on-copy");
    let nested = conflict_copy_id(&copy, &oid(5));
    assert_eq!(memo_of(&p, &nested).unwrap().text, "a3");
    assert_eq!(memo_of(&p, &nested).unwrap().conflict_of.as_deref(), Some(copy.as_str()));
    assert_eq!(p.memo_lineage.get(&oid(4)).map(String::as_str), Some(copy.as_str()));
    assert_eq!(p.memo_lineage.get(&oid(7)).map(String::as_str), Some(M));

    // Store level: memo_lineage persists the routing.
    let mut remote = Remote::new(true);
    let (mut a, mut b) = (device("a"), device("b"));
    local(&mut a, create(1, T0, M, "base"));
    publish(&mut a, &mut remote).unwrap();
    sync_stream(&mut b, &remote, "a", &all);
    local(&mut a, edit(2, T0 + 1, M, 1, "a1"));
    local(&mut b, edit(3, T0 + 2, M, 1, "b1"));
    local(&mut a, edit(4, T0 + 3, M, 2, "a2"));
    publish(&mut a, &mut remote).unwrap();
    publish(&mut b, &mut remote).unwrap();
    sync_stream(&mut a, &remote, "b", &all);
    sync_stream(&mut b, &remote, "a", &all);
    assert_eq!(hash(&a), hash(&b));
    for s in [&a, &b] {
        let routed: String = s
            .conn()
            .query_row("SELECT routed_to FROM memo_lineage WHERE vault_id = ?1 AND op_id = ?2", [VAULT, &oid(4)], |r| r.get(0))
            .unwrap();
        assert_eq!(routed, copy);
        assert_eq!(store_memo(s, &copy).unwrap().text, "a2");
    }
}

// ------------------------------------------------------- conversation tree

#[test]
fn g3_r15_regenerated_siblings_and_newest_leaf() {
    let pubs = vec![
        at(body(1, "a", T0, &[], (Kind::Conv, CV, "create"), json!({"title": "t"})), 1),
        at(msg(2, "a", &[("a", 1)], "m1", None, 10), 2),
        at(msg(3, "a", &[("a", 2)], "m2", Some("m1"), 20), 3),
        at(msg(4, "b", &[("a", 3)], "m3", Some("m1"), 30), 1),
        at(msg(5, "c", &[("a", 3)], "m4", Some("m1"), 30), 1),
    ];
    let p = order_free(&pubs, 300, 31);
    let Some(EntityState::Conv(conv)) = p.entities.get(&(Kind::Conv, CV.to_owned())) else { panic!("conv missing") };
    assert_eq!(conv.messages.len(), 4);
    let siblings = conv.messages.values().filter(|m| m.parent_id.as_deref() == Some("m1")).count();
    assert_eq!(siblings, 3, "regenerate makes siblings, never replaces");
    assert_eq!(conv.active_leaf.as_deref(), Some("m4"), "newest leaf by (ts, msg_id)");
}

#[test]
fn g3_r16_set_active_to_an_unknown_leaf_falls_back_until_it_arrives() {
    let base = vec![
        at(body(1, "a", T0, &[], (Kind::Conv, CV, "create"), json!({"title": "t"})), 1),
        at(msg(2, "a", &[("a", 1)], "m1", None, 10), 2),
        at(msg(3, "a", &[("a", 2)], "m2", Some("m1"), 20), 3),
        at(body(4, "b", T0 + 40, &[("a", 3)], (Kind::Conv, CV, "set_active"), json!({"leaf": "ghost"})), 1),
    ];
    let p = order_free(&base, 100, 32);
    let Some(EntityState::Conv(conv)) = p.entities.get(&(Kind::Conv, CV.to_owned())) else { panic!("conv missing") };
    assert_eq!(conv.active_leaf.as_deref(), Some("m2"), "unknown leaf is ignored");
    let mut more = base;
    more.push(at(msg(5, "c", &[("a", 3)], "ghost", Some("m1"), 5), 1));
    let p = order_free(&more, 100, 33);
    let Some(EntityState::Conv(conv)) = p.entities.get(&(Kind::Conv, CV.to_owned())) else { panic!("conv missing") };
    assert_eq!(conv.active_leaf.as_deref(), Some("ghost"), "the register applies once the leaf exists");
}

#[test]
fn g3_r17_message_cycles_and_dangling_parents_do_not_panic() {
    let pubs = vec![
        at(body(1, "a", T0, &[], (Kind::Conv, CV, "create"), json!({"title": "t"})), 1),
        at(msg(2, "a", &[("a", 1)], "x", Some("y"), 10), 2),
        at(msg(3, "a", &[("a", 2)], "y", Some("x"), 20), 3),
        at(msg(4, "b", &[], "z", Some("nope"), 30), 1),
        at(msg(5, "b", &[], "self", Some("self"), 40), 2),
    ];
    let p = order_free(&pubs, 100, 34);
    let Some(EntityState::Conv(conv)) = p.entities.get(&(Kind::Conv, CV.to_owned())) else { panic!("conv missing") };
    assert_eq!(conv.messages.len(), 4);
    assert_eq!(conv.active_leaf.as_deref(), Some("z"));
}

// ------------------------------------------------------------- store AC 7

struct StoreWorld {
    seg2: Vec<u8>,
    seg2_plain: Segment,
    reader: Store,
}

/// a publishes 1-1 and 2-2; the reader applied 1-1 only.
fn store_world() -> StoreWorld {
    let mut remote = Remote::new(true);
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "one"));
    publish(&mut a, &mut remote).unwrap();
    local(&mut a, edit(2, 2_000, M, 1, "two"));
    let (seg2, _) = publish(&mut a, &mut remote).unwrap();
    let mut reader = device("r");
    let k11 = remote_key("a", 1, 1);
    sync_stream(&mut reader, &remote, "a", &|key, _| key == k11);
    assert_eq!(reader.cursor(VAULT, "a").unwrap().unwrap().applied_seq, 1);
    let seg2_plain = open_segment(&seg_key(), &addr("a", 2, 2), &seg2.bytes).unwrap();
    StoreWorld { seg2: seg2.bytes, seg2_plain, reader }
}

#[test]
fn g3_r18_ac7_quarantine_only_failures_leave_every_table_byte_identical() {
    let key = seg_key();
    let mut cases: Vec<(&str, ObjectAddress, Vec<u8>)> = Vec::new();
    let w = store_world();
    let mut flipped = w.seg2.clone();
    let mid = flipped.len() / 2;
    flipped[mid] ^= 0x80;
    cases.push(("corrupt", addr("a", 2, 2), flipped));
    let mut truncated = w.seg2.clone();
    truncated.truncate(40);
    cases.push(("truncated", addr("a", 2, 2), truncated));
    // A newer envelope version byte is a hold, not a quarantine (§4.5 line
    // 242, critic B1): asserted after this loop.
    cases.push(("aad range", addr("a", 3, 3), w.seg2.clone()));
    cases.push(("aad device", addr("c", 2, 2), w.seg2.clone()));
    cases.push(("aad vault", ObjectAddress::segment(V2, EPOCH, "a", 2, 2), w.seg2.clone()));
    cases.push(("wrong key", addr("a", 2, 2), seal(&key2(), &addr("a", 2, 2), &w.seg2_plain.encode().unwrap()).unwrap()));
    let plain = w.seg2_plain.encode().unwrap();
    cases.push(("cross-check device", addr("c", 2, 2), seal(&key, &addr("c", 2, 2), &plain).unwrap()));
    cases.push(("cross-check range", addr("a", 2, 3), seal(&key, &addr("a", 2, 3), &plain).unwrap()));
    let mut spaced = plain.clone();
    spaced.insert(1, b' ');
    cases.push(("non-canonical whitespace", addr("a", 2, 2), seal(&key, &addr("a", 2, 2), &spaced).unwrap()));
    let mut dup = b"{\"v\":1,".to_vec();
    dup.extend_from_slice(&plain[1..]);
    cases.push(("non-canonical duplicate key", addr("a", 2, 2), seal(&key, &addr("a", 2, 2), &dup).unwrap()));
    let mut gap = w.seg2_plain.clone();
    gap.ops[0].seq = 3;
    cases.push(("seqs not contiguous", addr("a", 2, 2), seal(&key, &addr("a", 2, 2), &gap.encode().unwrap()).unwrap()));

    let mut reader = w.reader;
    let tables = table_names(&reader).len();
    for (name, address, bytes) in &cases {
        let before = dump_all(&reader, &["quarantine"]);
        let quarantined = reader.quarantine_rows(VAULT).unwrap().len();
        let out = reader.apply_sealed(VAULT, &key, address, bytes).unwrap();
        assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "{name}: {out:?}");
        assert_eq!(dump_all(&reader, &["quarantine"]), before, "{name}: every table except quarantine unchanged");
        // One row per object key; a second failure under the same key updates it.
        let qkey = address.remote_key().unwrap();
        let rows = reader.quarantine_rows(VAULT).unwrap();
        assert!(rows.iter().any(|(k, _)| *k == qkey), "{name}: quarantined under {qkey}");
        assert!(rows.len() == quarantined || rows.len() == quarantined + 1, "{name}");
        assert_eq!(table_names(&reader).len(), tables);
    }
    // The genuine object still applies afterwards.
    let out = reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &w.seg2).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { conflicts: 0, .. }), "{out:?}");
    assert_eq!(store_memo(&reader, M).unwrap().text, "two");

    // Envelope version 0x02: FutureVersion(2), cursor held, no quarantine row,
    // every other table unchanged.
    let w = store_world();
    let mut reader = w.reader;
    let mut version = w.seg2.clone();
    version[0] = 0x02;
    let before = dump_all(&reader, &["quarantine", "cursors"]);
    let out = reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &version).unwrap();
    assert_eq!(out, ApplyOutcome::FutureVersion(2));
    assert_eq!(reader.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held);
    assert!(reader.quarantine_rows(VAULT).unwrap().is_empty());
    assert_eq!(dump_all(&reader, &["quarantine", "cursors"]), before);
}

#[test]
fn g3_r19_hold_failures_change_only_the_cursor_state() {
    let key = seg_key();
    type Mutate = fn(&mut Segment);
    let cases: [(&str, Mutate, bool, bool); 5] = [
        ("chain break", |s| s.prev_seg_hash = "0".repeat(64), true, false),
        ("future segment version", |s| s.v = 2, false, true),
        ("future op version", |s| s.ops[0].body.v = 2, false, true),
        ("schema: bad entity id", |s| s.ops[0].body.entity_id = "x y".to_owned(), true, false),
        ("foreign origin", |s| s.ops[0].body.origin_device = "c".to_owned(), true, false),
    ];
    for (name, mutate, quarantine, future) in cases {
        let w = store_world();
        let mut reader = w.reader;
        let mut seg = w.seg2_plain.clone();
        mutate(&mut seg);
        let bytes = seal(&key, &addr("a", 2, 2), &seg.encode().unwrap()).unwrap();
        let before = dump_all(&reader, &["quarantine", "cursors"]);
        let cursor = reader.cursor(VAULT, "a").unwrap().unwrap();
        let out = reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &bytes).unwrap();
        if future {
            assert!(matches!(out, ApplyOutcome::FutureVersion(2)), "{name}: {out:?}");
        } else {
            assert!(matches!(out, ApplyOutcome::Held { .. }), "{name}: {out:?}");
        }
        assert_eq!(dump_all(&reader, &["quarantine", "cursors"]), before, "{name}: state unchanged");
        let after = reader.cursor(VAULT, "a").unwrap().unwrap();
        assert_eq!((after.applied_seq, after.chain_head), (cursor.applied_seq, cursor.chain_head), "{name}");
        assert_eq!(after.state, CursorState::Held, "{name}");
        assert_eq!(reader.quarantine_rows(VAULT).unwrap().len(), usize::from(quarantine), "{name}");
        // Held: even the genuine object is now skipped (no auto recovery, v1).
        let out = reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &w.seg2).unwrap();
        assert_eq!(out, ApplyOutcome::Skipped(CursorState::Held), "{name}");
    }
}

#[test]
fn g3_r20_op_id_content_conflict_quarantines_only_that_op() {
    let key = seg_key();
    let w = store_world();
    let mut reader = w.reader;
    let seg1_hash = reader.cursor(VAULT, "a").unwrap().unwrap().chain_head.unwrap();
    let original = reader.load_set(VAULT).unwrap().0.get(&oid(1)).unwrap().body.clone();
    let mut forged = original.clone();
    forged.payload = json!({"text": "forged", "pinned": true});
    let mut fresh = op(5, 5_000, Kind::Memo, K, "create", json!({"text": "fresh", "pinned": false}));
    fresh.origin_device = "a".to_owned();
    fresh.ctx = ctx_of(&[("a", 2)]);
    let seg = seg_of("a", 2, hex(&seg1_hash), vec![SegOp { seq: 2, body: forged }, SegOp { seq: 3, body: fresh }]);
    let row = |s: &Store| dump_rows(s, &format!("SELECT * FROM ops WHERE op_id = '{}'", oid(1)));
    let before_row = row(&reader);
    let before_dots = reader.dots_of(VAULT, &oid(1)).unwrap();
    let out = reader.apply_sealed(VAULT, &key, &addr("a", 2, 3), &sealed(&seg)).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { conflicts: 1, .. }), "{out:?}");
    assert_eq!(row(&reader), before_row, "the stored op row is byte-identical");
    assert_eq!(reader.dots_of(VAULT, &oid(1)).unwrap(), before_dots, "the forged dot is skipped");
    assert!(reader.quarantine_rows(VAULT).unwrap().iter().any(|(k, _)| *k == format!("op/{}", oid(1))));
    assert_eq!(store_memo(&reader, M).unwrap().text, "one");
    assert_eq!(store_memo(&reader, K).unwrap().text, "fresh", "the rest of the segment applied");
    assert_eq!(count(&reader, "SELECT COUNT(*) FROM op_dots WHERE device_id = 'a' AND seq = 2"), 0);
}

// ------------------------------------------------------- cross-vault isolation

#[test]
fn g3_r21_cross_vault_isolation() {
    let mut reader = Store::open_in_memory().unwrap();
    reader.init_identity(&identity("r")).unwrap();
    reader.init_identity(&DeviceIdentity { vault_id: V2.to_owned(), ..identity("r") }).unwrap();
    reader.set_active_vault(VAULT).unwrap();

    let mut p1 = device("a");
    local(&mut p1, create(1, 1_000, M, "vault one"));
    let s1 = seal_next(&mut p1).unwrap();
    let publisher = |device_id: &str, n: u64, text: &str| {
        let mut p = Store::open_in_memory().unwrap();
        p.init_identity(&DeviceIdentity { vault_id: V2.to_owned(), ..identity(device_id) }).unwrap();
        p.set_active_vault(V2).unwrap();
        p.insert_local_op(V2, create(n, 2_000, M, text), UploadState::Never).unwrap();
        let k2 = key2();
        let pending = p
            .seal_pending(V2, |s, plain| {
                seal(&k2, &ObjectAddress::segment(V2, EPOCH, &s.device_id, s.first_seq, s.last_seq), plain)
            })
            .unwrap()
            .unwrap();
        pending.bytes
    };
    // Same device id and the same op id in both vaults, different content.
    let s2 = publisher("a", 1, "vault two");
    let a2_11 = ObjectAddress::segment(V2, EPOCH, "a", 1, 1);

    let out = reader.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &s1.bytes).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { conflicts: 0, .. }), "{out:?}");
    let v1 = dump_vault(&reader, VAULT);
    let out = reader.apply_sealed(V2, &key2(), &a2_11, &s2).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { conflicts: 0, .. }), "same op id in another vault is no conflict: {out:?}");
    assert_eq!(dump_vault(&reader, VAULT), v1, "vault 2 writes touch no vault 1 row (nor entities)");
    assert_ne!(reader.op_state(VAULT, &oid(1)).unwrap().unwrap().0, reader.op_state(V2, &oid(1)).unwrap().unwrap().0);

    // A vault-2 object offered to vault 1 is quarantined in vault 1 only.
    let v2 = dump_vault(&reader, V2);
    let out = reader.apply_sealed(VAULT, &key2(), &a2_11, &s2).unwrap();
    assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "{out:?}");
    assert_eq!(dump_vault(&reader, V2), v2);
    assert!(reader.quarantine_rows(V2).unwrap().is_empty());
    // The vault-1 key on a vault-2 object fails authentication.
    let out = reader.apply_sealed(V2, &seg_key(), &a2_11, &s2).unwrap();
    assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "{out:?}");

    // A fork in vault 2 never reaches vault 1.
    let v1 = dump_vault(&reader, VAULT);
    let variant = publisher("a", 2, "vault two, other variant");
    let segs: Vec<Segment> = [&s2, &variant].iter().map(|b| open_segment(&key2(), &a2_11, b).unwrap()).collect();
    let out = reader.apply_fork_variant(V2, "a", 1, 1, &segs).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { fork_started: true, .. }), "{out:?}");
    assert_eq!(reader.forks(V2).unwrap(), BTreeMap::from([("a".to_owned(), 1)]));
    assert!(reader.forks(VAULT).unwrap().is_empty());
    // Another vault-2 stream does not enter vault 1's frontier or new ops' ctx.
    let z = publisher("z", 3, "z");
    reader.apply_sealed(V2, &key2(), &ObjectAddress::segment(V2, EPOCH, "z", 1, 1), &z).unwrap();
    assert_eq!(dump_vault(&reader, VAULT), v1);
    let mine = reader.insert_local_op(VAULT, edit(4, 3_000, M, 1, "r edits"), UploadState::Never).unwrap();
    assert_eq!(mine.body.ctx, ctx_of(&[("a", 1)]));
    assert!(reader.frontier(VAULT).unwrap().keys().all(|d| d != "z"));
    // Rows of one vault carry only that vault's op ids.
    let v2_ops = count(&reader, &format!("SELECT COUNT(*) FROM ops WHERE vault_id = '{V2}' AND op_id = '{}'", oid(4)));
    assert_eq!(v2_ops, 0);
}

// ------------------------------------------------------- SQL-ish ids, unicode

#[test]
fn g3_r22_sql_and_unicode_ids_are_refused_and_text_round_trips() {
    for bad in ["x'); DROP TABLE ops; --", "메모", "a b", "../x", "\"q\"", "", "-lead", &"x".repeat(65), "a\u{0}b"] {
        let mut b = mc(1, "a", T0, &[], M, "t");
        b.entity_id = bad.to_owned();
        assert!(b.validate().is_err(), "entity id {bad:?}");
        let mut b = mc(1, "a", T0, &[], M, "t");
        b.origin_device = bad.to_owned();
        assert!(b.validate().is_err(), "origin {bad:?}");
        let mut b = mc(1, "a", T0, &[], M, "t");
        b.ctx = ctx_of(&[(bad, 1)]);
        assert!(b.validate().is_err(), "ctx device {bad:?}");
    }
    // A remote op carrying an injection-looking id holds the stream; no table is touched.
    let w = store_world();
    let mut reader = w.reader;
    let tables = table_names(&reader);
    let mut seg = w.seg2_plain.clone();
    seg.ops[0].body.entity_id = "x'); DROP TABLE ops; --".to_owned();
    let out = reader.apply_sealed(VAULT, &seg_key(), &addr("a", 2, 2), &sealed(&seg)).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
    assert_eq!(table_names(&reader), tables);
    // A device id with a quote in the address is quarantined, not interpolated.
    let quoted = ObjectAddress::segment(VAULT, EPOCH, "a'b", 1, 1);
    let out = catch_unwind(AssertUnwindSafe(|| reader.apply_sealed(VAULT, &seg_key(), &quoted, &w.seg2)));
    assert!(matches!(out, Ok(Ok(ApplyOutcome::Quarantined { .. }))), "{out:?}");
    assert_eq!(table_names(&reader), tables);

    // Hostile text content round trips through segments byte for byte.
    let text = "'); DROP TABLE ops; -- \u{0} \u{2028}\u{2029} 한국어 🦀 \"q\" \\ \u{202e}rtl \u{feff} \u{7f}";
    let mut remote = Remote::new(true);
    let (mut a, mut b) = (device("a"), device("b"));
    local(&mut a, create(1, 1_000, M, text));
    local(&mut a, op(2, 2_000, Kind::Memo, M, "note", json!({"note": text})));
    publish(&mut a, &mut remote).unwrap();
    sync_stream(&mut b, &remote, "a", &all);
    assert_eq!(hash(&a), hash(&b));
    let m = store_memo(&b, M).unwrap();
    assert_eq!((m.text.as_str(), m.note.as_str()), (text, text));
    // Non-UUID memo ids get deterministic, distinct UUIDv5 copy ids.
    let c1 = conflict_copy_id("memo.1_x", &oid(1));
    assert_eq!(c1, conflict_copy_id("memo.1_x", &oid(1)));
    assert_ne!(c1, conflict_copy_id("memo.1_x", &oid(2)));
    assert_ne!(c1, conflict_copy_id("memo.1_y", &oid(1)));
    assert_eq!(uuid::Uuid::parse_str(&c1).unwrap().get_version_num(), 5);
}

// --------------------------------------------------------------- large seqs

#[test]
fn g3_r23_large_seqs_and_counters_do_not_overflow() {
    let x = mc(1, "a", T0, &[], M, "x");
    let y = mc(2, "a", T0, &[], "memo-y", "y");
    let mut set = OpSet::new();
    assert!(!set.insert(x.clone(), dot("a", u64::MAX)).unwrap().fork_started);
    // Observation: `u64::MAX` is also OpSet's "no fork yet" sentinel, so a
    // collision exactly at u64::MAX records the right fork_start but reports
    // `fork_started = false`. Unreachable from the store (SQLite seqs are i64).
    set.insert(y, dot("a", u64::MAX)).unwrap();
    assert_eq!(set.forks()["a"], u64::MAX);
    let mut below = OpSet::new();
    below.insert(x.clone(), dot("a", u64::MAX - 1)).unwrap();
    assert!(below.insert(mc(2, "a", T0, &[], "memo-y", "y"), dot("a", u64::MAX - 1)).unwrap().fork_started);
    assert_eq!(below.forks()["a"], u64::MAX - 1);
    // Merge level only (the store rejects ctx values past i64::MAX, p2_i2):
    // the fixture validates, so the huge ctx is set after building it.
    let mut del = md(3, "b", T0, &[], M);
    del.ctx = ctx_of(&[("a", u64::MAX)]);
    assert!(!saw(&del, set.forks(), &x.op_id, &[dot("a", u64::MAX)]), "fork range needs obs even at u64::MAX");
    assert!(saw(&del, &Forks::new(), &x.op_id, &[dot("a", u64::MAX)]));
    assert_eq!(eff_ctx(&ctx_of(&[("a", u64::MAX)]), &Forks::from([("a".to_owned(), 0)])), ctx_of(&[("a", 0)]));
    set.insert(del, dot("b", u64::MAX)).unwrap();
    let p = project(&set, &Forks::from([("b".to_owned(), 1)])).unwrap();
    assert!(is_restored(&p, Kind::Memo, M));

    // Plaintext range checks near (but below) u64::MAX; u64::MAX itself is r23c.
    let big = u64::MAX - 1;
    let seg = seg_of("a", big, genesis(VAULT, "a"), vec![SegOp { seq: big, body: x.clone() }]);
    assert_eq!(check_plaintext(&seg, "a", big, big), Ok(()));
    assert!(check_plaintext(&seg, "a", 1, big).is_err());
    assert!(check_plaintext(&seg, "a", 0, 0).is_err());
    // A store offered a far-future range holds on the gap without overflow.
    let mut r = device("r");
    let mut body = x;
    body.ctx = ctx_of(&[("a", u64::MAX - 1), ("b", u64::MAX)]);
    let seg = seg_of("a", big, genesis(VAULT, "a"), vec![SegOp { seq: big, body: body.clone() }]);
    let out = r.apply_segment(VAULT, "a", big, big, &seg).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", big, big), &sealed(&seg)).unwrap();
    assert_eq!(out, ApplyOutcome::Skipped(CursorState::Held));
    // Contract change (leader decision p2_i2): ctx values past i64::MAX are
    // rejected by validation like every other integer, so the segment is
    // quarantined + held and nothing of it is stored.
    let mut r = device("r");
    body.hlc.ctr = u32::MAX;
    let seg = seg_of("a", 1, genesis(VAULT, "a"), vec![SegOp { seq: 1, body: body.clone() }]);
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &sealed(&seg)).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
    assert_eq!(r.cursor(VAULT, "a").unwrap().map(|c| c.state), Some(CursorState::Held));
    assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1);
    assert!(!op_ids(&r).contains(&oid(1)));
    // The largest storable ctx values and a max counter are stored and read
    // back verbatim.
    let mut r = device("r");
    body.ctx = ctx_of(&[("a", i64::MAX as u64 - 1), ("b", i64::MAX as u64)]);
    let seg = seg_of("a", 1, genesis(VAULT, "a"), vec![SegOp { seq: 1, body: body.clone() }]);
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &sealed(&seg)).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
    let stored = r.load_set(VAULT).unwrap().0.get(&oid(1)).unwrap().body.clone();
    assert_eq!((stored.ctx, stored.hlc), (body.ctx, body.hlc));
}

#[test]
fn g3_r23b_hlc_beyond_sqlite_integer_range_is_held_not_an_error() {
    let mut r = device("r");
    let mut body = mc(1, "a", T0, &[], M, "x");
    body.hlc.wall_ms = u64::MAX;
    // Contract change (shared decision r23b): validate() now rejects it as a
    // typed range error; the outcome below (quarantine + held) is unchanged.
    assert!(matches!(body.validate(), Err(OpError::Range(_))), "validate rejects an unstorable wall_ms");
    let seg = seg_of("a", 1, genesis(VAULT, "a"), vec![SegOp { seq: 1, body }]);
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &sealed(&seg));
    assert!(
        matches!(out, Ok(ApplyOutcome::Held { .. } | ApplyOutcome::Quarantined { .. })),
        "a storage-range failure must roll back into quarantine + held: {out:?}"
    );
    assert_eq!(r.cursor(VAULT, "a").unwrap().map(|c| c.state), Some(CursorState::Held));
}

#[test]
fn g3_r23c_segment_ending_at_u64_max_is_quarantined_not_a_panic() {
    let mut body = mc(1, "a", T0, &[], M, "x");
    body.ctx = BTreeMap::new();
    let seg = seg_of("a", u64::MAX, genesis(VAULT, "a"), vec![SegOp { seq: u64::MAX, body }]);
    let direct = catch_unwind(|| check_plaintext(&seg, "a", u64::MAX, u64::MAX));
    let mut r = device("r");
    let bytes = sealed(&seg);
    let via_store = catch_unwind(AssertUnwindSafe(|| r.apply_sealed(VAULT, &seg_key(), &addr("a", u64::MAX, u64::MAX), &bytes)));
    assert!(direct.is_ok(), "check_plaintext panicked on last_seq = u64::MAX");
    assert!(
        matches!(via_store, Ok(Ok(ApplyOutcome::Held { .. } | ApplyOutcome::Quarantined { .. }))),
        "apply_sealed must hold or quarantine, got {:?}",
        via_store.map_err(|_| "panic")
    );
}

// ---------------------------------------------------------------- rotation

/// AC 17 world: P published at (a,1), X rejected at (a,2), U = edit of X at (a,3).
fn rotation_world() -> (Store, BTreeMap<u64, Vec<[u8; 32]>>) {
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

fn plan_for(store: &Store, remote: &BTreeMap<u64, Vec<[u8; 32]>>, new: &str) -> super::rotation::RotationPlan {
    let source = store.rotation_input(VAULT, remote.clone()).unwrap();
    Store::plan_rotation(&source, new, || oid(99)).unwrap()
}

#[test]
fn g3_r24_apply_rotation_failing_at_each_step_rolls_back_everything() {
    let steps = [
        ("R5 old dot delete", "BEFORE DELETE ON op_dots"),
        ("R6 reissue insert", "BEFORE INSERT ON ops"),
        ("R5 restamp update", "BEFORE UPDATE OF ctx_frontier ON ops"),
        ("R5 new dot", "BEFORE INSERT ON op_dots"),
        ("R6 quarantine", "BEFORE INSERT ON quarantine"),
        ("R7 retired_devices", "BEFORE INSERT ON retired_devices"),
        ("R7 cursor", "BEFORE INSERT ON cursors"),
        ("vault_identity", "BEFORE UPDATE ON vault_identity"),
        ("sync_stats", "BEFORE INSERT ON sync_stats"),
        ("reproject lineage", "BEFORE INSERT ON memo_lineage"),
        ("reproject entities", "BEFORE INSERT ON entities"),
    ];
    let (mut a, remote) = rotation_world();
    let before = dump_all(&a, &[]);
    let plan = plan_for(&a, &remote, "a2");
    for (step, when) in steps {
        a.conn()
            .execute_batch(&format!("CREATE TRIGGER g3_fail {when} BEGIN SELECT RAISE(ABORT, 'g3 crash'); END;"))
            .unwrap();
        let out = a.apply_rotation(VAULT, &plan, &remote, &identity("a2"));
        assert!(out.is_err(), "{step}: the injected failure must surface");
        a.conn().execute_batch("DROP TRIGGER g3_fail;").unwrap();
        assert_eq!(dump_all(&a, &[]), before, "{step}: nothing of the failed rotation is applied");
    }
    a.apply_rotation_then_crash(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert_eq!(dump_all(&a, &[]), before, "drop before COMMIT");
    // The same plan then applies and matches a device that never failed.
    a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).unwrap();
    let (mut clean, remote2) = rotation_world();
    let plan2 = plan_for(&clean, &remote2, "a2");
    clean.apply_rotation(VAULT, &plan2, &remote2, &identity("a2")).unwrap();
    assert_eq!(a.dump_state().unwrap(), clean.dump_state().unwrap());
    assert_eq!(a.quarantine_rows(VAULT).unwrap(), clean.quarantine_rows(VAULT).unwrap());
    // A replayed plan is refused and changes nothing.
    let after = dump_all(&a, &[]);
    assert!(a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).is_err());
    assert_eq!(dump_all(&a, &[]), after);
}

#[test]
fn g3_r25_i1_holds_after_rotation_and_a_second_rotation() {
    let (mut a, remote) = rotation_world();
    let plan = plan_for(&a, &remote, "a2");
    a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).unwrap();
    for n in 40..43 {
        local(&mut a, edit(n, 4_000 + n, K, 3, &format!("after {n}")));
    }
    let check = |s: &Store, own: &str| {
        assert_one_dot_each(s, &["a", "a2", "a3"]);
        let seqs: Vec<u64> = {
            let mut stmt = s.conn().prepare("SELECT seq FROM op_dots WHERE vault_id = ?1 AND device_id = ?2 ORDER BY seq").unwrap();
            stmt.query_map([VAULT, own], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>(), "{own}: own seqs are 1..n without gaps");
        let origin_ops = count(s, &format!("SELECT COUNT(*) FROM ops WHERE origin_device = '{own}'"));
        assert_eq!(origin_ops as usize, seqs.len(), "{own}: every own op has exactly one dot");
    };
    check(&a, "a2");
    assert_eq!(a.dots_of(VAULT, &oid(1)).unwrap(), vec![dot("a", 1)], "published P keeps its dot");
    let new_op = a.load_set(VAULT).unwrap().0.get(&oid(42)).unwrap().body.clone();
    assert_eq!(new_op.ctx.get("a"), Some(&1), "post-rotation ops see S on the retired id");

    // Rotate again with nothing published under a2: every a2 op moves to a3.
    let plan = plan_for(&a, &BTreeMap::new(), "a3");
    a.apply_rotation(VAULT, &plan, &BTreeMap::new(), &identity("a3")).unwrap();
    check(&a, "a3");
    assert_eq!(count(&a, "SELECT COUNT(*) FROM op_dots WHERE device_id = 'a2'"), 0);
    assert_eq!(a.cursor(VAULT, "a2").unwrap().unwrap().state, CursorState::Retired);
}

// ------------------------------------------------------------- compaction

/// A reader holding x at (a,1) and its restamped alias at (a2,1) (AC 16c).
fn alias_reader() -> Store {
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "x"));
    let p = seal_next(&mut a).unwrap();
    let seg_a = open_segment(&seg_key(), &addr("a", 1, 1), &p.bytes).unwrap();
    let mut x2 = seg_a.ops[0].body.clone();
    x2.origin_device = "a2".to_owned();
    x2.ctx = ctx_of(&[("q", 3)]);
    let seg_a2 = seg_of("a2", 1, genesis(VAULT, "a2"), vec![SegOp { seq: 1, body: x2 }]);
    let mut r = device("r");
    r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &p.bytes).unwrap();
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a2", 1, 1), &sealed(&seg_a2)).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { conflicts: 0, .. }), "{out:?}");
    assert_eq!(r.dots_of(VAULT, &oid(1)).unwrap(), vec![dot("a", 1), dot("a2", 1)]);
    r
}

fn redeleted_fork_world() -> ForkWorld {
    let mut w = fork_world();
    local(&mut w.b, delete(5, 400, M));
    let copy = w.copy_id.clone();
    local(&mut w.b, delete(6, 410, &copy));
    publish(&mut w.b, &mut w.remote).unwrap();
    w
}

#[test]
fn g3_r26_snapshot_with_forks_obs_and_aliases_restores_the_same_hash() {
    let w = redeleted_fork_world();
    let snap = build_snapshot(&w.b, VAULT).unwrap();
    assert_eq!(snap.forks, BTreeMap::from([("a".to_owned(), 1)]));
    assert!(snap.ops.iter().any(|o| o.body.obs == vec![oid(2), oid(3)]), "obs preserved");
    assert_eq!(snap.ops.iter().filter(|o| o.dots.contains(&dot("a", 1))).count(), 2, "both fork variants kept");
    let decoded = Snapshot::decode(&snap.encode().unwrap()).unwrap();
    assert_eq!(decoded, snap);
    assert_eq!(restore_equivalent(&decoded).unwrap().hash, hash(&w.b));
    assert!(verify_snapshot(&w.b, VAULT, &snap).unwrap());
    assert!(is_deleted(&restore_equivalent(&snap).unwrap(), Kind::Memo, M), "tombstone survives compaction");

    let r = alias_reader();
    let snap = build_snapshot(&r, VAULT).unwrap();
    assert_eq!(snap.ops[0].dots, vec![dot("a", 1), dot("a2", 1)], "alias dots preserved");
    assert_eq!(snap.ops[0].body.ctx, ctx_of(&[("q", 3)]), "alias-unioned ctx preserved");
    assert!(verify_snapshot(&r, VAULT, &snap).unwrap());
    // Restoring in any op/dot order gives the same hash.
    let mut shuffled = snap.clone();
    shuffled.ops.reverse();
    for o in &mut shuffled.ops {
        o.dots.reverse();
    }
    assert_eq!(restore_equivalent(&shuffled).unwrap().hash, hash(&r));
}

type Tamper = fn(&mut Snapshot);

fn op_index(s: &Snapshot, n: u64) -> usize {
    s.ops.iter().position(|o| o.body.op_id == oid(n)).unwrap()
}

/// Tampers that change what the snapshot projects to must fail verify, and a
/// failed verify offers nothing for deletion.
#[test]
fn g3_r27_tampered_snapshot_fails_verify_and_deletes_nothing() {
    let live = fork_world();
    let gone = redeleted_fork_world();
    let cases: [(&str, &Store, Tamper); 6] = [
        ("head text", &live.b, |s| {
            let i = op_index(s, 3);
            s.ops[i].body.payload = json!({"text": "tampered", "base_op_id": oid(1)});
        }),
        ("drop a fork variant", &live.b, |s| {
            let i = op_index(s, 2);
            s.ops.remove(i);
        }),
        ("other vault", &live.b, |s| s.vault_id = V2.to_owned()),
        ("float payload", &live.b, |s| {
            let i = op_index(s, 3);
            s.ops[i].body.payload = json!({"text": 1.5, "base_op_id": oid(1)});
        }),
        ("drop obs", &gone.b, |s| s.ops.iter_mut().for_each(|o| o.body.obs.clear())),
        ("drop the re-delete", &gone.b, |s| s.ops.retain(|o| o.body.op_id != oid(5))),
    ];
    for (name, store, tamper) in cases {
        let snap = build_snapshot(store, VAULT).unwrap();
        let ok = verify_snapshot(store, VAULT, &snap).unwrap();
        assert!(ok, "{name}: the genuine snapshot verifies");
        assert!(!segments_deletable(store, VAULT, "b", &[u64::MAX], ok, &snap).unwrap().is_empty(), "{name}: sanity");
        let mut t = snap.clone();
        tamper(&mut t);
        // An encode error (float) is also "not verified".
        let ok = verify_snapshot(store, VAULT, &t).unwrap_or(false);
        assert!(!ok, "{name}: verify must fail");
        assert!(segments_deletable(store, VAULT, "b", &[u64::MAX], ok, &t).unwrap().is_empty(), "{name}: nothing deleted");
    }
    // Tampered bytes never decode.
    let mut bytes = build_snapshot(&live.b, VAULT).unwrap().encode().unwrap();
    bytes.insert(1, b' ');
    assert!(Snapshot::decode(&bytes).is_err());
}

/// AC 8 "compaction equivalence (obs, forks preserved)": a snapshot that lost
/// ops, dots, or payloads is not equivalent even when today's projection hash
/// happens to match, and must not verify.
#[test]
fn g3_r27b_lossy_snapshots_fail_verify() {
    let alias = alias_reader();
    let live = fork_world();
    let gone = redeleted_fork_world();
    let cases: [(&str, &Store, Tamper); 4] = [
        ("drop an alias dot", &alias, |s| s.ops[0].dots.retain(|d| d.device != "a2")),
        ("drop a fork variant of a tombstone", &gone.b, |s| {
            let i = op_index(s, 3);
            s.ops.remove(i);
        }),
        ("drop the blind delete", &live.b, |s| s.ops.retain(|o| o.body.op_id != oid(4))),
        ("rewrite a tombstoned payload", &gone.b, |s| {
            let i = op_index(s, 1);
            s.ops[i].body.payload = json!({"text": "rewritten", "pinned": true});
        }),
    ];
    // Fixture: the alias-lossy restore really diverges on a later delete.
    let del = md(7, "c", T0, &[("a2", 1)], M);
    let replay = |s: &Snapshot| {
        let mut set = OpSet::new();
        for o in &s.ops {
            for d in &o.dots {
                set.insert(o.body.clone(), d.clone()).unwrap();
            }
        }
        set.insert(del.clone(), dot("c", 1)).unwrap();
        project(&set, &s.forks).unwrap().hash
    };
    let genuine = build_snapshot(&alias, VAULT).unwrap();
    let mut lossy = genuine.clone();
    (cases[0].2)(&mut lossy);
    assert_ne!(replay(&genuine), replay(&lossy), "fixture: not equivalent");

    let mut accepted = Vec::new();
    for (name, store, tamper) in cases {
        let mut t = build_snapshot(store, VAULT).unwrap();
        tamper(&mut t);
        if verify_snapshot(store, VAULT, &t).unwrap() {
            let offered = segments_deletable(store, VAULT, &store.device_id(VAULT).unwrap(), &[u64::MAX], true, &t).unwrap();
            accepted.push(format!("{name} (offers {offered:?})"));
        }
    }
    assert!(accepted.is_empty(), "lossy snapshots verified: {accepted:?}");
}

#[test]
fn g3_r28_retired_and_forked_streams_are_never_offered_for_deletion() {
    let (mut a, remote) = rotation_world();
    let plan = plan_for(&a, &remote, "a2");
    a.apply_rotation(VAULT, &plan, &remote, &identity("a2")).unwrap();
    assert!(!a.pending_segments(VAULT, "a").unwrap().is_empty(), "the retired id still has sealed segments");
    let sealed_a2 = seal_next(&mut a).unwrap();
    // Contract update (architect B2): only fully published ranges are offered.
    a.mark_attempted(VAULT, "a2", sealed_a2.first_seq).unwrap();
    a.mark_published(VAULT, "a2", sealed_a2.first_seq).unwrap();
    let snap = build_snapshot(&a, VAULT).unwrap();
    assert!(verify_snapshot(&a, VAULT, &snap).unwrap());
    assert!(segments_deletable(&a, VAULT, "a", &[u64::MAX], true, &snap).unwrap().is_empty(), "retired stream");
    let last = sealed_a2.last_seq;
    assert_eq!(segments_deletable(&a, VAULT, "a2", &[last], true, &snap).unwrap(), vec![(1, last)]);
    assert!(segments_deletable(&a, VAULT, "a2", &[last, last - 1], true, &snap).unwrap().is_empty(), "a reader behind");
    assert!(segments_deletable(&a, VAULT, "a2", &[last], false, &snap).unwrap().is_empty(), "unverified");
    let mut other = snap.clone();
    other.vault_id = V2.to_owned();
    assert!(segments_deletable(&a, VAULT, "a2", &[last], true, &other).unwrap().is_empty(), "another vault's snapshot");

    // Own stream forked (a Drive clone published the same name).
    let mut f = device("f");
    local(&mut f, create(1, 1_000, M, "mine"));
    let p = seal_next(&mut f).unwrap();
    f.mark_attempted(VAULT, "f", 1).unwrap();
    f.mark_published(VAULT, "f", 1).unwrap();
    let mine = open_segment(&seg_key(), &addr("f", 1, 1), &p.bytes).unwrap();
    let snap = build_snapshot(&f, VAULT).unwrap();
    assert_eq!(segments_deletable(&f, VAULT, "f", &[1], true, &snap).unwrap(), vec![(1, 1)], "sanity before the fork");
    let mut theirs = op(2, 1_001, Kind::Memo, K, "create", json!({"text": "clone", "pinned": false}));
    theirs.origin_device = "f".to_owned();
    let clone_seg = seg_of("f", 1, genesis(VAULT, "f"), vec![SegOp { seq: 1, body: theirs }]);
    let out = f.apply_fork_variant(VAULT, "f", 1, 1, &[mine, clone_seg]).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { fork_started: true, .. }), "{out:?}");
    let snap = build_snapshot(&f, VAULT).unwrap();
    assert!(verify_snapshot(&f, VAULT, &snap).unwrap());
    assert!(segments_deletable(&f, VAULT, "f", &[u64::MAX], true, &snap).unwrap().is_empty(), "forked own stream");
}

#[test]
fn g3_r29_checkpoints_keep_the_newest_three() {
    let dir = TempDir::new("g3-ckpt");
    let mut s = device("a");
    local(&mut s, create(1, 1_000, M, "x"));
    let mut paths = Vec::new();
    for n in 0..5u64 {
        local(&mut s, edit(10 + n, 2_000 + n, M, if n == 0 { 1 } else { 9 + n }, &format!("v{n}")));
        paths.push(checkpoint(&mut s, &dir.0).unwrap());
    }
    let rows: Vec<String> = {
        let mut stmt = s.conn().prepare("SELECT path FROM checkpoints ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    let kept: Vec<String> = paths[2..].iter().map(|p| p.to_string_lossy().into_owned()).collect();
    assert_eq!(rows, kept);
    assert_eq!(rows.len(), KEEP_CHECKPOINTS);
    for p in &paths[..2] {
        assert!(!p.exists(), "{p:?} removed");
    }
    for (i, p) in paths[2..].iter().enumerate() {
        let copy = Store::open(p).unwrap();
        assert_eq!(store_memo(&copy, M).unwrap().text, format!("v{}", i + 2), "checkpoint {i} is a full copy");
    }
}

// ------------------------------------------------- malformed remote input

/// The canonical plaintext of a valid 1-1 segment of stream a, as a value.
fn template() -> Value {
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "ok"));
    let pending = seal_next(&mut a).unwrap();
    let plain = open_segment(&seg_key(), &addr("a", 1, 1), &pending.bytes).unwrap().encode().unwrap();
    serde_json::from_slice(&plain).unwrap()
}

fn canon(value: &Value) -> Vec<u8> {
    canonical_json(value).unwrap()
}

fn with(t: &Value, f: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut v = t.clone();
    f(&mut v);
    canon(&v)
}

fn with_body(t: &Value, f: impl FnOnce(&mut Value)) -> Vec<u8> {
    with(t, |v| f(&mut v["ops"][0]["body"]))
}

/// Canonical bytes with the marker counter replaced by raw number text.
fn with_raw_number(t: &Value, raw: &str) -> Vec<u8> {
    let marked = with_body(t, |b| b["hlc"]["ctr"] = json!(777_777));
    String::from_utf8(marked).unwrap().replace("777777", raw).into_bytes()
}

fn conv_body(payload: Value) -> impl FnOnce(&mut Value) {
    move |b: &mut Value| {
        b["kind"] = json!("conv");
        b["type"] = json!("msg.add");
        b["payload"] = payload;
    }
}

#[test]
fn g3_r30_malformed_remote_payloads_quarantine_or_hold_never_panic() {
    let t = template();
    let nested = format!("{}1{}", "[".repeat(300), "]".repeat(300));
    let msg_ok = json!({"conv_id": M, "msg_id": "m1", "role": "user", "content": "x", "attachments": [], "ts": 1});
    let mut msg_bad_attach = msg_ok.clone();
    msg_bad_attach["attachments"] = json!({});
    let mut msg_bad_ts = msg_ok.clone();
    msg_bad_ts["ts"] = json!(-1);
    let mut msg_no_role = msg_ok.clone();
    msg_no_role.as_object_mut().unwrap().remove("role");
    let mut msg_wrong_conv = msg_ok.clone();
    msg_wrong_conv["conv_id"] = json!("other");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("text is a number", with_body(&t, |b| b["payload"]["text"] = json!(5))),
        ("text is null", with_body(&t, |b| b["payload"]["text"] = Value::Null)),
        ("missing pinned", with_body(&t, |b| b["payload"] = json!({"text": "x"}))),
        ("pinned is a string", with_body(&t, |b| b["payload"]["pinned"] = json!("true"))),
        ("unknown payload field", with_body(&t, |b| b["payload"]["evil"] = json!(1))),
        ("payload is an array", with_body(&t, |b| b["payload"] = json!([1, 2]))),
        ("unknown body field", with_body(&t, |b| b["evil"] = json!(1))),
        ("missing op_id", with_body(&t, |b| { b.as_object_mut().unwrap().remove("op_id"); })),
        ("op_id is a v4 uuid", with_body(&t, |b| b["op_id"] = json!(uuid::Uuid::new_v4().to_string()))),
        ("op_id uppercase", with_body(&t, |b| b["op_id"] = json!(oid(1).to_uppercase()))),
        ("unknown kind", with_body(&t, |b| b["kind"] = json!("spaceship"))),
        ("unknown type", with_body(&t, |b| b["type"] = json!("explode"))),
        ("hlc wall is a string", with_body(&t, |b| b["hlc"]["wall_ms"] = json!("now"))),
        ("hlc missing ctr", with_body(&t, |b| b["hlc"] = json!({"wall_ms": 1}))),
        ("ctx value negative", with_body(&t, |b| b["ctx"] = json!({"b": -1}))),
        ("ctx is an array", with_body(&t, |b| b["ctx"] = json!([1]))),
        ("obs unsorted", with_body(&t, |b| b["obs"] = json!([oid(9), oid(8)]))),
        ("obs duplicate", with_body(&t, |b| b["obs"] = json!([oid(8), oid(8)]))),
        ("obs not op ids", with_body(&t, |b| b["obs"] = json!(["x"]))),
        ("body v 0", with_body(&t, |b| b["v"] = json!(0))),
        ("body is null", with(&t, |v| v["ops"][0]["body"] = Value::Null)),
        ("ops is null", with(&t, |v| v["ops"] = Value::Null)),
        ("ops empty", with(&t, |v| v["ops"] = json!([]))),
        ("seq negative", with(&t, |v| v["ops"][0]["seq"] = json!(-1))),
        ("seq is a string", with(&t, |v| v["ops"][0]["seq"] = json!("1"))),
        ("first_seq 0", with(&t, |v| v["first_seq"] = json!(0))),
        ("prev hash uppercase", with(&t, |v| {
            let up = v["prev_seg_hash"].as_str().unwrap().to_uppercase();
            v["prev_seg_hash"] = json!(up);
        })),
        ("prev hash short", with(&t, |v| v["prev_seg_hash"] = json!("ab"))),
        ("segment is an array", canon(&json!([1, 2, 3]))),
        ("segment is a string", canon(&json!("seg"))),
        ("empty plaintext", Vec::new()),
        ("not utf-8", vec![0xff, 0xfe, 0x00, 0x7b]),
        ("float counter", with_raw_number(&t, "1.5")),
        ("exponent counter", with_raw_number(&t, "1e3")),
        ("counter above u32", with_raw_number(&t, "4294967296")),
        ("integer above u64", with_raw_number(&t, "18446744073709551616")),
        ("negative zero", with_raw_number(&t, "-0")),
        ("deep nesting", with_raw_number(&t, &nested)),
        ("msg.add attachments object", with_body(&t, conv_body(msg_bad_attach))),
        ("msg.add negative ts", with_body(&t, conv_body(msg_bad_ts))),
        ("msg.add missing role", with_body(&t, conv_body(msg_no_role))),
        ("msg.add conv_id mismatch", with_body(&t, conv_body(msg_wrong_conv))),
        ("set_active leaf null", with_body(&t, |b| {
            b["kind"] = json!("conv");
            b["type"] = json!("set_active");
            b["payload"] = json!({"leaf": null});
        })),
        ("tr.add pinned string", with_body(&t, |b| {
            b["kind"] = json!("tr");
            b["type"] = json!("tr.add");
            b["payload"] = json!({"pinned": "yes"});
        })),
        ("policy payload array", with_body(&t, |b| {
            b["kind"] = json!("policy");
            b["type"] = json!("policy.retention");
            b["payload"] = json!([30]);
        })),
        ("edit base not an op id", with_body(&t, |b| {
            b["type"] = json!("edit");
            b["payload"] = json!({"text": "x", "base_op_id": "x'; --"});
        })),
        ("delete with payload", with_body(&t, |b| {
            b["type"] = json!("delete");
            b["payload"] = json!({"why": "x"});
        })),
        ("future segment v", with(&t, |v| v["v"] = json!(2))),
        ("future body v", with_body(&t, |b| b["v"] = json!(3))),
    ];
    let mut failures = Vec::new();
    for (name, plain) in &cases {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut r = device("r");
            let sealed = seal(&seg_key(), &addr("a", 1, 1), plain).unwrap();
            let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &sealed);
            let ops = count(&r, "SELECT COUNT(*) FROM ops");
            let entities = count(&r, "SELECT COUNT(*) FROM entities");
            (out, ops, entities)
        }));
        match outcome {
            Err(_) => failures.push(format!("{name}: panicked")),
            Ok((Ok(ApplyOutcome::Quarantined { .. } | ApplyOutcome::Held { .. } | ApplyOutcome::FutureVersion(_)), 0, 0)) => {}
            Ok(other) => failures.push(format!("{name}: {other:?}")),
        }
    }
    assert!(failures.is_empty(), "malformed input must quarantine or hold, never apply or panic:\n{}", failures.join("\n"));

    // A huge but well-formed string is accepted intact.
    let big = "가".repeat(1 << 20);
    let plain = with_body(&t, |b| b["payload"]["text"] = json!(big));
    let mut r = device("r");
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &seal(&seg_key(), &addr("a", 1, 1), &plain).unwrap()).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
    assert_eq!(store_memo(&r, M).unwrap().text.len(), big.len());
    // Huge ids are refused.
    let plain = with_body(&t, |b| b["entity_id"] = json!("x".repeat(100_000)));
    let mut r = device("r");
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &seal(&seg_key(), &addr("a", 1, 1), &plain).unwrap()).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
}

#[test]
fn g3_r31_projection_of_unvalidated_garbage_never_panics() {
    let raw = |n: u64, kind: Kind, entity: &str, op_type: &str, payload: Value| OpBody {
        v: 1,
        op_id: oid(n),
        origin_device: "a".to_owned(),
        hlc: Hlc { wall_ms: n, ctr: 0 },
        ctx: BTreeMap::new(),
        obs: Vec::new(),
        kind,
        entity_id: entity.to_owned(),
        op_type: op_type.to_owned(),
        payload,
    };
    let pubs: Vec<Pub> = vec![
        at(raw(1, Kind::Memo, M, "create", json!({"text": 5, "pinned": "x"})), 1),
        at(raw(2, Kind::Memo, M, "edit", json!({"text": [1], "base_op_id": 7})), 2),
        at(raw(3, Kind::Memo, M, "edit", json!({"text": "loop", "base_op_id": oid(3)})), 3),
        at(raw(4, Kind::Memo, M, "edit", json!({"text": "a", "base_op_id": oid(5)})), 4),
        at(raw(5, Kind::Memo, M, "edit", json!({"text": "b", "base_op_id": oid(4)})), 5),
        at(raw(6, Kind::Conv, CV, "msg.add", json!({"msg_id": 1, "attachments": "x", "ts": "y"})), 6),
        at(raw(7, Kind::Conv, CV, "set_active", json!({"leaf": 3})), 7),
        at(raw(8, Kind::Tr, TRX, "tr.add", json!("str")), 8),
        at(raw(9, Kind::Tr, TRX, "tr.pin", Value::Null), 9),
        at(raw(10, Kind::Policy, "p", "policy.retention", json!(null)), 10),
        at(raw(11, Kind::Memo, "memo-n", "note", json!({})), 11),
        at(raw(12, Kind::Memo, M, "teleport", json!({})), 12),
        at(raw(13, Kind::Conv, "c2", "msg.add", json!({"msg_id": "m", "parent_id": "m"})), 13),
    ];
    let p = order_free(&pubs, 200, 41);
    assert!(!p.entities.is_empty());
    // A float cannot even enter the set.
    let mut set = OpSet::new();
    assert!(set.insert(raw(14, Kind::Memo, M, "create", json!({"text": 0.5})), dot("a", 14)).is_err());
    assert!(set.is_empty());
}

// ------------------------------------------- arrival order at store level

#[test]
fn g3_r32_alias_arrival_order_gives_the_same_store_state() {
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "x"));
    let p = seal_next(&mut a).unwrap();
    let seg_a = open_segment(&seg_key(), &addr("a", 1, 1), &p.bytes).unwrap();
    let mut x2 = seg_a.ops[0].body.clone();
    x2.origin_device = "a2".to_owned();
    x2.ctx = ctx_of(&[("q", 3)]);
    let seg_a2 = sealed(&seg_of("a2", 1, genesis(VAULT, "a2"), vec![SegOp { seq: 1, body: x2 }]));
    let mut r1 = device("r");
    r1.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &p.bytes).unwrap();
    r1.apply_sealed(VAULT, &seg_key(), &addr("a2", 1, 1), &seg_a2).unwrap();
    let mut r2 = device("r");
    r2.apply_sealed(VAULT, &seg_key(), &addr("a2", 1, 1), &seg_a2).unwrap();
    r2.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &p.bytes).unwrap();
    assert_eq!(hash(&r1), hash(&r2));
    let cols = "vault_id, op_id, origin_device, hlc_wall, hlc_ctr, ctx_frontier, obs, entity_kind, entity_id, op_type, payload, content_hash, upload_state";
    let rows = |s: &Store| dump_rows(s, &format!("SELECT {cols} FROM ops"));
    assert_eq!(rows(&r1), rows(&r2), "alias union and origin choice are order-free");
    assert_eq!(dump_rows(&r1, "SELECT * FROM op_dots"), dump_rows(&r2, "SELECT * FROM op_dots"));
    assert_eq!(
        dump_rows(&r1, "SELECT vault_id, remote_device_id, applied_seq, chain_head, state FROM cursors"),
        dump_rows(&r2, "SELECT vault_id, remote_device_id, applied_seq, chain_head, state FROM cursors")
    );
}

#[test]
fn g3_r33_same_op_id_different_content_is_quarantined_on_every_reader() {
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "from a"));
    let p = seal_next(&mut a).unwrap();
    let seg_a = open_segment(&seg_key(), &addr("a", 1, 1), &p.bytes).unwrap();
    let mut forged = seg_a.ops[0].body.clone();
    forged.origin_device = "c".to_owned();
    forged.payload = json!({"text": "from c", "pinned": false});
    let seg_c = sealed(&seg_of("c", 1, genesis(VAULT, "c"), vec![SegOp { seq: 1, body: forged }]));
    let key = format!("op/{}", oid(1));
    for order in [["a", "c"], ["c", "a"]] {
        let mut r = device("r");
        let mut outcomes = Vec::new();
        for d in order {
            let bytes = if d == "a" { &p.bytes } else { &seg_c };
            outcomes.push(r.apply_sealed(VAULT, &seg_key(), &addr(d, 1, 1), bytes).unwrap());
        }
        assert!(matches!(outcomes[1], ApplyOutcome::Applied { conflicts: 1, .. }), "{order:?}: {outcomes:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().iter().any(|(k, _)| *k == key), "{order:?}");
        assert_eq!(r.dots_of(VAULT, &oid(1)).unwrap().len(), 1, "{order:?}: the latecomer's dot is skipped");
    }
}
// ======================================================================
// Pass 2: attacks on the pass-1 fixes (multi-head forks, orphan bases,
// pending_segments trim, future versions, i64 bounds, snapshot verify).
// ======================================================================

fn head(segment: &Segment) -> String {
    hex(&super::ops::segment_hash(segment).unwrap())
}

/// A one-op segment of `device` at `seq` continuing `prev`.
fn link(device: &str, seq: u64, prev: &str, body: OpBody) -> Segment {
    seg_of(device, seq, prev.to_owned(), vec![SegOp { seq, body }])
}

/// One engine step: a name listing one object, or a name listing several.
#[derive(Clone)]
enum Feed {
    One(Segment),
    Many(Vec<Segment>),
}

fn feed(r: &mut Store, step: &Feed) -> ApplyOutcome {
    match step {
        Feed::One(s) => {
            r.apply_sealed(VAULT, &seg_key(), &addr(&s.device_id, s.first_seq, s.last_seq), &sealed(s)).unwrap()
        }
        Feed::Many(vs) => r.apply_fork_variant(VAULT, &vs[0].device_id, vs[0].first_seq, vs[0].last_seq, vs).unwrap(),
    }
}

fn run(steps: &[Feed]) -> Store {
    let mut r = device("r");
    for step in steps {
        feed(&mut r, step);
    }
    r
}

fn op_ids(s: &Store) -> BTreeSet<String> {
    s.load_set(VAULT).unwrap().0.iter().map(|e| e.body.op_id.clone()).collect()
}

fn nodes(s: &Store, device: &str) -> i64 {
    count(s, &format!("SELECT COUNT(*) FROM chain_nodes WHERE vault_id = '{VAULT}' AND device_id = '{device}'"))
}

fn memo_texts(p: &Projection) -> BTreeSet<String> {
    p.entities
        .values()
        .filter_map(|e| match e {
            EntityState::Memo(m) => Some(m.text.clone()),
            _ => None,
        })
        .collect()
}

/// Stream a forked three ways at seq 1; every variant continues at seq 2;
/// the first chain continues at seq 3. b blindly deletes c1 (ctx a:2).
struct ThreeWay {
    v: Vec<Segment>,
    c: Vec<Segment>,
    d0: Segment,
    b1: Segment,
}

fn three_way() -> ThreeWay {
    let g = genesis(VAULT, "a");
    let v: Vec<Segment> =
        (0..3u64).map(|i| link("a", 1, &g, mc(10 + i, "a", T0 + i, &[], &format!("memo-v{i}"), &format!("v{i}")))).collect();
    let c: Vec<Segment> = (0..3u64)
        .map(|i| {
            let body = mc(20 + i, "a", T0 + 10 + i, &[("a", 1)], &format!("memo-c{i}"), &format!("c{i}"));
            link("a", 2, &head(&v[i as usize]), body)
        })
        .collect();
    let d0 = link("a", 3, &head(&c[0]), mc(30, "a", T0 + 20, &[("a", 2)], "memo-d0", "d0"));
    let b1 = link("b", 1, &genesis(VAULT, "b"), md(40, "b", T0 + 100, &[("a", 2)], "memo-c1"));
    ThreeWay { v, c, d0, b1 }
}

fn three_way_pubs(w: &ThreeWay) -> Vec<Pub> {
    let mut pubs = Vec::new();
    for s in w.v.iter().chain(&w.c).chain([&w.d0, &w.b1]) {
        let op = &s.ops[0];
        pubs.push((op.body.clone(), dot(&s.device_id, op.seq)));
    }
    pubs
}

fn r3_steps(w: &ThreeWay) -> Vec<Feed> {
    vec![Feed::Many(w.v.clone()), Feed::Many(w.c.clone()), Feed::One(w.d0.clone()), Feed::One(w.b1.clone())]
}

/// Leader decision: a forked cursor keeps every known head. Three variants,
/// each continued, read by four readers in four orders (single objects
/// first, the clone chain first, all at once, a lagging two-variant listing
/// after a chain-mismatch hold) converge on one projection.
#[test]
fn g3_p2_f1_three_variants_four_readers_converge() {
    let w = three_way();
    let (v, c) = (&w.v, &w.c);
    let one = |s: &Segment| Feed::One(s.clone());
    let many = |ss: &[&Segment]| Feed::Many(ss.iter().map(|s| (*s).clone()).collect());
    let orders: Vec<(&str, Vec<Feed>)> = vec![
        (
            "originals one by one first",
            vec![
                one(&v[0]),
                one(&c[0]),
                one(&w.d0),
                many(&[&v[0], &v[1], &v[2]]),
                many(&[&c[0], &c[1], &c[2]]),
                one(&w.d0),
                one(&w.b1),
            ],
        ),
        (
            "third chain first, b first",
            vec![
                one(&w.b1),
                one(&v[2]),
                one(&c[2]),
                many(&[&v[2], &v[1], &v[0]]),
                many(&[&c[2], &c[0], &c[1]]),
                one(&w.d0),
            ],
        ),
        ("all at once", r3_steps(&w)),
        (
            "chain mismatch hold, then a lagging two-variant listing",
            vec![
                one(&v[1]),
                one(&c[0]),
                one(&w.b1),
                many(&[&v[1], &v[0]]),
                many(&[&c[1], &c[0]]),
                one(&w.d0),
                many(&[&v[0], &v[1], &v[2]]),
                many(&[&c[0], &c[1], &c[2]]),
                one(&w.d0),
            ],
        ),
    ];
    let expected = project_all(&three_way_pubs(&w));
    assert!(is_restored(&expected, Kind::Memo, "memo-c1"), "fixture: the blind delete misses the fork range");
    for (name, steps) in &orders {
        let r = run(steps);
        assert_eq!(hash(&r), expected.hash, "{name}: converges on the full-set projection");
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]), "{name}");
        assert_eq!(op_ids(&r).len(), 8, "{name}: no op lost");
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked, "{name}");
        assert!(store_restored(&r, Kind::Memo, "memo-c1"), "{name}");
        assert_eq!(memo_texts(&r.projection(VAULT).unwrap()).len(), 7, "{name}: every variant's memo is shown");
    }
}

/// A fork first seen at seq 3 later turns out to start at seq 2 (a second
/// clone): fork_start moves down, the clone chain from seq 2 applies, and a
/// delete that saw (a,2) only under the old fork_start is re-judged.
#[test]
fn g3_p2_f2_fork_found_later_at_a_lower_seq_converges() {
    let g = genesis(VAULT, "a");
    let s1 = link("a", 1, &g, mc(1, "a", T0, &[], "memo-1", "one"));
    let s2 = link("a", 2, &head(&s1), mc(2, "a", T0 + 1, &[("a", 1)], "memo-r", "r"));
    let s3 = link("a", 3, &head(&s2), mc(3, "a", T0 + 2, &[("a", 2)], "memo-s3", "s3"));
    let s4 = link("a", 4, &head(&s3), mc(4, "a", T0 + 3, &[("a", 3)], "memo-s4", "s4"));
    let x3 = link("a", 3, &head(&s2), mc(5, "a", T0 + 4, &[("a", 2)], "memo-x3", "x3"));
    let y2 = link("a", 2, &head(&s1), mc(6, "a", T0 + 5, &[("a", 1)], "memo-y2", "y2"));
    let y3 = link("a", 3, &head(&y2), mc(7, "a", T0 + 6, &[("a", 2)], "memo-y3", "y3"));
    let b1 = link("b", 1, &genesis(VAULT, "b"), md(8, "b", T0 + 7, &[("a", 3)], "memo-r"));
    let one = |s: &Segment| Feed::One(s.clone());
    let many = |ss: &[&Segment]| Feed::Many(ss.iter().map(|s| (*s).clone()).collect());

    // P: fork at 3 first; the delete then counts as having seen r at (a,2).
    let mut p = run(&[one(&s1), one(&s2), many(&[&s3, &x3]), one(&s4), one(&b1)]);
    assert_eq!(p.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 3)]));
    assert!(is_deleted(&p.projection(VAULT).unwrap(), Kind::Memo, "memo-r"), "fixture: under fork_start 3 the delete saw r");
    for step in [many(&[&s2, &y2]), many(&[&s3, &x3, &y3])] {
        feed(&mut p, &step);
    }
    let readers = [
        ("lower fork found later", p),
        ("in seq order", run(&[one(&s1), many(&[&s2, &y2]), many(&[&s3, &x3, &y3]), one(&s4), one(&b1)])),
        ("b first, clone chain singly", run(&[one(&b1), one(&s1), many(&[&y2, &s2]), one(&y3), many(&[&x3, &s3, &y3]), one(&s4)])),
        ("clone chain alone first", run(&[one(&s1), one(&y2), one(&y3), one(&s2), many(&[&s2, &y2]), many(&[&s3, &x3, &y3]), one(&s4), one(&b1)])),
    ];
    let all_pubs: Vec<Pub> = [&s1, &s2, &s3, &s4, &x3, &y2, &y3, &b1]
        .iter()
        .map(|s| (s.ops[0].body.clone(), dot(&s.device_id, s.first_seq)))
        .collect();
    let expected = project_all(&all_pubs);
    for (name, r) in &readers {
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]), "{name}: fork_start moved down");
        assert_eq!(hash(r), expected.hash, "{name}");
        assert_eq!(op_ids(r).len(), 8, "{name}: no op lost");
        assert!(store_restored(r, Kind::Memo, "memo-r"), "{name}: re-judged under fork_start 2");
    }
}

/// A segment of a forked stream whose prev_seg_hash is no known head is
/// list lag (architect B1, leader decision): deferred with nothing written,
/// no quarantine, the cursor untouched and still forked, so a genuine
/// continuation listed later applies at once. Contract update: this test
/// asserted `Held` before, which skipped every later segment.
#[test]
fn g3_p2_f3_segment_extending_no_head_is_deferred() {
    let w = three_way();
    let mut r = run(&r3_steps(&w));
    let cursor = r.cursor(VAULT, "a").unwrap().unwrap();
    let before = dump_all(&r, &[]);
    let (q, n) = (r.quarantine_rows(VAULT).unwrap().len(), nodes(&r, "a"));
    let bogus = link("a", 4, &"ab".repeat(32), mc(50, "a", T0 + 30, &[("a", 3)], "memo-bogus", "bogus"));
    let out = feed(&mut r, &Feed::One(bogus.clone()));
    assert!(matches!(out, ApplyOutcome::Deferred { ref reason } if reason.contains("no known head")), "{out:?}");
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap(), cursor, "the cursor stays forked and unchanged");
    assert_eq!(cursor.state, CursorState::Forked);
    assert_eq!(dump_all(&r, &[]), before, "nothing changes");
    assert_eq!((r.quarantine_rows(VAULT).unwrap().len(), nodes(&r, "a")), (q, n));

    // A genuine continuation applies right away, and a relist of the forked
    // stream (bogus included) changes nothing more.
    let e0 = link("a", 4, &head(&w.d0), mc(51, "a", T0 + 31, &[("a", 3)], "memo-e0", "e0"));
    assert!(matches!(feed(&mut r, &Feed::One(e0.clone())), ApplyOutcome::Applied { .. }));
    assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), q);
    let mut rescan = r3_steps(&w);
    rescan.push(Feed::One(bogus));
    rescan.push(Feed::One(e0.clone()));
    for step in &rescan {
        feed(&mut r, step);
    }
    let mut clean_steps = r3_steps(&w);
    clean_steps.push(Feed::One(e0));
    let clean = run(&clean_steps);
    assert_eq!(hash(&r), hash(&clean), "the bogus segment left no trace");
    assert!(!op_ids(&r).contains(&oid(50)));
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);

    // A known head with the wrong next seq is a break of that chain.
    let mut r = run(&r3_steps(&w));
    let skip = link("a", 5, &head(&w.d0), mc(52, "a", T0 + 32, &[("a", 3)], "memo-skip", "skip"));
    let out = feed(&mut r, &Feed::One(skip));
    assert!(matches!(out, ApplyOutcome::Held { ref reason } if reason.contains("does not follow")), "{out:?}");
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().applied_seq, 3);
    assert!(!op_ids(&r).contains(&oid(52)));

    // An unattached object listed under the same name as a genuine
    // continuation is deferred alone (red-team f3): the genuine one applies
    // at once, the stray leaves no trace and nothing is quarantined; a relist
    // of the same name is then deferred and writes nothing.
    let mut r = run(&[Feed::Many(w.v.clone()), Feed::Many(w.c.clone())]);
    let stray = link("a", 3, &"cd".repeat(32), mc(53, "a", T0 + 33, &[("a", 2)], "memo-stray", "stray"));
    let mixed = Feed::Many(vec![w.d0.clone(), stray]);
    let out = feed(&mut r, &mixed);
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);
    assert!(op_ids(&r).contains(&oid(30)));
    assert!(!op_ids(&r).contains(&oid(53)));
    assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
    let before = dump_all(&r, &[]);
    let out = feed(&mut r, &mixed);
    assert!(matches!(out, ApplyOutcome::Deferred { .. }), "{out:?}");
    assert_eq!(dump_all(&r, &[]), before, "a relist of the mixed name writes nothing");
}

/// Head bookkeeping grows with distinct applied segments only: re-listing
/// the same objects adds nothing, and an unforked stream records no heads.
#[test]
fn g3_p2_f4_known_heads_grow_only_with_distinct_segments() {
    let w = three_way();
    let mut steps = r3_steps(&w);
    let mut prev = head(&w.d0);
    for seq in 4..154u64 {
        let s = link("a", seq, &prev, mc(1_000 + seq, "a", T0 + seq, &[("a", seq - 1)], &format!("memo-g{seq}"), "g"));
        prev = head(&s);
        steps.push(Feed::One(s));
    }
    let mut prev_b = head(&w.b1);
    for seq in 2..32u64 {
        let s = link("b", seq, &prev_b, mc(5_000 + seq, "b", T0 + seq, &[("b", seq - 1)], &format!("memo-b{seq}"), "b"));
        prev_b = head(&s);
        steps.push(Feed::One(s));
    }
    let mut r = run(&steps);
    let distinct_a = 3 + 3 + 1 + 150;
    let grown = nodes(&r, "a");
    assert!(grown <= distinct_a + 1, "{grown} heads for {distinct_a} segments (+ the genesis head)");
    assert_eq!(nodes(&r, "b"), 0, "an unforked stream keeps only its cursor head");
    let (h, ops) = (hash(&r), op_ids(&r).len());
    for _ in 0..3 {
        for step in &steps {
            feed(&mut r, step);
        }
    }
    assert_eq!(nodes(&r, "a"), grown, "re-listing adds no head");
    assert_eq!((hash(&r), op_ids(&r).len()), (h, ops));
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().applied_seq, 153);
}

// ------------------------------------------------------------ orphan bases

/// b's edit e (base x from a) reaches readers before, after, or without x;
/// all six stream orders end in one store state, equal to the full set.
#[test]
fn g3_p2_o1_base_after_edit_equals_base_first_in_every_stream_order() {
    let c = link("b", 1, &genesis(VAULT, "b"), mc(1, "b", T0, &[], M, "base"));
    let e = link("b", 2, &head(&c), me(4, "b", T0 + 20, &[("a", 1), ("b", 1)], M, 2, "e"));
    let x = link("a", 1, &genesis(VAULT, "a"), me(2, "a", T0 + 10, &[("b", 1)], M, 1, "x"));
    let y = link("d", 1, &genesis(VAULT, "d"), me(3, "d", T0 + 30, &[("b", 1)], M, 1, "y"));
    let stream = |d: &str| -> Vec<Feed> {
        match d {
            "a" => vec![Feed::One(x.clone())],
            "b" => vec![Feed::One(c.clone()), Feed::One(e.clone())],
            _ => vec![Feed::One(y.clone())],
        }
    };
    // Edit before its base, base never arrives (yet): the create stays the memo.
    let partial = run(&stream("b"));
    let p = partial.projection(VAULT).unwrap();
    assert!(p.notices.is_empty(), "{:?}", p.notices);
    assert_eq!(memo_texts(&p), BTreeSet::from(["e".to_owned()]), "the create is not turned into a copy");

    let full: Vec<Pub> = [&c, &e, &x, &y].iter().map(|s| (s.ops[0].body.clone(), dot(&s.device_id, s.first_seq))).collect();
    let expected = order_free(&full, 200, 0x0b);
    assert_eq!(memo_of(&expected, M).unwrap().text, "y");
    assert_eq!(memo_of(&expected, &conflict_copy_id(M, &oid(2))).unwrap().text, "e", "e follows its base into x's copy");
    let mut dumps = BTreeSet::new();
    for order in [["a", "b", "d"], ["a", "d", "b"], ["b", "a", "d"], ["b", "d", "a"], ["d", "a", "b"], ["d", "b", "a"]] {
        let steps: Vec<Feed> = order.iter().flat_map(|d| stream(d)).collect();
        let r = run(&steps);
        assert_eq!(hash(&r), expected.hash, "{order:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{order:?}");
        dumps.insert(dump_vault(&r, VAULT));
    }
    assert_eq!(dumps.len(), 1, "every arrival order leaves byte-identical vault rows");
}

/// A chain of orphan edits (c ← x ← e1 ← {e2, e2'} ← e3): every subset of
/// arrivals projects order-free, only e2/e2' (same known base e1) ever
/// compete, and the create or x never becomes a copy.
#[test]
fn g3_p2_o2_chains_of_orphan_edits_in_every_subset() {
    let pubs = [
        at(mc(1, "b", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 10, &[("b", 1)], M, 1, "x"), 1),
        at(me(3, "b", T0 + 20, &[("a", 1), ("b", 1)], M, 2, "e1"), 2),
        at(me(4, "b", T0 + 30, &[("a", 1), ("b", 2)], M, 3, "e2"), 3),
        at(me(5, "c", T0 + 25, &[("a", 1), ("b", 2)], M, 3, "e2p"), 1),
        at(me(6, "b", T0 + 40, &[("a", 1), ("b", 3)], M, 4, "e3"), 4),
    ];
    for mask in 1u32..64 {
        let subset: Vec<Pub> = (0..6).filter(|i| mask & (1 << i) != 0).map(|i| pubs[i].clone()).collect();
        let p = order_free(&subset, 12, u64::from(mask));
        assert!(memo_of(&p, M).is_some(), "mask {mask:06b}: the memo is live");
        let has = |i: u32| mask & (1 << i) != 0;
        // Contract update (leader decision p2_o3): e2p (c, base e1) is
        // concurrent with e2 and with e3 (neither ctx covers c:1). Whether
        // e1 / e2 are present or still missing, e2p loses to the memo's text
        // and is a copy as soon as e2 or e3 is present (no orphan edit is
        // ever invisible).
        let want = if has(4) && (has(3) || has(5)) {
            vec![(conflict_copy_id(M, &oid(5)), oid(5))]
        } else {
            Vec::new()
        };
        assert_eq!(copies(&p), want, "mask {mask:06b}");
    }
    let full = project_all(&pubs);
    assert_eq!(memo_of(&full, M).unwrap().text, "e3");
    assert_eq!(memo_of(&full, &conflict_copy_id(M, &oid(5))).unwrap().text, "e2p");
}

/// The base never arrives: a concurrent orphan edit must still be visible
/// (as the winner or as a copy with a notice), never silently hidden.
#[test]
fn g3_p2_o3_concurrent_orphan_is_never_hidden_while_its_base_is_missing() {
    let c = at(mc(1, "b", T0, &[], M, "base"), 1);
    let y = at(me(3, "d", T0 + 30, &[("b", 1)], M, 1, "y"), 1);
    let e = at(me(4, "b", T0 + 20, &[("a", 1), ("b", 1)], M, 2, "e"), 2);
    let e1 = at(me(5, "b", T0 + 20, &[("a", 1), ("b", 1)], M, 2, "e1"), 2);
    let e2 = at(me(6, "c", T0 + 21, &[("a", 1), ("b", 1)], M, 2, "e2"), 1);
    let cases: [(&str, Vec<Pub>, [&str; 2]); 2] = [
        ("orphan vs a known-lineage edit", vec![c.clone(), y, e], ["y", "e"]),
        ("two orphans on one missing base", vec![c, e1, e2], ["e1", "e2"]),
    ];
    let mut hidden = Vec::new();
    for (name, set, texts) in cases {
        let p = order_free(&set, 50, 9);
        let shown = memo_texts(&p);
        for t in texts {
            if !shown.contains(t) {
                hidden.push(format!("{name}: {t} hidden (shown {shown:?}, notices {:?})", p.notices));
            }
        }
    }
    assert!(hidden.is_empty(), "{hidden:#?}");
}

// ---------------------------------------------------- pending_segments trim

/// Trims are all-or-nothing and only for published sealed ranges; a trim
/// never lets an attempted segment be resealed, and compaction_due clears
/// only for trimmed segments. Contract update (leader decision p2_p2): a trim
/// is accepted only for ranges `segments_deletable` offered, so each trim
/// follows a verified snapshot and an offer.
#[test]
fn g3_p2_p1_trim_refusals_never_reseal_and_compaction_due_clears() {
    use super::compaction::{compaction_due, MAX_SEGMENT_AGE_MS};
    use super::store::now_ms;
    let offer = |a: &Store| {
        let snap = build_snapshot(a, VAULT).unwrap();
        assert!(verify_snapshot(a, VAULT, &snap).unwrap());
        segments_deletable(a, VAULT, "a", &[u64::MAX], true, &snap).unwrap()
    };
    let mut remote = Remote::new(true);
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "one"));
    let s1 = seal_next(&mut a).unwrap();
    a.mark_attempted(VAULT, "a", 1).unwrap();
    local(&mut a, create(2, 2_000, K, "two"));
    let (s2, created) = publish(&mut a, &mut remote).unwrap();
    assert!(created);
    let aged = now_ms() + MAX_SEGMENT_AGE_MS;
    assert!(compaction_due(&a, VAULT, aged).unwrap());
    assert_eq!(offer(&a), vec![(2, 2)]);

    let before = a.dump_state().unwrap();
    let refused: [(&str, Vec<(u64, u64)>); 6] = [
        ("attempted, publish unconfirmed", vec![(1, 1)]),
        ("published then attempted", vec![(2, 2), (1, 1)]),
        ("wrong last", vec![(2, 3)]),
        ("never sealed", vec![(3, 3)]),
        ("same range twice", vec![(2, 2), (2, 2)]),
        ("inverted", vec![(2, 1)]),
    ];
    for (name, ranges) in &refused {
        assert!(a.forget_segments(VAULT, "a", ranges).is_err(), "{name}");
        assert_eq!(a.dump_state().unwrap(), before, "{name}: a refused trim changes nothing");
    }

    assert_eq!(a.forget_segments(VAULT, "a", &[(2, 2)]).unwrap(), 1);
    assert!(a.sealed_bytes(VAULT, "a", 2).unwrap().is_none());
    assert_eq!(a.sealed_bytes(VAULT, "a", 1).unwrap().unwrap().bytes, s1.bytes, "the attempted resend keeps its exact bytes");
    assert!(seal_next(&mut a).is_none(), "nothing is resealed after a trim");
    assert!(a.mark_attempted(VAULT, "a", 2).is_err() && a.mark_published(VAULT, "a", 2).is_err(), "a trimmed range has no state machine");
    assert_eq!(a.op_state(VAULT, &oid(2)).unwrap().unwrap().2, UploadState::Published);
    assert!(compaction_due(&a, VAULT, aged).unwrap(), "the attempted segment is still live");

    // New ops seal at 3 and chain onto the trimmed 2-2.
    local(&mut a, create(3, 3_000, "memo-3", "three"));
    let s3 = seal_next(&mut a).unwrap();
    assert_eq!((s3.first_seq, s3.last_seq), (3, 3));
    let plain2 = open_segment(&seg_key(), &addr("a", 2, 2), &s2.bytes).unwrap();
    let plain3 = open_segment(&seg_key(), &addr("a", 3, 3), &s3.bytes).unwrap();
    assert_eq!(plain3.prev_seg_hash, head(&plain2));

    a.mark_published(VAULT, "a", 1).unwrap();
    a.mark_attempted(VAULT, "a", 3).unwrap();
    a.mark_published(VAULT, "a", 3).unwrap();
    assert_eq!(offer(&a), vec![(1, 1), (3, 3)]);
    assert_eq!(a.forget_segments(VAULT, "a", &[(1, 1), (3, 3)]).unwrap(), 2);
    assert!(!compaction_due(&a, VAULT, aged).unwrap(), "the trigger clears once every live segment is trimmed");
    assert!(seal_next(&mut a).is_none());
    assert_eq!(a.dots_of(VAULT, &oid(1)).unwrap(), vec![dot("a", 1)], "ops and dots stay");
}

/// Leader decision: pending_segments rows are trimmed only after the remote
/// delete is confirmed, i.e. only ranges `segments_deletable` would offer
/// (verified snapshot, below every reader, not retired, not forked); any
/// other fully published sealed range is refused.
#[test]
fn g3_p2_p2_trim_of_a_range_compaction_never_offered_is_refused() {
    use super::compaction::{compaction_due, MAX_SEGMENT_AGE_MS};
    use super::store::now_ms;
    let aged = now_ms() + MAX_SEGMENT_AGE_MS + 60_000;
    let mut accepted = Vec::new();

    // (1) No snapshot was ever built or verified.
    let mut remote = Remote::new(true);
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "one"));
    publish(&mut a, &mut remote).unwrap();
    let due = compaction_due(&a, VAULT, aged).unwrap();
    if a.forget_segments(VAULT, "a", &[(1, 1)]).is_ok() {
        accepted.push(format!("unverified (compaction_due {due} -> {})", compaction_due(&a, VAULT, aged).unwrap()));
    }

    // (2) The own stream is forked.
    let mut f = device("f");
    local(&mut f, create(1, 1_000, M, "mine"));
    let p = seal_next(&mut f).unwrap();
    f.mark_attempted(VAULT, "f", 1).unwrap();
    f.mark_published(VAULT, "f", 1).unwrap();
    let mine = open_segment(&seg_key(), &addr("f", 1, 1), &p.bytes).unwrap();
    let mut theirs = op(2, 1_001, Kind::Memo, K, "create", json!({"text": "clone", "pinned": false}));
    theirs.origin_device = "f".to_owned();
    f.apply_fork_variant(VAULT, "f", 1, 1, &[mine, link("f", 1, &genesis(VAULT, "f"), theirs)]).unwrap();
    let snap = build_snapshot(&f, VAULT).unwrap();
    assert!(verify_snapshot(&f, VAULT, &snap).unwrap());
    assert!(segments_deletable(&f, VAULT, "f", &[u64::MAX], true, &snap).unwrap().is_empty());
    if f.forget_segments(VAULT, "f", &[(1, 1)]).is_ok() {
        accepted.push("forked own stream".to_owned());
    }

    // (3) A retired id.
    let (mut r, hashes) = rotation_world();
    let plan = plan_for(&r, &hashes, "a2");
    r.apply_rotation(VAULT, &plan, &hashes, &identity("a2")).unwrap();
    if r.forget_segments(VAULT, "a", &[(1, 1)]).is_ok() {
        accepted.push("retired id".to_owned());
    }
    assert!(accepted.is_empty(), "trimmed without a confirmed compaction: {accepted:?}");
}

// --------------------------------------------------------- future versions

struct FutureWorld {
    segs: Vec<Segment>,
    bytes: Vec<Vec<u8>>,
    reader: Store,
}

/// a sealed 1-1, 2-2, 3-3; the reader applied 1-1 only.
fn future_world() -> FutureWorld {
    let mut a = device("a");
    let (mut segs, mut bytes) = (Vec::new(), Vec::new());
    for n in 1..=3u64 {
        local(&mut a, create(n, n * 1_000, &format!("memo-f{n}"), "x"));
        let p = seal_next(&mut a).unwrap();
        segs.push(open_segment(&seg_key(), &addr("a", n, n), &p.bytes).unwrap());
        bytes.push(p.bytes);
    }
    let mut reader = device("r");
    reader.apply_sealed(VAULT, &seg_key(), &addr("a", 1, 1), &bytes[0]).unwrap();
    FutureWorld { segs, bytes, reader }
}

/// Envelope version and plaintext `v` (segment and op, up to u64::MAX):
/// FutureVersion, the cursor's position untouched, no quarantine, every
/// later segment skipped without quarantine, and a fork that re-attaches
/// the stream never applies anything chained after the future object.
#[test]
fn g3_p2_v1_future_versions_hold_without_quarantine_and_keep_the_cursor() {
    type Mutate = fn(&mut Segment);
    let big = i64::MAX as u64 + 1;
    let plain: [(&str, Mutate, u64); 6] = [
        ("segment v 2", |s| s.v = 2, 2),
        ("op v 2", |s| s.ops[0].body.v = 2, 2),
        ("segment v i64::MAX+1", |s| s.v = i64::MAX as u64 + 1, big),
        ("segment v u64::MAX", |s| s.v = u64::MAX, u64::MAX),
        ("op v u64::MAX", |s| s.ops[0].body.v = u64::MAX, u64::MAX),
        ("op v 2 and a foreign device in the plaintext", |s| {
            s.ops[0].body.v = 2;
            s.device_id = "c".to_owned();
        }, 2),
    ];
    let mut cases: Vec<(String, Vec<u8>, u64, Option<Segment>)> = Vec::new();
    let w = future_world();
    let mut envelope = w.bytes[1].clone();
    envelope[0] = 0x02;
    cases.push(("envelope version 2".to_owned(), envelope, 2, None));
    for (name, mutate, v) in plain {
        let mut s = w.segs[1].clone();
        mutate(&mut s);
        let bytes = seal(&seg_key(), &addr("a", 2, 2), &s.encode().unwrap()).unwrap();
        cases.push((name.to_owned(), bytes, v, Some(s)));
    }
    let key = seg_key();
    for (name, bytes, v, future) in cases {
        let FutureWorld { segs, bytes: genuine, mut reader } = future_world();
        let cursor = reader.cursor(VAULT, "a").unwrap().unwrap();
        let before = dump_all(&reader, &["cursors"]);
        let position = |r: &Store| {
            let c = r.cursor(VAULT, "a").unwrap().unwrap();
            (c.applied_seq, c.chain_head)
        };
        assert_eq!(reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &bytes).unwrap(), ApplyOutcome::FutureVersion(v), "{name}");
        assert_eq!(reader.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held, "{name}");
        assert_eq!(position(&reader), (cursor.applied_seq, cursor.chain_head), "{name}");
        assert_eq!(dump_all(&reader, &["cursors"]), before, "{name}: nothing but the cursor state");
        for (n, label) in [(3, "later"), (2, "genuine same name")] {
            let out = reader.apply_sealed(VAULT, &key, &addr("a", n, n), &genuine[n as usize - 1]).unwrap();
            assert_eq!(out, ApplyOutcome::Skipped(CursorState::Held), "{name}: {label}");
        }
        assert_eq!(reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &bytes).unwrap(), ApplyOutcome::FutureVersion(v), "{name}: again");
        assert!(reader.quarantine_rows(VAULT).unwrap().is_empty(), "{name}: never quarantined");
        assert_eq!(position(&reader), (cursor.applied_seq, cursor.chain_head), "{name}");

        // A fork variant set re-attaches the stream; whatever is chained after
        // the future object must still not apply.
        let clone1 = link("a", 1, &genesis(VAULT, "a"), mc(91, "a", T0, &[], "memo-clone", "clone"));
        feed(&mut reader, &Feed::Many(vec![segs[0].clone(), clone1]));
        if let Some(future) = future {
            let cont = link("a", 3, &head(&future), mc(90, "a", T0 + 1, &[("a", 2)], "memo-cont", "cont"));
            let out = feed(&mut reader, &Feed::One(cont));
            assert!(!matches!(out, ApplyOutcome::Applied { .. }), "{name}: continuation of a future object applied: {out:?}");
        }
        assert_eq!(reader.apply_sealed(VAULT, &key, &addr("a", 2, 2), &bytes).unwrap(), ApplyOutcome::FutureVersion(v), "{name}");
        let ids = op_ids(&reader);
        assert!(!ids.contains(&oid(2)) && !ids.contains(&oid(3)) && !ids.contains(&oid(90)), "{name}: {ids:?}");
        assert!(reader.quarantine_rows(VAULT).unwrap().is_empty(), "{name}");
    }
}

/// The same hold on a forked stream, as a single object and inside a
/// variant set: the cursor and every known head stay as they were.
#[test]
fn g3_p2_v2_future_versions_on_a_forked_stream() {
    let w = three_way();
    let mut future = mc(60, "a", T0 + 40, &[("a", 3)], "memo-future", "future");
    future.v = 2;
    let fseg = link("a", 4, &head(&w.d0), future);
    let next = link("a", 5, &head(&fseg), mc(61, "a", T0 + 41, &[("a", 4)], "memo-next", "next"));
    let mut r = run(&r3_steps(&w));
    let (applied, n, q, ids) = (
        r.cursor(VAULT, "a").unwrap().unwrap().applied_seq,
        nodes(&r, "a"),
        r.quarantine_rows(VAULT).unwrap().len(),
        op_ids(&r),
    );
    assert_eq!(feed(&mut r, &Feed::One(fseg.clone())), ApplyOutcome::FutureVersion(2));
    let c = r.cursor(VAULT, "a").unwrap().unwrap();
    assert_eq!((c.state, c.applied_seq), (CursorState::Held, applied));
    assert_eq!(feed(&mut r, &Feed::One(next)), ApplyOutcome::Skipped(CursorState::Held));
    assert_eq!((nodes(&r, "a"), r.quarantine_rows(VAULT).unwrap().len(), op_ids(&r)), (n, q, ids.clone()));

    let mut r = run(&r3_steps(&w));
    let mut sibling = mc(62, "a", T0 + 42, &[("a", 3)], "memo-sib", "sib");
    sibling.v = 2;
    let sib = link("a", 4, &head(&w.d0), sibling);
    let genuine = link("a", 4, &head(&w.d0), mc(63, "a", T0 + 43, &[("a", 3)], "memo-gen", "gen"));
    let out = feed(&mut r, &Feed::Many(vec![genuine, sib]));
    assert_eq!(out, ApplyOutcome::FutureVersion(2));
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().applied_seq, applied);
    assert_eq!((nodes(&r, "a"), r.quarantine_rows(VAULT).unwrap().len(), op_ids(&r)), (n, q, ids));
}

// -------------------------------------------------------------- i64 bounds

/// Every integer field at i64::MAX is accepted; above it each is rejected
/// (quarantine + hold), held on a gap, or reported as a future version,
/// and never fails the cycle or panics.
#[test]
fn g3_p2_i1_i64_bounds_on_every_integer_field() {
    let max = i64::MAX as u64;
    let key = seg_key();
    // hlc.wall_ms
    let mut ok = mc(1, "a", T0, &[], M, "x");
    ok.hlc.wall_ms = max;
    ok.ctx = ctx_of(&[("b", max)]);
    assert_eq!(ok.validate(), Ok(()), "i64::MAX wall_ms and ctx are storable");
    let mut r = device("r");
    let out = feed(&mut r, &Feed::One(link("a", 1, &genesis(VAULT, "a"), ok.clone())));
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
    assert_eq!(r.load_set(VAULT).unwrap().0.get(&oid(1)).unwrap().body.hlc, ok.hlc);
    let mut over = ok.clone();
    over.hlc.wall_ms = max + 1;
    over.ctx = BTreeMap::new();
    assert!(matches!(over.validate(), Err(OpError::Range(_))));
    let mut r = device("r");
    let out = feed(&mut r, &Feed::One(link("a", 1, &genesis(VAULT, "a"), over)));
    assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
    assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1);
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().applied_seq, 0);

    // first_seq / last_seq / seq
    let kbody = mc(2, "a", T0, &[], K, "k");
    for (name, first, last, seq) in [("first", max + 1, max + 1, max + 1), ("seq", 1, 1, max + 1), ("last", 1, max + 1, 1)] {
        let s = Segment { v: 1, device_id: "a".to_owned(), first_seq: first, last_seq: last, prev_seg_hash: genesis(VAULT, "a"), ops: vec![SegOp { seq, body: kbody.clone() }] };
        assert!(matches!(s.validate(), Err(OpError::Range(_))), "{name}");
    }
    let edge = link("a", max, &genesis(VAULT, "a"), kbody.clone());
    assert_eq!(edge.validate(), Ok(()));
    let over_seq = link("a", max + 1, &genesis(VAULT, "a"), kbody.clone());
    let mut r = device("r");
    let out = feed(&mut r, &Feed::One(over_seq.clone()));
    assert!(matches!(out, ApplyOutcome::Held { .. }), "Ok stream: a gap hold: {out:?}");
    // Forked stream, as a single object and as a variant set.
    let w = three_way();
    let after_d0 = |seq: u64, n: u64| link("a", seq, &head(&w.d0), mc(n, "a", T0, &[("a", 3)], &format!("memo-big{n}"), "big"));
    let mut r = run(&r3_steps(&w));
    for step in [Feed::One(after_d0(max + 1, 70)), Feed::Many(vec![after_d0(max + 1, 71), after_d0(max + 1, 72)])] {
        let out = feed(&mut r, &step);
        assert!(matches!(out, ApplyOutcome::Held { .. } | ApplyOutcome::Quarantined { .. } | ApplyOutcome::Skipped(_)), "{out:?}");
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().applied_seq, 3);
    }
    assert!(op_ids(&r).iter().all(|id| ![oid(70), oid(71), oid(72)].contains(id)));

    // hlc.ctr beyond u32 (raw JSON): a decode failure, quarantined.
    let t = template();
    let bytes = seal(&key, &addr("a", 1, 1), &with_raw_number(&t, "4294967296")).unwrap();
    let mut r = device("r");
    assert!(matches!(r.apply_sealed(VAULT, &key, &addr("a", 1, 1), &bytes).unwrap(), ApplyOutcome::Quarantined { .. }));
    // v: a future version (asserted in g3_p2_v1).

    // Observation: payload integers are not range-checked; they are stored as
    // JSON text only and never bound as SQL integers (G4/G8 to review).
    let payload = |id: &str, ts: u64| json!({"conv_id": CV, "msg_id": id, "parent_id": null, "role": "user", "content": id, "attachments": [], "ts": ts});
    let m1 = at(body_of(80, "a", (Kind::Conv, CV, "msg.add"), payload("m1", u64::MAX)), 2);
    let m2 = at(body_of(81, "b", (Kind::Conv, CV, "msg.add"), payload("m2", max + 1)), 1);
    let conv = at(body(82, "a", T0, &[], (Kind::Conv, CV, "create"), json!({"title": "t"})), 1);
    let p = order_free(&[conv.clone(), m1.clone(), m2], 50, 4);
    assert!(matches!(p.entities.get(&(Kind::Conv, CV.to_owned())), Some(EntityState::Conv(c)) if c.messages.len() == 2));
    let mut r = device("r");
    let conv_seg = link("a", 1, &genesis(VAULT, "a"), conv.0);
    feed(&mut r, &Feed::One(conv_seg.clone()));
    let out = feed(&mut r, &Feed::One(link("a", 2, &head(&conv_seg), m1.0)));
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
}

fn body_of(n: u64, origin: &str, what: (Kind, &str, &str), payload: Value) -> OpBody {
    body(n, origin, T0 + n, &[], what, payload)
}

/// Shared decision: hlc.wall_ms, seqs, and ctx values above i64::MAX are
/// rejected at validation, then quarantined and held.
#[test]
fn g3_p2_i2_ctx_value_beyond_i64_is_rejected_quarantined_and_held() {
    let mut b = mc(1, "a", T0, &[], M, "x");
    b.ctx = ctx_of(&[("b", i64::MAX as u64 + 1)]);
    let validated = b.validate();
    let mut r = device("r");
    let out = feed(&mut r, &Feed::One(link("a", 1, &genesis(VAULT, "a"), b)));
    let state = r.cursor(VAULT, "a").unwrap().map(|c| c.state);
    let stored = op_ids(&r).contains(&oid(1));
    assert!(
        matches!(validated, Err(OpError::Range(_)))
            && matches!(out, ApplyOutcome::Held { .. })
            && state == Some(CursorState::Held)
            && r.quarantine_rows(VAULT).unwrap().len() == 1
            && !stored,
        "validate {validated:?}, outcome {out:?}, cursor {state:?}, op stored {stored}"
    );
}

// --------------------------------------------------------- snapshot verify

/// Leader decision: verify compares the full op set, dots, ctx, obs, and
/// forks. Dropping or altering any single one fails verify and offers
/// nothing for deletion; a verify (pass or fail) writes nothing.
#[test]
fn g3_p2_s1_single_dot_ctx_obs_fork_tampers_fail_verify_and_delete_nothing() {
    let mut w = redeleted_fork_world();
    // An alias of b's create at (a2,1) with an extra ctx entry.
    let mut alias = w.b.load_set(VAULT).unwrap().0.get(&oid(1)).unwrap().body.clone();
    alias.origin_device = "a2".to_owned();
    alias.ctx = ctx_of(&[("q", 3)]);
    let out = feed(&mut w.b, &Feed::One(link("a2", 1, &genesis(VAULT, "a2"), alias)));
    assert!(matches!(out, ApplyOutcome::Applied { conflicts: 0, .. }), "{out:?}");
    let store = &w.b;
    let snap = build_snapshot(store, VAULT).unwrap();
    assert!(verify_snapshot(store, VAULT, &snap).unwrap(), "sanity: the genuine snapshot verifies");
    let offered = segments_deletable(store, VAULT, "b", &[u64::MAX], true, &snap).unwrap();
    assert!(!offered.is_empty(), "sanity");
    let i1 = op_index(&snap, 1);
    assert_eq!(snap.ops[i1].dots, vec![dot("a2", 1), dot("b", 1)]);
    assert_eq!(snap.ops[i1].body.ctx, ctx_of(&[("q", 3)]));
    let i5 = op_index(&snap, 5);
    assert_eq!(snap.ops[i5].body.obs, vec![oid(2), oid(3)]);

    type T = Box<dyn Fn(&mut Snapshot)>;
    let cases: Vec<(&str, T)> = vec![
        ("drop the alias dot", Box::new(move |s| s.ops[i1].dots.retain(|d| d.device != "a2"))),
        ("drop the own dot", Box::new(move |s| s.ops[i1].dots.retain(|d| d.device != "b"))),
        ("move a dot", Box::new(move |s| s.ops[i1].dots[1].seq = 9)),
        ("add a dot", Box::new(move |s| s.ops[i1].dots.push(dot("z", 1)))),
        ("drop a ctx entry", Box::new(move |s| {
            s.ops[i1].body.ctx.remove("q");
        })),
        ("alter a ctx value", Box::new(move |s| {
            s.ops[i1].body.ctx.insert("q".to_owned(), 4);
        })),
        ("add a ctx entry", Box::new(move |s| {
            s.ops[i1].body.ctx.insert("zz".to_owned(), 1);
        })),
        ("drop one obs entry", Box::new(move |s| s.ops[i5].body.obs = vec![oid(2)])),
        ("swap an obs entry", Box::new(move |s| s.ops[i5].body.obs = vec![oid(2), oid(4)])),
        ("add an obs entry", Box::new(move |s| s.ops[i5].body.obs = vec![oid(1), oid(2), oid(3)])),
        ("drop the fork", Box::new(|s| {
            s.forks.clear();
        })),
        ("move the fork", Box::new(|s| {
            s.forks.insert("a".to_owned(), 2);
        })),
        ("add a fork", Box::new(|s| {
            s.forks.insert("b".to_owned(), 1);
        })),
        ("alter the frontier", Box::new(|s| {
            s.frontier.insert("b".to_owned(), 1);
        })),
        ("other device id", Box::new(|s| s.device_id = "c".to_owned())),
    ];
    let before = dump_all(store, &[]);
    let pending = store.pending_segments(VAULT, "b").unwrap();
    for (name, tamper) in &cases {
        let mut t = snap.clone();
        tamper(&mut t);
        assert_ne!(t, snap, "{name}: fixture changed something");
        let ok = verify_snapshot(store, VAULT, &t).unwrap_or(false);
        assert!(!ok, "{name}: verify must fail");
        assert!(segments_deletable(store, VAULT, "b", &[u64::MAX], ok, &t).unwrap().is_empty(), "{name}: nothing offered");
        assert_eq!(store.pending_segments(VAULT, "b").unwrap(), pending, "{name}");
    }
    assert_eq!(dump_all(store, &[]), before, "verify is read-only");
}

// ================================================================ pass 3

/// One-op segments of `device` at seqs 1.. chained from its genesis head.
fn chain_of(device: &str, bodies: Vec<OpBody>) -> Vec<Segment> {
    let mut prev = genesis(VAULT, device);
    let mut out = Vec::new();
    for (i, body) in bodies.into_iter().enumerate() {
        let s = link(device, i as u64 + 1, &prev, body);
        prev = head(&s);
        out.push(s);
    }
    out
}

fn pubs_of<'a>(segments: impl IntoIterator<Item = &'a Segment>) -> Vec<Pub> {
    segments
        .into_iter()
        .flat_map(|s| s.ops.iter().map(|op| (op.body.clone(), dot(&s.device_id, op.seq))))
        .collect()
}

fn set_of(pubs: &[Pub]) -> OpSet {
    let mut set = OpSet::new();
    for (body, dot) in pubs {
        set.insert(body.clone(), dot.clone()).unwrap();
    }
    set
}

/// Memo creates/edits of live memos that no live memo shows: neither its
/// text is shown nor did any shown head see it. Must always be empty
/// (leader decision p2_o3: no edit is ever invisible).
fn hidden_in(set: &OpSet, forks: &Forks, p: &Projection) -> Vec<String> {
    let mut all = forks.clone();
    for (d, s) in set.forks() {
        let slot = all.entry(d.clone()).or_insert(*s);
        *slot = (*slot).min(*s);
    }
    let live: Vec<&MemoState> = p
        .entities
        .values()
        .filter_map(|e| match e {
            EntityState::Memo(m) => Some(m),
            _ => None,
        })
        .collect();
    set.iter()
        .filter(|o| o.body.kind == Kind::Memo && matches!(o.body.op_type.as_str(), "create" | "edit"))
        .filter(|o| !is_deleted(p, Kind::Memo, &o.body.entity_id))
        .filter(|o| {
            let text = o.body.payload.get("text").and_then(Value::as_str);
            !live.iter().any(|m| {
                Some(m.text.as_str()) == text || set.get(&m.head_op_id).is_some_and(|h| OpSet::saw_entry(h, o, &all))
            })
        })
        .map(|o| o.body.op_id.clone())
        .collect()
}

fn hidden_edits(store: &Store) -> Vec<String> {
    let (set, forks) = store.load_set(VAULT).unwrap();
    hidden_in(&set, &forks, &store.projection(VAULT).unwrap())
}

/// Every interleaving of streams with `counts[i]` segments each.
fn interleavings(counts: &[usize]) -> Vec<Vec<usize>> {
    fn go(left: &mut Vec<usize>, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if left.iter().all(|n| *n == 0) {
            out.push(cur.clone());
            return;
        }
        for i in 0..left.len() {
            if left[i] > 0 {
                left[i] -= 1;
                cur.push(i);
                go(left, cur, out);
                cur.pop();
                left[i] += 1;
            }
        }
    }
    let mut out = Vec::new();
    go(&mut counts.to_vec(), &mut Vec::new(), &mut out);
    out
}

fn memos_with_text(p: &Projection, text: &str) -> usize {
    p.entities.values().filter(|e| matches!(e, EntityState::Memo(m) if m.text == text)).count()
}

// ------------------------------------------------------ orphan copies (p3)

/// Three devices; b's and d's edits name a's x as their base. All 210
/// interleavings of the three streams (the base first, in the middle, last):
/// after every single apply the store equals the pure projection of what
/// it holds and no edit is hidden; every order ends byte-identical.
#[test]
fn g3_p3_o1_three_device_base_arrival_interleavings_converge_and_never_hide() {
    let streams: [Vec<Segment>; 3] = [
        chain_of(
            "a",
            vec![
                mc(1, "a", T0, &[], M, "base"),
                me(2, "a", T0 + 10, &[("a", 1)], M, 1, "x"),
                mc(3, "a", T0 + 12, &[("a", 2)], K, "k"),
            ],
        ),
        chain_of(
            "b",
            vec![
                me(4, "b", T0 + 20, &[("a", 2)], M, 2, "e"),
                me(5, "b", T0 + 25, &[("a", 2), ("b", 1)], M, 4, "e2"),
            ],
        ),
        chain_of(
            "d",
            vec![
                me(6, "d", T0 + 30, &[("a", 1)], M, 1, "y"),
                me(7, "d", T0 + 15, &[("a", 2), ("d", 1)], M, 2, "z"),
            ],
        ),
    ];
    let all_pubs = pubs_of(streams.iter().flatten());
    let expected = order_free(&all_pubs, 300, 0x3a);
    assert!(hidden_in(&set_of(&all_pubs), &Forks::new(), &expected).is_empty());
    let orders = interleavings(&[3, 2, 2]);
    assert_eq!(orders.len(), 210);
    let mut dumps = BTreeSet::new();
    let mut failures = Vec::new();
    for order in &orders {
        let mut r = device("r");
        let mut next = [0usize; 3];
        let mut applied: Vec<Pub> = Vec::new();
        for &s in order {
            let seg = &streams[s][next[s]];
            next[s] += 1;
            let out = feed(&mut r, &Feed::One(seg.clone()));
            assert!(matches!(out, ApplyOutcome::Applied { .. }), "{order:?}: {out:?}");
            applied.extend(pubs_of([seg]));
            if hash(&r) != project_all(&applied).hash {
                failures.push(format!("{order:?} after {} ops: store != pure projection", applied.len()));
            }
            let hidden = hidden_edits(&r);
            if !hidden.is_empty() {
                failures.push(format!("{order:?} after {} ops: hidden {hidden:?}", applied.len()));
            }
        }
        assert_eq!(hash(&r), expected.hash, "{order:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{order:?}");
        dumps.insert(dump_vault(&r, VAULT));
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(dumps.len(), 1, "every interleaving leaves byte-identical vault rows");
}

/// Four orphans on one missing base, two with identical text: one copy per
/// distinct losing text, in every subset and order; the same copies once
/// the base arrives (the orphans then compete as same-base children).
#[test]
fn g3_p3_o2_multiple_orphans_on_one_missing_base() {
    let c = at(mc(1, "b", T0, &[], M, "base"), 1);
    let saw = [("b", 1), ("g", 1)];
    let orphans = [
        at(me(2, "a", T0 + 10, &saw, M, 9, "p"), 1),
        at(me(3, "d", T0 + 20, &saw, M, 9, "q"), 1),
        at(me(4, "e", T0 + 30, &saw, M, 9, "q"), 1),
        at(me(5, "f", T0 + 40, &saw, M, 9, "r"), 1),
    ];
    let base = at(me(9, "g", T0 + 5, &[("b", 1)], M, 1, "nine"), 1);
    let texts = ["p", "q", "q", "r"];
    for mask in 1u32..16 {
        let mut set = vec![c.clone()];
        set.extend((0..4).filter(|i| mask & (1 << i) != 0).map(|i| orphans[i].clone()));
        let missing = order_free(&set, 30, u64::from(mask));
        set.push(base.clone());
        let arrived = order_free(&set, 30, u64::from(mask) + 100);
        let present: Vec<usize> = (0..4).filter(|i| mask & (1 << i) != 0).collect();
        let win = *present.iter().max().unwrap();
        let mut losing: BTreeMap<&str, String> = BTreeMap::new();
        for &i in &present {
            if texts[i] != texts[win] {
                losing.entry(texts[i]).or_insert_with(|| oid(i as u64 + 2));
            }
        }
        let want: BTreeSet<(String, String)> =
            losing.values().map(|o| (conflict_copy_id(M, o), o.clone())).collect();
        for (name, p, extra) in [("base missing", &missing, None), ("base arrived", &arrived, Some(&base))] {
            let mut pubs = set.clone();
            if extra.is_none() {
                pubs.pop();
            }
            assert_eq!(memo_of(p, M).unwrap().text, texts[win], "mask {mask:04b} {name}");
            assert_eq!(copies(p).into_iter().collect::<BTreeSet<_>>(), want, "mask {mask:04b} {name}");
            let hidden = hidden_in(&set_of(&pubs), &Forks::new(), p);
            assert!(hidden.is_empty(), "mask {mask:04b} {name}: hidden {hidden:?}");
        }
    }
}

/// An orphan whose text equals the shown text is no conflict: no copy, no
/// notice, whether the orphan or the known-lineage edit wins; a third,
/// different orphan still gets its copy.
#[test]
fn g3_p3_o3_orphan_identical_to_the_winner_makes_no_copy() {
    let c = at(mc(1, "b", T0, &[], M, "base"), 1);
    for (name, x_wall, o_wall) in [("known edit wins", T0 + 30, T0 + 10), ("orphan wins", T0 + 10, T0 + 30)] {
        let x = at(me(2, "a", x_wall, &[("b", 1)], M, 1, "same"), 1);
        let o = at(me(3, "d", o_wall, &[("b", 1), ("g", 1)], M, 9, "same"), 1);
        let p = order_free(&[c.clone(), x.clone(), o.clone()], 50, 7);
        assert_eq!(memo_of(&p, M).unwrap().text, "same", "{name}");
        assert!(p.notices.is_empty(), "{name}: {:?}", p.notices);
        assert_eq!(memos_with_text(&p, "same"), 1, "{name}");
        let other = at(me(4, "e", T0 + 5, &[("b", 1), ("g", 1)], M, 9, "other"), 1);
        let p = order_free(&[c.clone(), x, o, other], 50, 8);
        assert_eq!(copies(&p), vec![(conflict_copy_id(M, &oid(4)), oid(4))], "{name}");
        assert_eq!(memos_with_text(&p, "same"), 1, "{name}");
    }
}

/// An orphan whose text equals an existing conflict copy's text: the orphan
/// gets its own copy while its base is missing, exactly as its base's
/// lineage copy shows it once the base arrives (observation: the copy id
/// changes from M/orphan to M/base on arrival).
#[test]
fn g3_p3_o4_orphan_with_the_text_of_an_existing_copy() {
    let c = at(mc(1, "b", T0, &[], M, "base"), 1);
    let x = at(me(2, "a", T0 + 30, &[("b", 1)], M, 1, "X"), 1);
    let y = at(me(3, "d", T0 + 10, &[("b", 1)], M, 1, "Y"), 1);
    let o = at(me(4, "e", T0 + 5, &[("b", 1), ("g", 1)], M, 9, "Y"), 1);
    let base = at(me(9, "g", T0 + 1, &[("b", 1)], M, 1, "nine"), 1);
    let before = order_free(&[c.clone(), x.clone(), y.clone(), o.clone()], 50, 11);
    let after = order_free(&[c.clone(), x.clone(), y.clone(), o.clone(), base.clone()], 50, 12);
    assert_eq!(memo_of(&before, M).unwrap().text, "X");
    assert_eq!(memo_of(&after, M).unwrap().text, "X");
    assert_eq!(memos_with_text(&before, "Y"), 2, "copy(y) and the orphan's copy");
    assert_eq!(memos_with_text(&after, "Y"), 2, "copy(y) and copy(base) headed by the orphan");
    assert_eq!(memo_of(&before, &conflict_copy_id(M, &oid(4))).unwrap().text, "Y");
    assert_eq!(memo_of(&after, &conflict_copy_id(M, &oid(9))).unwrap().text, "Y");
    assert!(memo_of(&after, &conflict_copy_id(M, &oid(4))).is_none(), "observation: the orphan copy id is replaced");
    for (p, pubs) in [(&before, vec![c.clone(), x.clone(), y.clone(), o.clone()]), (&after, vec![c, x, y, o, base])] {
        assert!(hidden_in(&set_of(&pubs), &Forks::new(), p).is_empty());
    }
}

/// The base x sits in a's stream behind a chain break: a is held (and
/// quarantined), x never applies, and b's edit e on x stays an orphan for
/// good. e is shown (as winner or as a copy with a notice) in every order.
#[test]
fn g3_p3_o5_orphan_whose_base_is_in_a_held_stream_stays_visible() {
    let a = chain_of("a", vec![mc(1, "a", T0, &[], M, "base"), me(2, "a", T0 + 10, &[("a", 1)], M, 1, "x")]);
    let broken = link("a", 2, &"ee".repeat(32), a[1].ops[0].body.clone());
    for (name, e_wall, y_wall) in [("y wins", T0 + 20, T0 + 30), ("e wins", T0 + 40, T0 + 30)] {
        let e = link("b", 1, &genesis(VAULT, "b"), me(3, "b", e_wall, &[("a", 2)], M, 2, "e"));
        let y = link("d", 1, &genesis(VAULT, "d"), me(4, "d", y_wall, &[("a", 1)], M, 1, "y"));
        let steps_a = [Feed::One(a[0].clone()), Feed::One(broken.clone()), Feed::One(a[1].clone())];
        let orders: [Vec<Feed>; 3] = [
            steps_a.iter().cloned().chain([Feed::One(e.clone()), Feed::One(y.clone())]).collect(),
            [Feed::One(e.clone())].into_iter().chain(steps_a.iter().cloned()).chain([Feed::One(y.clone())]).collect(),
            [Feed::One(y.clone()), Feed::One(e.clone())].into_iter().chain(steps_a.iter().cloned()).collect(),
        ];
        let expected = project_all(&pubs_of([&a[0], &e, &y]));
        for (i, steps) in orders.iter().enumerate() {
            let r = run(steps);
            assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held, "{name} #{i}");
            assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name} #{i}");
            assert!(!op_ids(&r).contains(&oid(2)), "{name} #{i}: x never applies");
            assert_eq!(hash(&r), expected.hash, "{name} #{i}");
            let shown = memo_texts(&r.projection(VAULT).unwrap());
            assert!(shown.contains("e") && shown.contains("y"), "{name} #{i}: {shown:?}");
            assert!(hidden_edits(&r).is_empty(), "{name} #{i}");
            assert_eq!(copies(&r.projection(VAULT).unwrap()).len(), 1, "{name} #{i}: the loser is a copy with a notice");
        }
    }
}

/// AC 3 with an orphan: a delete that did not see an orphan edit keeps the
/// memo (restored + notice) while the base is missing and after it
/// arrives; a delete that saw the orphan deletes it in both states.
#[test]
fn g3_p3_o6_orphan_edit_versus_delete() {
    let c = at(mc(1, "b", T0, &[], M, "base"), 1);
    let x = at(me(2, "a", T0 + 10, &[("b", 1)], M, 1, "x"), 1);
    let e = at(me(3, "f", T0 + 20, &[("b", 1), ("g", 1)], M, 9, "e"), 1);
    let base = at(me(9, "g", T0 + 5, &[("b", 1)], M, 1, "nine"), 1);
    let blind = at(md(4, "d", T0 + 90, &[("a", 1), ("b", 1), ("g", 1)], M), 1);
    let seeing = at(md(4, "d", T0 + 90, &[("a", 1), ("b", 1), ("f", 1), ("g", 1)], M), 1);
    for (state, with_base) in [("base missing", false), ("base arrived", true)] {
        let mut pubs = vec![c.clone(), x.clone(), e.clone()];
        if with_base {
            pubs.push(base.clone());
        }
        let mut kept = pubs.clone();
        kept.push(blind.clone());
        let p = order_free(&kept, 50, 21);
        assert!(memo_texts(&p).contains("e"), "{state}: the unseen orphan stays visible: {:?}", p.entities);
        if with_base {
            // e follows its base (a losing sibling of x) into copy(9); the
            // delete saw M's own lineage, so M goes and the copy keeps e.
            assert!(is_deleted(&p, Kind::Memo, M), "{state}");
            assert_eq!(memo_of(&p, &conflict_copy_id(M, &oid(9))).unwrap().text, "e", "{state}");
        } else {
            assert!(!is_deleted(&p, Kind::Memo, M), "{state}: the unseen orphan keeps the memo");
            assert!(is_restored(&p, Kind::Memo, M), "{state}: with a notice");
        }
        assert!(hidden_in(&set_of(&kept), &Forks::new(), &p).is_empty(), "{state}");
        let mut gone = pubs;
        gone.push(seeing.clone());
        let p = order_free(&gone, 50, 22);
        assert!(is_deleted(&p, Kind::Memo, M), "{state}: a delete that saw everything deletes");
        // With the base present, e lives in copy(9), an entity the delete
        // of M does not address.
        assert_eq!(memo_texts(&p).contains("e"), with_base, "{state}: {:?}", memo_texts(&p));
    }
}

// ------------------------------------------------------- forked lag (p3)

struct LagWorld {
    v: [Segment; 2],
    c: [Segment; 2],
    d: [Segment; 2],
    e0: Segment,
    b1: Segment,
}

/// a forked at 1 (v0, v1); chain 0 continues c0, d0, e0; chain 1 (the
/// clone) continues c1, d1. b deletes memo-d0 having seen (a,4).
fn lag_world() -> LagWorld {
    let g = genesis(VAULT, "a");
    let v0 = link("a", 1, &g, mc(10, "a", T0, &[], "memo-v0", "v0"));
    let v1 = link("a", 1, &g, mc(11, "a", T0 + 1, &[], "memo-v1", "v1"));
    let c0 = link("a", 2, &head(&v0), mc(20, "a", T0 + 2, &[("a", 1)], "memo-c0", "c0"));
    let c1 = link("a", 2, &head(&v1), mc(23, "a", T0 + 2, &[("a", 1)], "memo-c1", "c1"));
    let d0 = link("a", 3, &head(&c0), mc(21, "a", T0 + 3, &[("a", 2)], "memo-d0", "d0"));
    let d1 = link("a", 3, &head(&c1), mc(24, "a", T0 + 3, &[("a", 2)], "memo-d1", "d1"));
    let e0 = link("a", 4, &head(&d0), me(22, "a", T0 + 4, &[("a", 3)], "memo-c0", 20, "c0 edited"));
    let b1 = link("b", 1, &genesis(VAULT, "b"), md(30, "b", T0 + 5, &[("a", 4)], "memo-d0"));
    LagWorld { v: [v0, v1], c: [c0, c1], d: [d0, d1], e0, b1 }
}

fn one(s: &Segment) -> Feed {
    Feed::One(s.clone())
}

/// Leader decision (architect B1): a child listed before its parent on a
/// forked stream is deferred, writes nothing, and applies once the parent
/// is listed; two lag patterns over three cycles converge on the in-order
/// reader, with no quarantine and the stream still forked.
#[test]
fn g3_p3_f1_child_before_parent_across_three_cycles_converges() {
    let w = lag_world();
    let fork = Feed::Many(w.v.to_vec());
    let reference = run(&[fork.clone(), one(&w.c[0]), one(&w.d[0]), one(&w.e0), one(&w.b1)]);
    let expected = project_all(&pubs_of([&w.v[0], &w.v[1], &w.c[0], &w.d[0], &w.e0, &w.b1]));
    assert_eq!(hash(&reference), expected.hash);
    let rev = vec![one(&w.e0), one(&w.d[0]), one(&w.c[0])];
    let patterns: [(&str, Vec<Vec<Feed>>); 2] = [
        (
            "children listed two cycles early",
            vec![
                vec![fork.clone(), one(&w.e0), one(&w.b1)],
                vec![one(&w.d[0]), one(&w.e0)],
                vec![one(&w.c[0]), one(&w.d[0]), one(&w.e0)],
            ],
        ),
        (
            "newest first every cycle",
            vec![[fork.clone(), one(&w.b1)].into_iter().chain(rev.clone()).collect(), rev.clone(), rev.clone()],
        ),
    ];
    for (name, cycles) in &patterns {
        let mut r = device("r");
        let mut deferred = 0;
        for cycle in cycles {
            for step in cycle {
                let before = dump_all(&r, &[]);
                match feed(&mut r, step) {
                    ApplyOutcome::Deferred { .. } => {
                        deferred += 1;
                        assert_eq!(dump_all(&r, &[]), before, "{name}: a deferral writes nothing");
                    }
                    ApplyOutcome::Applied { .. } | ApplyOutcome::Duplicate => {}
                    other => panic!("{name}: {other:?}"),
                }
            }
        }
        assert!(deferred >= 3, "{name}: {deferred} deferrals");
        assert_eq!(hash(&r), expected.hash, "{name}");
        assert_eq!(op_ids(&r), op_ids(&reference), "{name}");
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]), "{name}");
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked, "{name}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{name}");
        assert_eq!(nodes(&r, "a"), nodes(&reference, "a"), "{name}");
    }
}

/// A segment that never connects is deferred on every cycle and leaves no
/// trace; other streams and genuine continuations keep applying. A garbage
/// seq still holds the stream (validation precedes the head lookup).
#[test]
fn g3_p3_f2_segment_that_never_connects_is_deferred_forever_without_trace() {
    let w = lag_world();
    let base = vec![Feed::Many(w.v.to_vec()), one(&w.c[0]), one(&w.d[0]), one(&w.e0), one(&w.b1)];
    let mut r = run(&base);
    let bogus = link("a", 5, &"ab".repeat(32), mc(50, "a", T0 + 50, &[("a", 4)], "memo-bogus", "bogus"));
    // Observation: a never-connecting object that would fail schema
    // (foreign origin) is deferred, never quarantined, because the chain
    // lookup precedes the schema step on a forked stream.
    let mut foreign = mc(51, "a", T0 + 51, &[("a", 4)], "memo-foreign", "foreign");
    foreign.origin_device = "zz".to_owned();
    let foreign = link("a", 5, &"cd".repeat(32), foreign);
    let mut first_dump = None;
    for cycle in 0..10u64 {
        let mut relist = base.clone();
        relist.push(one(&bogus));
        relist.push(one(&foreign));
        for step in &relist {
            let out = feed(&mut r, step);
            if let Feed::One(s) = step {
                if s.ops[0].body.op_id == oid(50) || s.ops[0].body.op_id == oid(51) {
                    assert!(matches!(out, ApplyOutcome::Deferred { .. }), "cycle {cycle}: {out:?}");
                }
            }
        }
        let dump = dump_all(&r, &[]);
        match &first_dump {
            None => first_dump = Some(dump),
            Some(d) => assert_eq!(&dump, d, "cycle {cycle}: relisting changes nothing"),
        }
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);
    }
    // Other streams continue; a genuine continuation still applies.
    let b2 = link("b", 2, &head(&w.b1), mc(60, "b", T0 + 60, &[("b", 1)], "memo-b2", "b2"));
    assert!(matches!(feed(&mut r, &one(&b2)), ApplyOutcome::Applied { .. }));
    let f0 = link("a", 5, &head(&w.e0), mc(52, "a", T0 + 52, &[("a", 4)], "memo-f0", "f0"));
    assert!(matches!(feed(&mut r, &one(&f0)), ApplyOutcome::Applied { .. }));
    assert!(matches!(feed(&mut r, &one(&bogus)), ApplyOutcome::Deferred { .. }));
    assert!(!op_ids(&r).contains(&oid(50)) && !op_ids(&r).contains(&oid(51)));

    // A seq above i64::MAX is invalid input, not lag: quarantine + held.
    let mut r = run(&base);
    let huge = seg_of("a", u64::MAX, "ab".repeat(32), vec![SegOp { seq: u64::MAX, body: mc(53, "a", T0, &[], "memo-h", "h") }]);
    let out = r.apply_segment(VAULT, "a", u64::MAX, u64::MAX, &huge).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. } | ApplyOutcome::Quarantined { .. }), "{out:?}");
    assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1);
}

/// Leader decision: a forked cursor "applies any segment that extends one
/// of [its known heads]. It never skips." Here the clone's c1 object is
/// unreadable (quarantined), so its child d1 never connects. d0, listed
/// under the same name as d1, extends the known head c0, and e0 extends d0.
#[test]
fn g3_p3_f3_never_connecting_variant_must_not_block_a_connecting_sibling() {
    let w = lag_world();
    let mut bad_c1 = sealed(&w.c[1]);
    let last = bad_c1.len() - 1;
    bad_c1[last] ^= 1;
    let mut r = device("r");
    let mut outcomes = Vec::new();
    for _cycle in 0..5 {
        outcomes.push(feed(&mut r, &Feed::Many(w.v.to_vec())));
        outcomes.push(r.apply_sealed(VAULT, &seg_key(), &addr("a", 2, 2), &bad_c1).unwrap());
        outcomes.push(feed(&mut r, &one(&w.c[0])));
        outcomes.push(feed(&mut r, &Feed::Many(w.d.to_vec())));
        outcomes.push(feed(&mut r, &one(&w.e0)));
        outcomes.push(feed(&mut r, &one(&w.b1)));
    }
    let ids = op_ids(&r);
    assert!(
        ids.contains(&oid(21)) && ids.contains(&oid(22)),
        "d0 applied {}, e0 applied {}, cursor {:?}, quarantine {:?}, last cycle {:?}",
        ids.contains(&oid(21)),
        ids.contains(&oid(22)),
        r.cursor(VAULT, "a").unwrap(),
        r.quarantine_rows(VAULT).unwrap(),
        &outcomes[outcomes.len() - 6..]
    );
}

/// Deferred children interleave with a fork found later at a lower seq:
/// a is first forked at 3 (s3, x3); y4 and y3 of a clone chain from seq 2
/// are listed before (s2, y2) reveals fork_start 2. After a relist the
/// reader equals the full-set projection with fork_start 2.
#[test]
fn g3_p3_f4_deferred_children_interleaved_with_a_new_lower_fork_start() {
    let g = genesis(VAULT, "a");
    let s1 = link("a", 1, &g, mc(1, "a", T0, &[], "memo-1", "one"));
    let s2 = link("a", 2, &head(&s1), mc(2, "a", T0 + 1, &[("a", 1)], "memo-s2", "s2"));
    let s3 = link("a", 3, &head(&s2), mc(3, "a", T0 + 2, &[("a", 2)], "memo-s3", "s3"));
    let s4 = link("a", 4, &head(&s3), mc(4, "a", T0 + 3, &[("a", 3)], "memo-s4", "s4"));
    let x3 = link("a", 3, &head(&s2), mc(5, "a", T0 + 4, &[("a", 2)], "memo-x3", "x3"));
    let y2 = link("a", 2, &head(&s1), mc(6, "a", T0 + 5, &[("a", 1)], "memo-y2", "y2"));
    let y3 = link("a", 3, &head(&y2), mc(7, "a", T0 + 6, &[("a", 2)], "memo-y3", "y3"));
    let y4 = link("a", 4, &head(&y3), mc(8, "a", T0 + 7, &[("a", 3)], "memo-y4", "y4"));
    let b1 = link("b", 1, &genesis(VAULT, "b"), md(9, "b", T0 + 8, &[("a", 2)], "memo-s2"));
    let many = |ss: &[&Segment]| Feed::Many(ss.iter().map(|s| (*s).clone()).collect());
    let expected = project_all(&pubs_of([&s1, &s2, &s3, &s4, &x3, &y2, &y3, &y4, &b1]));
    let full = [one(&s1), many(&[&s2, &y2]), many(&[&s3, &x3, &y3]), many(&[&s4, &y4]), one(&b1)];

    let mut r = device("r");
    let mut seen = Vec::new();
    for step in [
        one(&s1),
        one(&s2),
        many(&[&s3, &x3]),
        one(&b1),
        one(&y4),
        many(&[&s3, &x3, &y3]),
        one(&s4),
        many(&[&s2, &y2]),
    ] {
        seen.push(feed(&mut r, &step));
    }
    assert!(matches!(seen[4], ApplyOutcome::Deferred { .. }), "y4 before y3: {:?}", seen[4]);
    assert!(matches!(seen[5], ApplyOutcome::Deferred { .. }), "y3 before y2: {:?}", seen[5]);
    assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]), "fork_start moved down");
    for step in &full {
        feed(&mut r, step);
    }
    let reference = run(&full);
    for (name, s) in [("interleaved", &r), ("in order", &reference)] {
        assert_eq!(hash(s), expected.hash, "{name}");
        assert_eq!(op_ids(s).len(), 9, "{name}");
        assert_eq!(s.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 2)]), "{name}");
        assert!(s.quarantine_rows(VAULT).unwrap().is_empty(), "{name}");
        assert!(store_restored(s, Kind::Memo, "memo-s2"), "{name}: the delete saw (a,2) only under the old fork_start");
    }
}

// ---------------------------------------------------- trim predicate (p3)

/// Every exclusion of the one deletable predicate, both on the offer and on
/// forget_segments (each refusal changes nothing): rejected, attempted,
/// lagging reader, unverified (revokes), snapshot of another vault,
/// forked after the offer, retired after the offer.
#[test]
fn g3_p3_p1_trim_predicate_exclusions() {
    let offer = |a: &Store, cursors: &[u64], verified: bool| {
        let snap = build_snapshot(a, VAULT).unwrap();
        assert!(verify_snapshot(a, VAULT, &snap).unwrap());
        segments_deletable(a, VAULT, "a", cursors, verified, &snap).unwrap()
    };
    let refused = |a: &mut Store, ranges: &[(u64, u64)], why: &str| {
        let before = a.dump_state().unwrap();
        assert!(a.forget_segments(VAULT, "a", ranges).is_err(), "{why}: must be refused");
        assert_eq!(a.dump_state().unwrap(), before, "{why}: a refusal changes nothing");
    };
    let mut remote = Remote::new(true);
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "one"));
    publish(&mut a, &mut remote).unwrap();
    local(&mut a, create(2, 2_000, K, "rejected"));
    seal_next(&mut a).unwrap();
    a.mark_attempted(VAULT, "a", 2).unwrap();
    a.mark_rejected(VAULT, "a", 2).unwrap();
    local(&mut a, create(3, 3_000, "memo-3", "attempted"));
    seal_next(&mut a).unwrap();
    a.mark_attempted(VAULT, "a", 3).unwrap();
    local(&mut a, create(4, 4_000, "memo-4", "four"));
    publish(&mut a, &mut remote).unwrap();
    assert_eq!(a.op_state(VAULT, &oid(2)).unwrap().unwrap().2, UploadState::Rejected);

    assert_eq!(offer(&a, &[u64::MAX], true), vec![(1, 1), (4, 4)], "rejected and attempted are never offered");
    refused(&mut a, &[(2, 2)], "rejected");
    refused(&mut a, &[(3, 3)], "attempted");
    refused(&mut a, &[(1, 1), (2, 2)], "a batch with a rejected range");

    assert!(offer(&a, &[0], true).is_empty(), "a reader at 0 blocks everything");
    refused(&mut a, &[(1, 1)], "lagging reader (offer replaced)");
    assert_eq!(offer(&a, &[u64::MAX, 1], true), vec![(1, 1)], "the slowest reader bounds the offer");
    refused(&mut a, &[(4, 4)], "beyond the slowest reader");

    assert_eq!(offer(&a, &[u64::MAX], true), vec![(1, 1), (4, 4)]);
    assert!(offer(&a, &[u64::MAX], false).is_empty(), "unverified offers nothing");
    refused(&mut a, &[(1, 1)], "unverified pass revoked the offer");

    let mut other = build_snapshot(&a, VAULT).unwrap();
    other.vault_id = V2.to_owned();
    assert!(segments_deletable(&a, VAULT, "a", &[u64::MAX], true, &other).unwrap().is_empty(), "another vault's snapshot");
    refused(&mut a, &[(4, 4)], "snapshot of another vault");

    // Forked after the offer.
    assert_eq!(offer(&a, &[u64::MAX], true), vec![(1, 1), (4, 4)]);
    let mine = open_segment(&seg_key(), &addr("a", 4, 4), &a.sealed_bytes(VAULT, "a", 4).unwrap().unwrap().bytes).unwrap();
    let mut theirs = op(40, 4_001, Kind::Memo, "memo-40", "create", json!({"text": "clone", "pinned": false}));
    theirs.origin_device = "a".to_owned();
    let theirs = link("a", 4, &mine.prev_seg_hash, theirs);
    let out = a.apply_fork_variant(VAULT, "a", 4, 4, &[mine, theirs]).unwrap();
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
    assert_eq!(a.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 4)]));
    refused(&mut a, &[(1, 1)], "own stream forked after the offer");
    assert!(offer(&a, &[u64::MAX], true).is_empty(), "a forked own stream is never offered");

    // Retired after the offer (P published 1-1, X rejected 2-2).
    let (mut r, hashes) = rotation_world();
    let snap = build_snapshot(&r, VAULT).unwrap();
    assert!(verify_snapshot(&r, VAULT, &snap).unwrap());
    assert_eq!(segments_deletable(&r, VAULT, "a", &[u64::MAX], true, &snap).unwrap(), vec![(1, 1)]);
    let plan = plan_for(&r, &hashes, "a2");
    r.apply_rotation(VAULT, &plan, &hashes, &identity("a2")).unwrap();
    refused(&mut r, &[(1, 1)], "retired after the offer");
    let snap = build_snapshot(&r, VAULT).unwrap();
    assert!(segments_deletable(&r, VAULT, "a", &[u64::MAX], true, &snap).unwrap().is_empty(), "retired id");
}

/// The offer is remembered as a limit, not as the set of ranges. A range
/// that became `published` after the offer but lies within the limit
/// (frontier of the verified snapshot, every active reader) is accepted
/// without a new offer: it satisfies the leader's four conditions.
/// Observation: the doc comment "a range never offered ... is refused" is
/// stronger than the code; with an active reader below it, it is refused.
#[test]
fn g3_p3_p2_offer_is_a_limit_not_a_set() {
    let mut remote = Remote::new(true);
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "one"));
    publish(&mut a, &mut remote).unwrap();
    local(&mut a, create(2, 2_000, K, "two"));
    let snap = build_snapshot(&a, VAULT).unwrap();
    assert!(verify_snapshot(&a, VAULT, &snap).unwrap());
    assert_eq!(snap.frontier.get("a"), Some(&2), "the snapshot covers the unsealed op");
    assert_eq!(segments_deletable(&a, VAULT, "a", &[], true, &snap).unwrap(), vec![(1, 1)]);
    publish(&mut a, &mut remote).unwrap();
    assert_eq!(a.forget_segments(VAULT, "a", &[(2, 2)]).unwrap(), 1, "accepted: within the offered limit, now published");

    let mut remote_b = Remote::new(true);
    let mut b = device("a");
    local(&mut b, create(1, 1_000, M, "one"));
    publish(&mut b, &mut remote_b).unwrap();
    local(&mut b, create(2, 2_000, K, "two"));
    let snap = build_snapshot(&b, VAULT).unwrap();
    assert_eq!(segments_deletable(&b, VAULT, "a", &[1], true, &snap).unwrap(), vec![(1, 1)]);
    publish(&mut b, &mut remote_b).unwrap();
    let before = b.dump_state().unwrap();
    assert!(b.forget_segments(VAULT, "a", &[(2, 2)]).is_err(), "a reader below 2 bounds the limit");
    assert_eq!(b.dump_state().unwrap(), before);
}

// ------------------------------------------------ fault classification (p3)

/// Local damage (an op row, the cursor's head or state, a forked stream's
/// chain_nodes, an unreadable integer) during a valid remote apply is a
/// local fault: Err, no quarantine row, the cursor and every row untouched.
/// A duplicate redelivery is not affected; a remote fault on top of local
/// damage still blames the remote object (schema precedes local loading).
#[test]
fn g3_p3_c1_local_corruption_is_err_never_quarantine_and_stream_untouched() {
    let a = chain_of(
        "a",
        vec![mc(1, "a", T0, &[], M, "base"), me(2, "a", T0 + 1, &[("a", 1)], M, 1, "x"), mc(3, "a", T0 + 2, &[("a", 2)], K, "k")],
    );
    let fresh = || run(&[one(&a[0]), one(&a[1])]);
    let damages: [(&str, &str); 6] = [
        ("op ctx not json", "UPDATE ops SET ctx_frontier = 'not json' WHERE op_id = '{OP1}'"),
        ("op payload float", "UPDATE ops SET payload = CAST('{\"pinned\":false,\"text\":1.5}' AS BLOB) WHERE op_id = '{OP1}'"),
        ("op hlc negative", "UPDATE ops SET hlc_wall = -1 WHERE op_id = '{OP1}'"),
        ("op kind unknown", "UPDATE ops SET entity_kind = 'bogus' WHERE op_id = '{OP1}'"),
        ("cursor head short", "UPDATE cursors SET chain_head = X'0102' WHERE remote_device_id = 'a'"),
        ("cursor state unknown", "UPDATE cursors SET state = 'bogus' WHERE remote_device_id = 'a'"),
    ];
    for (name, sql) in damages {
        let mut r = fresh();
        r.conn().execute(&sql.replace("{OP1}", &oid(1)), []).unwrap();
        let before = r.dump_state().unwrap();
        let cursor_row = dump_rows(&r, "SELECT * FROM cursors");
        let err = r.apply_sealed(VAULT, &seg_key(), &addr("a", 3, 3), &sealed(&a[2]));
        assert!(err.is_err(), "{name}: {err:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{name}: the remote object is not blamed");
        assert_eq!(dump_rows(&r, "SELECT * FROM cursors"), cursor_row, "{name}: the stream is untouched");
        assert_eq!(r.dump_state().unwrap(), before, "{name}: rolled back");
        let err = r.apply_fork_variant(VAULT, "a", 3, 3, &[a[2].clone(), link("a", 3, &head(&a[1]), mc(9, "a", T0 + 9, &[("a", 2)], "memo-9", "nine"))]);
        assert!(err.is_err(), "{name}: fork variant {err:?}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{name}");
        assert_eq!(r.dump_state().unwrap(), before, "{name}");
        if !name.starts_with("cursor") {
            // A redelivery of an applied segment never touches op rows.
            let dup = r.apply_sealed(VAULT, &seg_key(), &addr("a", 2, 2), &sealed(&a[1]));
            assert!(matches!(dup, Ok(ApplyOutcome::Duplicate)), "{name}: {dup:?}");
            // Remote fault on top of local damage: the remote object is blamed.
            let mut bad = a[2].clone();
            bad.ops[0].body.origin_device = "zz".to_owned();
            let out = r.apply_segment(VAULT, "a", 3, 3, &bad).unwrap();
            assert!(matches!(out, ApplyOutcome::Held { .. }), "{name}: {out:?}");
            assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
            // A local insert on a damaged store is Err and writes nothing.
            let before = r.dump_state().unwrap();
            let local_err = r.insert_local_op(VAULT, create(70, 7_000, "memo-70", "l"), UploadState::Never);
            assert!(local_err.is_err(), "{name}: {local_err:?}");
            assert_eq!(r.dump_state().unwrap(), before, "{name}: the local insert rolled back");
        }
    }

    // Forked stream with a damaged chain_nodes head.
    let w = lag_world();
    let mut r = run(&[Feed::Many(w.v.to_vec()), one(&w.c[0])]);
    let damaged = "UPDATE chain_nodes SET head = X'010203' WHERE rowid = (SELECT MIN(rowid) FROM chain_nodes WHERE device_id = 'a')";
    assert_eq!(r.conn().execute(damaged, []).unwrap(), 1);
    let before = r.dump_state().unwrap();
    let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 3, 3), &sealed(&w.d[0]));
    assert!(out.is_err(), "forked continuation on damaged chain_nodes: {out:?}");
    let out = r.apply_fork_variant(VAULT, "a", 3, 3, &w.d);
    assert!(out.is_err(), "fork variant on damaged chain_nodes: {out:?}");
    assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
    assert_eq!(r.dump_state().unwrap(), before);
}

/// Remote faults on every path (single, forked continuation, fork variants):
/// undecryptable → quarantine only (cursor untouched); schema/origin/seq →
/// quarantine + held; gap → held without quarantine; nothing applied.
#[test]
fn g3_p3_c2_remote_faults_quarantine_or_hold() {
    let a = chain_of("a", vec![mc(1, "a", T0, &[], M, "base"), mc(2, "a", T0 + 1, &[("a", 1)], K, "k")]);
    let w = lag_world();
    let forked = || run(&[Feed::Many(w.v.to_vec()), one(&w.c[0])]);
    let plain = || run(&[one(&a[0])]);
    let ops_rows = |s: &Store| dump_rows(s, "SELECT * FROM ops") .join("\n") + &dump_rows(s, "SELECT * FROM op_dots").join("\n");

    // Undecryptable bytes: quarantine only, cursor bytes unchanged.
    for (name, mut r, seq, seg) in [("plain", plain(), 2u64, a[1].clone()), ("forked", forked(), 3, w.d[0].clone())] {
        let cursor = dump_rows(&r, "SELECT * FROM cursors");
        let mut bytes = sealed(&seg);
        bytes[45] ^= 0x10;
        let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", seq, seq), &bytes).unwrap();
        assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "{name}: {out:?}");
        assert_eq!(dump_rows(&r, "SELECT * FROM cursors"), cursor, "{name}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
    }

    type Bad = fn(&mut Segment);
    let bads: [(&str, Bad); 3] = [
        ("foreign origin", |s| s.ops[0].body.origin_device = "zz".to_owned()),
        ("bad entity id", |s| s.ops[0].body.entity_id = "not an id".to_owned()),
        ("ctx beyond i64", |s| s.ops[0].body.ctx = ctx_of(&[("a", u64::MAX)])),
    ];
    for (name, bad) in bads {
        // Plain stream.
        let mut r = plain();
        let rows = ops_rows(&r);
        let mut s = a[1].clone();
        bad(&mut s);
        let out = r.apply_segment(VAULT, "a", 2, 2, &s).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} plain: {out:?}");
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held, "{name}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
        assert_eq!(ops_rows(&r), rows, "{name}: nothing applied");
        // Forked continuation of a known head.
        let mut r = forked();
        let rows = ops_rows(&r);
        let mut s = w.d[0].clone();
        bad(&mut s);
        let out = r.apply_segment(VAULT, "a", 3, 3, &s).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} forked: {out:?}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
        assert_eq!(ops_rows(&r), rows, "{name}: nothing applied");
        // Fork variants, one bad.
        let mut r = forked();
        let rows = ops_rows(&r);
        let mut s = w.d[0].clone();
        bad(&mut s);
        let sibling = link("a", 3, &head(&w.c[0]), mc(80, "a", T0 + 80, &[("a", 2)], "memo-80", "sib"));
        let out = r.apply_fork_variant(VAULT, "a", 3, 3, &[s, sibling]).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} variants: {out:?}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
        assert_eq!(ops_rows(&r), rows, "{name}: no variant applied");
    }

    // A gap holds without blaming the next object.
    let mut r = device("r");
    let out = feed(&mut r, &one(&a[1]));
    assert!(matches!(out, ApplyOutcome::Held { .. }), "{out:?}");
    assert!(r.quarantine_rows(VAULT).unwrap().is_empty());
}

// ------------------------------------------------------------ ctx bounds (p3)

/// ctx values: i64::MAX is stored verbatim and round-trips through the
/// store and a verified snapshot; anything above (one entry among several,
/// u64::MAX, an alias of a stored op) fails validation and is quarantined +
/// held on every path, and the stored op's ctx is not widened.
#[test]
fn g3_p3_i1_ctx_bounds() {
    let max = i64::MAX as u64;
    let mut ok = mc(1, "a", T0, &[], M, "x");
    ok.ctx = ctx_of(&[("b", max), ("c", 1)]);
    assert_eq!(ok.validate(), Ok(()));
    let mut r = device("r");
    let s1 = link("a", 1, &genesis(VAULT, "a"), ok.clone());
    assert!(matches!(feed(&mut r, &one(&s1)), ApplyOutcome::Applied { .. }));
    assert_eq!(r.load_set(VAULT).unwrap().0.get(&oid(1)).unwrap().body.ctx, ok.ctx, "stored verbatim");
    let snap = build_snapshot(&r, VAULT).unwrap();
    assert!(verify_snapshot(&r, VAULT, &snap).unwrap());
    assert_eq!(restore_equivalent(&snap).unwrap().hash, hash(&r));
    let mut tampered = snap.clone();
    tampered.ops[0].body.ctx.insert("b".to_owned(), u64::MAX);
    assert!(!verify_snapshot(&r, VAULT, &tampered).unwrap_or(false), "an out-of-range snapshot never verifies");
    assert!(catch_unwind(AssertUnwindSafe(|| restore_equivalent(&tampered))).is_ok(), "restore never panics");

    for (name, ctx) in [
        ("max + 1", ctx_of(&[("b", max + 1)])),
        ("u64::MAX", ctx_of(&[("b", u64::MAX)])),
        ("one of several", ctx_of(&[("a", 1), ("b", max), ("c", max + 7)])),
    ] {
        let mut b = mc(2, "a", T0 + 1, &[], K, "y");
        b.ctx = ctx;
        assert!(matches!(b.validate(), Err(OpError::Range(_))), "{name}");
        // Plain stream.
        let mut r2 = device("r");
        let out = feed(&mut r2, &one(&link("a", 1, &genesis(VAULT, "a"), b.clone())));
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name}: {out:?}");
        assert_eq!(r2.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
        assert!(op_ids(&r2).is_empty(), "{name}");
        // Alias of a stored op: same op id and content, wider ctx.
        let mut alias = ok.clone();
        alias.origin_device = "a2".to_owned();
        alias.ctx = b.ctx.clone();
        let mut r3 = run(&[one(&s1)]);
        let out = feed(&mut r3, &one(&link("a2", 1, &genesis(VAULT, "a2"), alias)));
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} alias: {out:?}");
        assert_eq!(r3.load_set(VAULT).unwrap().0.get(&oid(1)).unwrap().body.ctx, ok.ctx, "{name}: ctx not widened");
        assert_eq!(r3.dots_of(VAULT, &oid(1)).unwrap(), vec![dot("a", 1)], "{name}: no alias dot");
        // Forked stream continuation and fork variants.
        let w = lag_world();
        let mut r4 = run(&[Feed::Many(w.v.to_vec())]);
        let mut cont = mc(2, "a", T0 + 2, &[], K, "y");
        cont.ctx = b.ctx.clone();
        let out = feed(&mut r4, &one(&link("a", 2, &head(&w.v[0]), cont.clone())));
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} forked: {out:?}");
        assert!(!op_ids(&r4).contains(&oid(2)), "{name}");
        let mut r5 = run(&[Feed::Many(w.v.to_vec())]);
        let out = feed(&mut r5, &Feed::Many(vec![link("a", 2, &head(&w.v[0]), cont), w.c[1].clone()]));
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} variants: {out:?}");
        assert!(!op_ids(&r5).contains(&oid(2)) && !op_ids(&r5).contains(&oid(23)), "{name}");
        assert_eq!(r5.quarantine_rows(VAULT).unwrap().len(), 1, "{name}");
    }
}

// ======================================================================
// Pass 4: attacks on the post-pass-3 fixes (partial fork apply, the
// deletable count, orphan-chain fate, pin-base edits, MAX_STORED).
// ======================================================================

/// a forked three ways at seq 1 (chain 0 original, chains 1 and 2 clones);
/// every chain continues at seqs 2..4 (seq 4 edits the chain's seq-1 memo).
/// b blindly deletes memo-k0 having seen (a,4).
struct Tri {
    chains: Vec<Vec<Segment>>,
    b1: Segment,
}

fn tri_world() -> Tri {
    let g = genesis(VAULT, "a");
    let mut chains = Vec::new();
    for k in 0..3u64 {
        let mut prev = g.clone();
        let mut chain = Vec::new();
        for seq in 1..=4u64 {
            let n = 100 + k * 10 + seq;
            let wall = T0 + n;
            let body = match seq {
                1 => mc(n, "a", wall, &[], &format!("memo-k{k}"), &format!("k{k} create")),
                4 => me(n, "a", wall, &[("a", 3)], &format!("memo-k{k}"), n - 3, &format!("k{k} edited")),
                _ => mc(n, "a", wall, &[("a", seq - 1)], &format!("memo-k{k}s{seq}"), &format!("k{k} s{seq}")),
            };
            let s = link("a", seq, &prev, body);
            prev = head(&s);
            chain.push(s);
        }
        chains.push(chain);
    }
    let b1 = link("b", 1, &genesis(VAULT, "b"), md(190, "b", T0 + 190, &[("a", 4)], "memo-k0"));
    Tri { chains, b1 }
}

impl Tri {
    fn seg(&self, k: usize, seq: u64) -> &Segment {
        &self.chains[k][seq as usize - 1]
    }

    /// The name `a/seq-seq` listing the variants of `ks`, in that order.
    fn name(&self, seq: u64, ks: &[usize]) -> Feed {
        let vs: Vec<Segment> = ks.iter().map(|&k| self.seg(k, seq).clone()).collect();
        if vs.len() == 1 {
            Feed::One(vs[0].clone())
        } else {
            Feed::Many(vs)
        }
    }

    fn pubs(&self, skip: &[(usize, u64)]) -> Vec<Pub> {
        let mut segs: Vec<&Segment> = Vec::new();
        for k in 0..3 {
            for seq in 1..=4u64 {
                if !skip.contains(&(k, seq)) {
                    segs.push(self.seg(k, seq));
                }
            }
        }
        segs.push(&self.b1);
        pubs_of(segs)
    }

    fn op(&self, k: usize, seq: u64) -> String {
        self.seg(k, seq).ops[0].body.op_id.clone()
    }
}

/// One engine step with the forked-lag invariants: never held or
/// quarantined, a deferral writes nothing, and after every step the store
/// equals the pure projection of what it holds.
fn lag_step(r: &mut Store, step: &Feed, label: &str) -> ApplyOutcome {
    let before = dump_all(r, &[]);
    let out = feed(r, step);
    match &out {
        ApplyOutcome::Deferred { .. } => assert_eq!(dump_all(r, &[]), before, "{label}: a deferral writes nothing"),
        ApplyOutcome::Applied { .. } | ApplyOutcome::Duplicate => {}
        other => panic!("{label}: {other:?}"),
    }
    let (set, forks) = r.load_set(VAULT).unwrap();
    assert_eq!(project(&set, &forks).unwrap().hash, hash(r), "{label}: store = projection of its op set");
    out
}

/// Random relists: each cycle visits the names in a random order, and each
/// name lists a random non-empty subset of the readable variants in a
/// random order. A final in-order full cycle guarantees completeness.
fn random_schedule(t: &Tri, seed: u64, cycles: usize, chains: &[usize]) -> Vec<Feed> {
    let mut rng = Lcg(seed);
    let mut out = vec![t.name(1, &[chains[seed as usize % chains.len()], chains[(seed as usize + 1) % chains.len()]])];
    for _ in 0..cycles {
        let mut names: Vec<u64> = (1..=4).collect();
        rng.shuffle(&mut names);
        for seq in names {
            let mut ks: Vec<usize> = chains.to_vec();
            rng.shuffle(&mut ks);
            let keep = 1 + (rng.step() as usize % ks.len());
            out.push(t.name(seq, &ks[..keep]));
        }
        if rng.step() % 2 == 0 {
            out.push(one(&t.b1));
        }
    }
    for seq in 1..=4 {
        out.push(t.name(seq, chains));
    }
    out.push(one(&t.b1));
    out
}

/// f3 fix, three readers and more: names list attaching and non-attaching
/// variants together in different orders (a clone's seq-1 object lagging,
/// newest first, random subsets). Every reader converges on the in-order
/// reader: hash, op set, forks, chain nodes, no quarantine, never held.
#[test]
fn g3_p4_f1_partial_fork_apply_three_readers_converge() {
    let t = tri_world();
    let full = [0, 1, 2];
    let mut in_order: Vec<Feed> = (1..=4).map(|seq| t.name(seq, &full)).collect();
    in_order.push(one(&t.b1));
    let reference = run(&in_order);
    let expected = project_all(&t.pubs(&[]));
    assert_eq!(hash(&reference), expected.hash);
    assert_eq!(op_ids(&reference).len(), 13);

    // Reader 1: v2 (chain 2 at seq 1) is listed one cycle late.
    let mut r1 = device("r1");
    lag_step(&mut r1, &t.name(1, &[0, 1]), "r1 c1 name 1");
    for seq in 2..=4u64 {
        let out = lag_step(&mut r1, &t.name(seq, &[2, 0, 1]), &format!("r1 c1 name {seq}"));
        assert!(matches!(out, ApplyOutcome::Applied { .. }), "r1 name {seq}: the attaching siblings apply: {out:?}");
        let ids = op_ids(&r1);
        assert!(ids.contains(&t.op(0, seq)) && ids.contains(&t.op(1, seq)), "r1 name {seq}");
        assert!(!ids.contains(&t.op(2, seq)), "r1 name {seq}: chain 2 is deferred");
    }
    lag_step(&mut r1, &one(&t.b1), "r1 b1");
    // A later segment of the deferred chain, alone, stays deferred.
    for seq in [4u64, 3, 2] {
        let out = lag_step(&mut r1, &one(t.seg(2, seq)), &format!("r1 lone chain-2 seq {seq}"));
        assert!(matches!(out, ApplyOutcome::Deferred { .. }), "r1 lone seq {seq}: {out:?}");
    }
    // Cycle 2: v2 arrives; its children attach after their siblings applied.
    lag_step(&mut r1, &t.name(1, &full), "r1 c2 name 1");
    let out = lag_step(&mut r1, &one(t.seg(2, 3)), "r1 d2 before c2");
    assert!(matches!(out, ApplyOutcome::Deferred { .. }), "{out:?}");
    for seq in 2..=4u64 {
        let out = lag_step(&mut r1, &t.name(seq, &full), &format!("r1 c2 name {seq}"));
        assert!(matches!(out, ApplyOutcome::Applied { .. }), "r1 c2 name {seq}: the late variant attaches: {out:?}");
        assert!(op_ids(&r1).contains(&t.op(2, seq)), "r1 c2 name {seq}");
    }

    // Reader 2: forked by v1/v2 only, then newest name first with variants
    // reversed; v0's chain trails one name per cycle, so four cycles.
    let mut r2 = device("r2");
    lag_step(&mut r2, &t.name(1, &[1, 2]), "r2 fork");
    for cycle in 0..4 {
        lag_step(&mut r2, &one(&t.b1), "r2 b1");
        for seq in (1..=4u64).rev() {
            lag_step(&mut r2, &t.name(seq, &[2, 1, 0]), &format!("r2 cycle {cycle} name {seq}"));
        }
    }

    // Reader 3: chain 2 attaches only through lone objects after chain 1
    // was listed alone under each name.
    let mut r3 = device("r3");
    lag_step(&mut r3, &t.name(1, &[1, 0]), "r3 name 1");
    for seq in 2..=4u64 {
        lag_step(&mut r3, &t.name(seq, &[1, 2]), &format!("r3 name {seq} (1, 2)"));
    }
    for seq in 2..=4u64 {
        lag_step(&mut r3, &one(t.seg(0, seq)), &format!("r3 lone chain-0 seq {seq}"));
    }
    lag_step(&mut r3, &one(&t.b1), "r3 b1");
    lag_step(&mut r3, &t.name(1, &[2, 0]), "r3 v2 with v0");
    for seq in 2..=4u64 {
        lag_step(&mut r3, &one(t.seg(2, seq)), &format!("r3 lone chain-2 seq {seq}"));
    }

    // Readers 4..: random relists.
    let mut readers = vec![("r1", r1), ("r2", r2), ("r3", r3)];
    for seed in 1..=6u64 {
        let mut r = device("rx");
        for (i, step) in random_schedule(&t, seed, 6, &full).iter().enumerate() {
            lag_step(&mut r, step, &format!("random seed {seed} step {i}"));
        }
        readers.push(("random", r));
    }
    for (name, r) in &readers {
        assert_eq!(op_ids(r), op_ids(&reference), "{name}");
        assert_eq!(hash(r), expected.hash, "{name}");
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]), "{name}");
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked, "{name}");
        assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "{name}");
        assert_eq!(nodes(r, "a"), nodes(&reference, "a"), "{name}");
        assert_eq!(r.projection(VAULT).unwrap().notices, expected.notices, "{name}");
    }
}

/// f3 with an unreadable object: chain 2's c2 never decrypts, so d2 and e2
/// never attach. Readers in three different orders apply every attaching
/// sibling, quarantine only c2, stay forked (never held), and agree.
#[test]
fn g3_p4_f2_unreadable_variant_readers_agree_on_the_attaching_rest() {
    let t = tri_world();
    let mut bad_c2 = sealed(t.seg(2, 2));
    let last = bad_c2.len() - 1;
    bad_c2[last] ^= 1;
    let bad = |r: &mut Store| {
        let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", 2, 2), &bad_c2).unwrap();
        assert!(matches!(out, ApplyOutcome::Quarantined { .. }), "{out:?}");
    };
    let expected = project_all(&t.pubs(&[(2, 2), (2, 3), (2, 4)]));
    let full = [0, 1, 2];

    let mut x1 = device("x1");
    lag_step(&mut x1, &t.name(1, &full), "x1 name 1");
    bad(&mut x1);
    lag_step(&mut x1, &t.name(2, &[0, 1]), "x1 name 2");
    for seq in 3..=4u64 {
        let out = lag_step(&mut x1, &t.name(seq, &full), &format!("x1 name {seq}"));
        assert!(matches!(out, ApplyOutcome::Applied { .. }), "x1 name {seq}: {out:?}");
    }
    lag_step(&mut x1, &one(&t.b1), "x1 b1");

    let mut x2 = device("x2");
    lag_step(&mut x2, &t.name(1, &full), "x2 fork");
    for cycle in 0..3 {
        for seq in (1..=4u64).rev() {
            if seq == 2 {
                bad(&mut x2);
                lag_step(&mut x2, &t.name(2, &[1, 0]), &format!("x2 cycle {cycle} name 2"));
            } else {
                lag_step(&mut x2, &t.name(seq, &[2, 1, 0]), &format!("x2 cycle {cycle} name {seq}"));
            }
        }
        lag_step(&mut x2, &one(&t.b1), "x2 b1");
    }

    let mut x3 = device("x3");
    for (i, step) in random_schedule(&t, 77, 6, &full).iter().enumerate() {
        // The engine never hands an unreadable object to apply_* as a variant.
        let step = match step {
            Feed::Many(vs) if vs[0].first_seq == 2 => {
                bad(&mut x3);
                let rest: Vec<Segment> = vs.iter().filter(|v| v.ops[0].body.op_id != t.op(2, 2)).cloned().collect();
                match rest.len() {
                    0 => continue,
                    1 => Feed::One(rest[0].clone()),
                    _ => Feed::Many(rest),
                }
            }
            Feed::One(s) if s.first_seq == 2 && s.ops[0].body.op_id == t.op(2, 2) => {
                bad(&mut x3);
                continue;
            }
            other => other.clone(),
        };
        lag_step(&mut x3, &step, &format!("x3 step {i}"));
    }

    for (name, r) in [("x1", &x1), ("x2", &x2), ("x3", &x3)] {
        assert_eq!(hash(r), expected.hash, "{name}");
        let ids = op_ids(r);
        assert_eq!(ids.len(), 10, "{name}");
        assert!(!ids.contains(&t.op(2, 3)) && !ids.contains(&t.op(2, 4)), "{name}: d2 / e2 never attach");
        assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]), "{name}");
        assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked, "{name}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name}: only c2's object");
        assert_eq!(nodes(r, "a"), nodes(&x1, "a"), "{name}");
    }
}

/// Partial apply edge cases: a variant whose sibling chain is attached but
/// which itself shares no parent; a name whose only attaching variant is
/// already applied (Deferred, nothing written, fork_start untouched); and a
/// mixed name on a stream that is not forked yet (the simple-break rule:
/// held + quarantined, visibly), which a later fork listing recovers.
#[test]
fn g3_p4_f3_partial_apply_edges() {
    let t = tri_world();
    // Only-held attaching sibling: nothing new to apply.
    let mut r = run(&[t.name(1, &[0, 1]), t.name(2, &[0, 1])]);
    let before = dump_all(&r, &[]);
    let out = feed(&mut r, &t.name(2, &[1, 2, 0]));
    assert!(matches!(out, ApplyOutcome::Deferred { .. }), "{out:?}");
    assert_eq!(dump_all(&r, &[]), before);
    // The non-attaching variant is deferred even if it would fail schema.
    let mut forged = t.seg(2, 3).clone();
    forged.ops[0].body.origin_device = "zz".to_owned();
    let out = feed(&mut r, &Feed::Many(vec![t.seg(0, 3).clone(), forged.clone()]));
    assert!(matches!(out, ApplyOutcome::Applied { .. }), "{out:?}");
    assert!(op_ids(&r).contains(&t.op(0, 3)) && r.quarantine_rows(VAULT).unwrap().is_empty());
    assert_eq!(r.forks(VAULT).unwrap(), BTreeMap::from([("a".to_owned(), 1)]));

    // Not forked yet: the reader applied v0 and c0 alone (v1 not listed),
    // then name 2 lists c0 with c1 whose parent v1 it never saw.
    let mut r = run(&[one(t.seg(0, 1)), one(t.seg(0, 2))]);
    let out = feed(&mut r, &t.name(2, &[0, 1]));
    assert!(matches!(out, ApplyOutcome::Held { .. }), "simple-break rule on a non-forked stream: {out:?}");
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held);
    assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "visible, not silent");
    // The full relist reveals the fork at 1 and the stream recovers.
    for seq in 1..=4u64 {
        feed(&mut r, &t.name(seq, &[0, 1, 2]));
    }
    feed(&mut r, &one(&t.b1));
    assert_eq!(hash(&r), project_all(&t.pubs(&[])).hash, "recovered from held by the fork listing");
    assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Forked);
}

// ------------------------------------------------- deletable count (p4)

/// The deletable predicate counts published seqs: a sealed range with a
/// missing dot (first, middle, last), a dot whose op row is gone, or one
/// op not `published` is never offered and forget_segments refuses it,
/// changing nothing. The intact store offers it.
#[test]
fn g3_p4_p1_deletable_requires_every_seq_present_and_published() {
    let build = || {
        let mut remote = Remote::new(true);
        let mut a = device("a");
        local(&mut a, create(1, 1_000, M, "one"));
        local(&mut a, create(2, 2_000, K, "two"));
        local(&mut a, edit(3, 3_000, M, 1, "one'"));
        publish(&mut a, &mut remote).unwrap();
        local(&mut a, create(4, 4_000, "memo-4", "four"));
        publish(&mut a, &mut remote).unwrap();
        a
    };
    let a = build();
    assert_eq!(a.pending_segments(VAULT, "a").unwrap().iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(), vec![(1, 3), (4, 4)]);
    assert_eq!(a.offer_deletable(VAULT, "a", u64::MAX).unwrap(), vec![(1, 3), (4, 4)], "intact");
    // (An op row cannot go while its dot exists: op_dots has a foreign key.)
    let damages: [(&str, &str); 5] = [
        ("first dot missing", "DELETE FROM op_dots WHERE device_id = 'a' AND seq = 1"),
        ("middle dot missing", "DELETE FROM op_dots WHERE device_id = 'a' AND seq = 2"),
        ("last dot missing", "DELETE FROM op_dots WHERE device_id = 'a' AND seq = 3"),
        ("all dots missing", "DELETE FROM op_dots WHERE device_id = 'a' AND seq BETWEEN 1 AND 3"),
        (
            "one op attempted",
            "UPDATE ops SET upload_state = 'attempted' WHERE op_id = (SELECT op_id FROM op_dots WHERE device_id = 'a' AND seq = 3)",
        ),
    ];
    for (name, sql) in damages {
        let mut a = build();
        a.conn().execute_batch(sql).unwrap();
        assert_eq!(a.offer_deletable(VAULT, "a", u64::MAX).unwrap(), vec![(4, 4)], "{name}: only the intact range");
        let before = a.dump_state().unwrap();
        assert!(a.forget_segments(VAULT, "a", &[(1, 3)]).is_err(), "{name}: refused");
        assert!(a.forget_segments(VAULT, "a", &[(4, 4), (1, 3)]).is_err(), "{name}: a batch with it is refused");
        assert_eq!(a.dump_state().unwrap(), before, "{name}: a refusal changes nothing");
        assert_eq!(a.forget_segments(VAULT, "a", &[(4, 4)]).unwrap(), 1, "{name}: the intact range still goes");
    }
    // A pending row whose range is reversed or empty is never offered.
    let mut a = build();
    a.conn().execute_batch("UPDATE pending_segments SET last_seq = 0 WHERE first_seq = 4").unwrap();
    assert_eq!(a.offer_deletable(VAULT, "a", u64::MAX).unwrap(), vec![(1, 3)]);
    assert!(a.forget_segments(VAULT, "a", &[(4, 0)]).is_err());
}

// ------------------------------------------------- orphan chains (p4)

/// Two orphan chains on missing bases (A wins, B loses) plus the memo's own
/// lineage (y loses): every head text is visible. Blind deletes: one that
/// saw the shown lineage (c + chain A) deletes the memo while the copies
/// stay; one that missed a chain-A op restores it; one that saw only chain
/// B restores it. Every case in every order, and again once the bases
/// arrive.
#[test]
fn g3_p4_o1_orphan_chain_blind_deletes() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let y = at(me(2, "d", 500, &[("b", 1)], M, 1, "y"), 1);
    let oa1 = at(me(3, "o", 600, &[], M, 98, "a1"), 1);
    let oa2 = at(me(4, "o", 950, &[("o", 1)], M, 3, "a2"), 2);
    let ob1 = at(me(5, "p", 250, &[], M, 99, "b1"), 1);
    let ob2 = at(me(6, "p", 300, &[("p", 1)], M, 5, "b2"), 2);
    let world = vec![c, y, oa1, oa2, ob1, ob2];
    let p = order_free(&world, 300, 41);
    assert_eq!(memo_of(&p, M).unwrap().text, "a2");
    let y_copy = conflict_copy_id(M, &oid(2));
    let b_copy = conflict_copy_id(M, &oid(6));
    assert_eq!(memo_of(&p, &y_copy).unwrap().text, "y");
    assert_eq!(memo_of(&p, &b_copy).unwrap().text, "b2");
    for (op, want) in [(1, M), (3, M), (4, M)] {
        assert_eq!(p.memo_lineage[&oid(op)], want, "op {op}");
    }
    assert_eq!((p.memo_lineage[&oid(5)].as_str(), p.memo_lineage[&oid(6)].as_str()), (b_copy.as_str(), b_copy.as_str()));

    let del = |ctx: &[(&str, u64)]| at(md(9, "z", 2_000, ctx, M), 1);
    type Case<'a> = (&'a str, Vec<(&'a str, u64)>, bool);
    let cases: [Case; 4] = [
        ("saw c, y and chain A", vec![("b", 1), ("d", 1), ("o", 2)], true),
        ("saw c and chain A only", vec![("b", 1), ("o", 2)], true),
        ("missed a2", vec![("b", 1), ("d", 1), ("o", 1), ("p", 2)], false),
        ("saw chain B only", vec![("p", 2)], false),
    ];
    for (name, ctx, deleted) in &cases {
        let mut w = world.clone();
        w.push(del(ctx));
        let p = order_free(&w, 200, 43);
        assert_eq!(is_deleted(&p, Kind::Memo, M), *deleted, "{name}");
        assert_eq!(is_restored(&p, Kind::Memo, M), !*deleted, "{name}");
        assert_eq!(memo_of(&p, &y_copy).unwrap().text, "y", "{name}: copies are untouched");
        assert_eq!(memo_of(&p, &b_copy).unwrap().text, "b2", "{name}: copies are untouched");
        if !*deleted {
            assert_eq!(memo_of(&p, M).unwrap().text, "a2", "{name}");
        }
    }

    // The bases arrive: chains A and B become ordinary lineage.
    let x98 = at(me(98, "q", 200, &[("b", 1)], M, 1, "x98"), 1);
    let x99 = at(me(99, "q", 210, &[("b", 1), ("q", 1)], M, 98, "x99"), 2);
    let mut full = world.clone();
    full.extend([x98, x99]);
    let p = order_free(&full, 300, 47);
    let texts = memo_texts(&p);
    for t in ["a2", "b2", "y"] {
        assert!(texts.contains(t), "{t} visible once the bases arrive: {texts:?}");
    }
    for (name, ctx, _) in &cases {
        let mut w = full.clone();
        w.push(del(ctx));
        let p = order_free(&w, 150, 53);
        assert!(memo_texts(&p).contains("b2") || is_restored(&p, Kind::Memo, M), "{name}");
    }
}

/// Edits whose base names something other than a lineage op of the same
/// memo have unknown lineage, exactly like a missing base (architect
/// cleanup b): a pin, a note, a pin of another memo, a message op, a
/// translation op. A chain on a pin base loses and moves to its copy with
/// all its ops; a blind delete of the memo then deletes it.
#[test]
fn g3_p4_o2_pin_base_family_and_chain() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let pin = at(mp(2, "b", 150, &[("b", 1)], M, true), 2);
    let note = at(body(3, "b", 160, &[("b", 2)], (Kind::Memo, M, "note"), json!({"note": "n"})), 3);
    let y = at(me(4, "d", 300, &[("b", 1)], M, 1, "y"), 1);
    let kc = at(mc(10, "k", 50, &[], K, "kay"), 1);
    let kpin = at(mp(11, "k", 60, &[("k", 1)], K, true), 2);
    let m1 = at(msg(12, "k", &[("k", 2)], "m1", None, 1), 3);
    let tr = at(body(13, "k", 70, &[("k", 3)], (Kind::Tr, TRX, "tr.add"), json!({"src": "a", "dst": "b"})), 4);
    let base_world = vec![c, pin, note, y, kc, kpin, m1, tr];
    let odd = |base: u64| at(me(5, "a", 900, &[("b", 3)], M, base, "bad"), 1);
    let mut w = base_world.clone();
    w.push(odd(99));
    let missing = order_free(&w, 100, 61);
    for (name, base) in [("pin", 2), ("note", 3), ("pin of another memo", 11), ("message op", 12), ("translation op", 13)] {
        let mut w = base_world.clone();
        w.push(odd(base));
        let p = order_free(&w, 100, 67);
        assert_eq!(p.entities, missing.entities, "base on a {name}");
        assert_eq!(p.notices, missing.notices, "base on a {name}");
        assert_eq!(p.memo_lineage, missing.memo_lineage, "base on a {name}");
    }

    // A losing chain on a pin base: e1 (base pin) → e2; y wins on HLC.
    let e1 = at(me(6, "e", 200, &[("b", 2)], M, 2, "e1"), 1);
    let e2 = at(me(7, "e", 210, &[("b", 2), ("e", 1)], M, 6, "e2"), 2);
    let mut w = base_world.clone();
    w.extend([e1, e2]);
    let p = order_free(&w, 200, 71);
    let copy = conflict_copy_id(M, &oid(7));
    assert_eq!(memo_of(&p, M).unwrap().text, "y");
    assert_eq!(memo_of(&p, &copy).unwrap().text, "e2");
    assert_eq!((p.memo_lineage[&oid(6)].as_str(), p.memo_lineage[&oid(7)].as_str()), (copy.as_str(), copy.as_str()));
    let blind = at(md(8, "z", 1_000, &[("b", 3), ("d", 1)], M), 1);
    w.push(blind);
    let p = order_free(&w, 200, 73);
    assert!(is_deleted(&p, Kind::Memo, M), "the chain's text lives in the copy");
    assert_eq!(memo_of(&p, &copy).unwrap().text, "e2");
    assert_eq!(memo_of(&p, K).unwrap().text, "kay", "another memo is untouched");
}

/// The same class as a pin base, with a lineage op that is not this memo's
/// own: an edit naming itself as base, an edit naming another memo's
/// create, and two edits naming each other. Each must have unknown lineage
/// (like a missing base): never a root that pushes the memo's own lineage
/// into a copy, never routed into another memo, never invisible.
#[test]
fn g3_p4_o3a_self_based_edit_has_unknown_lineage() {
    odd_base_case(OddBase::SelfBase);
}

/// An edit of memo M whose base is memo K's create. This test first
/// asserted entity-based routing (an orphan of M, like a missing base). The
/// reversal follows the leader decision in `_sync/plan-amendments.md`
/// ("Memo lineage is structural", G3 delta 2): every entity-based override
/// of `base_op_id` broke honest copy-addressed edits (red-team p6-b1..b3),
/// so lineage is decided by the base chain alone. An edit that saw K's
/// create joins K's lineage: deterministic over every order, visible, M
/// untouched.
///
/// Re-baselined when the unrecorded causal-past filter was removed (G3
/// delta 3, architect b1, leader decision): an edit that names
/// `base_op_id` X has seen X by construction; causality across a fork range
/// is decided only by the edit-vs-delete `saw`/obs rule, not by lineage
/// (`_sync/plan-amendments.md`, "Memo lineage is structural"). The second half asserted that an edit blind
/// to K's create equals a missing base (an orphan of M); it now projects
/// exactly like the sighted edit.
#[test]
fn g3_p4_o3b_cross_memo_base_joins_the_base_memo_lineage() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let y = at(me(4, "d", 300, &[("b", 1)], M, 1, "y"), 1);
    let kc = at(mc(10, "k", 50, &[], K, "kay"), 1);
    let w = vec![c.clone(), y.clone(), kc.clone(), at(me(5, "a", 900, &[("b", 1), ("k", 1)], M, 10, "bad"), 1)];
    let p = order_free(&w, 50, 83);
    assert_eq!(memo_of(&p, M).unwrap().text, "y", "M keeps its own lineage");
    assert_eq!(memo_of(&p, K).unwrap().text, "bad", "the edit continues K's lineage");
    assert_eq!(p.memo_lineage[&oid(5)], K);
    assert_eq!((p.memo_lineage[&oid(1)].as_str(), p.memo_lineage[&oid(4)].as_str()), (M, M));
    assert_eq!(p.memo_lineage[&oid(10)], K);
    assert!(copies(&p).is_empty(), "no conflict copy: {:?}", copies(&p));
    assert!(hidden(&w, &p).is_empty());

    let w = vec![c, y, kc, at(me(5, "a", 900, &[("b", 1)], M, 10, "bad"), 1)];
    let blind = order_free(&w, 50, 85);
    assert_eq!(view(&blind), view(&p), "blind to K's create: joins K's lineage all the same");
    assert!(invisible(&w, &blind).is_empty(), "{:?}", invisible(&w, &blind));
}

/// See `g3_p4_o3a`: two edits naming each other as base.
#[test]
fn g3_p4_o3c_cyclic_bases_are_never_invisible() {
    odd_base_case(OddBase::Cycle);
}

#[derive(Clone, Copy, Debug)]
enum OddBase {
    SelfBase,
    Cycle,
}

fn odd_base_case(case: OddBase) {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let y = at(me(4, "d", 300, &[("b", 1)], M, 1, "y"), 1);
    let kc = at(mc(10, "k", 50, &[], K, "kay"), 1);
    let base_world = vec![c, y, kc];
    let odd = |base: u64| at(me(5, "a", 900, &[("b", 1)], M, base, "bad"), 1);
    let mut w = base_world.clone();
    w.push(odd(99));
    let missing = order_free(&w, 50, 81);
    let mut failures = Vec::new();
    if let OddBase::SelfBase = case {
        let mut w = base_world.clone();
        w.push(odd(5));
        let p = order_free(&w, 50, 83);
        if p.entities != missing.entities || p.notices != missing.notices {
            failures.push(format!(
                "base on itself: memo {:?}, memo K {:?}, notices {:?}",
                memo_of(&p, M).map(|m| m.text),
                memo_of(&p, K).map(|m| m.text),
                copies(&p)
            ));
        }
    }
    if let OddBase::Cycle = case {
        // e5 names e6 and e6 names e5.
        let mut w = base_world.clone();
        w.push(at(me(5, "a", 900, &[("b", 1)], M, 6, "cyc5"), 1));
        w.push(at(me(6, "e", 910, &[("b", 1)], M, 5, "cyc6"), 1));
        let p = order_free(&w, 50, 89);
        let texts = memo_texts(&p);
        if !(texts.contains("cyc5") && texts.contains("cyc6")) {
            failures.push(format!("base cycle: visible texts {texts:?}"));
        }
    }
    assert!(failures.is_empty(), "{case:?}: {failures:#?}");
}

// --------------------------------------------- forged bases (p5 delta)

/// `hidden_in` over a plain op list with no forks.
fn hidden(w: &[Pub], p: &Projection) -> Vec<String> {
    hidden_in(&set_of(w), &Forks::new(), p)
}

/// `invisible_memo_edits` over a plain op list: memo creates and edits
/// neither shown nor superseded within their own lineage. Unlike `hidden`,
/// a node that another node names as base counts as superseded (it was
/// seen by construction), whatever the naming edit's ctx.
fn invisible(w: &[Pub], p: &Projection) -> Vec<String> {
    invisible_memo_edits(&set_of(w), p)
}

/// Memo creates and edits of `w` that have no lineage owner.
fn unrouted(w: &[Pub], p: &Projection) -> Vec<String> {
    w.iter()
        .filter(|(b, _)| b.kind == Kind::Memo && matches!(b.op_type.as_str(), "create" | "edit"))
        .filter(|(b, _)| !p.memo_lineage.contains_key(&b.op_id))
        .map(|(b, _)| b.op_id.clone())
        .collect()
}

/// A ring of `n` edits of M (ids `first..first+n`), each naming the next as
/// base and the last naming the first. `concurrent`: one device each, all
/// saw only the create; otherwise one device in sequence.
fn ring(first: u64, n: u64, concurrent: bool) -> Vec<Pub> {
    (0..n)
        .map(|i| {
            let id = first + i;
            let base = first + (i + 1) % n;
            let text = format!("ring{i}");
            if concurrent {
                let dev = format!("r{i:02}");
                at(me(id, &dev, 1_000 + i, &[("b", 1)], M, base, &text), 1)
            } else {
                let ctx: Vec<(&str, u64)> = if i == 0 { vec![("b", 1)] } else { vec![("b", 1), ("r", i)] };
                at(me(id, "r", 1_000 + i, &ctx, M, base, &text), i + 1)
            }
        })
        .collect()
}

/// Long base cycles: a 40-edit ring of concurrent devices shows the newest
/// and gives every other member its own copy; a 40-edit ring written in
/// sequence on one device shows its last edit with no copy; a 12-ring with
/// a 5-edit honest tail hanging off one member. No edit is invisible, every
/// lineage op is routed, and 200 orders give one hash.
#[test]
fn g3_p5_r1_long_cycles_are_visible_and_order_free() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);

    let mut w = vec![c.clone()];
    w.extend(ring(100, 40, true));
    let p = order_free(&w, 200, 101);
    assert_eq!(memo_of(&p, M).unwrap().text, "ring39");
    assert_eq!(copies(&p).len(), 39, "one copy per losing ring member");
    let texts = memo_texts(&p);
    for i in 0..40 {
        assert!(texts.contains(&format!("ring{i}")), "ring{i} visible: {texts:?}");
    }
    assert_eq!(p.memo_lineage[&oid(1)], M, "the create stays in M");
    assert!(hidden(&w, &p).is_empty());
    assert!(unrouted(&w, &p).is_empty(), "unrouted: {:?}", unrouted(&w, &p));

    let mut w = vec![c.clone()];
    w.extend(ring(100, 40, false));
    let p = order_free(&w, 200, 103);
    assert_eq!(memo_of(&p, M).unwrap().text, "ring39");
    assert!(copies(&p).is_empty(), "the last edit saw every other: {:?}", copies(&p));
    for i in 0..40 {
        assert_eq!(p.memo_lineage[&oid(100 + i)], M, "ring op {i}");
    }
    assert!(hidden(&w, &p).is_empty());

    // 12-ring (concurrent) plus an honest tail t0 → ring3, t1 → t0, …
    let mut w = vec![c];
    w.extend(ring(100, 12, true));
    for i in 0..5u64 {
        let base = if i == 0 { 103 } else { 200 + i - 1 };
        let ctx: Vec<(&str, u64)> = if i == 0 { vec![("b", 1), ("r03", 1)] } else { vec![("b", 1), ("r03", 1), ("t", i)] };
        w.push(at(me(200 + i, "t", 500 + i, &ctx, M, base, &format!("tail{i}")), i + 1));
    }
    let p = order_free(&w, 200, 107);
    let texts = memo_texts(&p);
    for t in ["tail4", "ring11"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert!(hidden(&w, &p).is_empty(), "hidden: {:?}", hidden(&w, &p));
    assert!(unrouted(&w, &p).is_empty());
}

/// A cycle (e5 ↔ e6), a cycle descendant e8 → e5, and an honest edit w9
/// on cycle member e6, next to M's honest chain c ← y ← z. When the honest
/// chain is newest it stays M with all its ops; when the cycle is newest the
/// chain head z becomes a copy. Either way nothing is invisible.
///
/// Re-baselined with the structural forest (G3 delta 3, leader decision):
/// this asserted that the cycle descendant e8 is flattened into an orphan
/// head, which kept "cyc5" shown. Only cycle members become roots; e8 and
/// w9 stay children of their cycle member (plan-amendments "Memo lineage
/// is structural": a place is decided by the `base_op_id` chain), so they
/// supersede cyc5 and cyc6 in the members' own lineage. An edit that names
/// `base_op_id` X has seen X by construction; causality across a fork range
/// is decided only by the edit-vs-delete `saw`/obs rule, not by lineage.
#[test]
fn g3_p5_r2_cycle_next_to_an_honest_chain() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let y = at(me(2, "d", 300, &[("b", 1)], M, 1, "y"), 1);
    let z = at(me(3, "d", 400, &[("b", 1), ("d", 1)], M, 2, "z"), 2);
    let forged = |wall: u64| {
        vec![
            at(me(5, "a", wall, &[("b", 1)], M, 6, "cyc5"), 1),
            at(me(6, "e", wall + 10, &[("b", 1)], M, 5, "cyc6"), 1),
            at(me(8, "a", wall + 20, &[("b", 1), ("a", 1)], M, 5, "desc8"), 2),
            at(me(9, "h", wall + 30, &[("b", 1), ("e", 1)], M, 6, "w9"), 1),
        ]
    };
    for (name, wall, chain_wins) in [("honest chain newest", 150, true), ("cycle newest", 900, false)] {
        let mut w = vec![c.clone(), y.clone(), z.clone()];
        w.extend(forged(wall));
        let p = order_free(&w, 200, 109 + wall);
        let texts = memo_texts(&p);
        for t in ["z", "desc8", "w9"] {
            assert!(texts.contains(t), "{name}: {t} visible: {texts:?}");
        }
        assert_eq!(p.memo_lineage[&oid(5)], p.memo_lineage[&oid(8)], "{name}: desc8 continues cyc5");
        assert_eq!(p.memo_lineage[&oid(6)], p.memo_lineage[&oid(9)], "{name}: w9 continues cyc6");
        assert!(hidden(&w, &p).is_empty(), "{name}: hidden {:?}", hidden(&w, &p));
        assert!(invisible(&w, &p).is_empty(), "{name}: invisible {:?}", invisible(&w, &p));
        assert!(unrouted(&w, &p).is_empty(), "{name}");
        if chain_wins {
            assert_eq!(memo_of(&p, M).unwrap().text, "z", "{name}");
            for op in [1, 2, 3] {
                assert_eq!(p.memo_lineage[&oid(op)], M, "{name}: honest op {op} stays in M");
            }
        } else {
            assert_ne!(memo_of(&p, M).unwrap().text, "z", "{name}");
            let z_copy = conflict_copy_id(M, &oid(3));
            assert_eq!(memo_of(&p, &z_copy).unwrap().text, "z", "{name}");
            assert_eq!(p.memo_lineage[&oid(3)], z_copy, "{name}");
        }
    }
}

/// An edit naming a lineage op of a deleted memo K as base joins K's
/// lineage and follows K's delete rule; an edit of deleted K naming M's
/// create joins M's lineage and K stays deleted.
///
/// Adapted to the structural rule (`_sync/plan-amendments.md`, "Memo
/// lineage is structural"); this asserted entity-based routing (the edit
/// lives in the memo it addresses, as with a missing base). An edit that
/// saw K's create and delete joins K's lineage, and §4.4 sets it aside as
/// content that saw the delete: K stays deleted, M is untouched, and the
/// edit is causally superseded by the delete (not invisible by accident).
///
/// Re-baselined when the unrecorded causal-past filter was removed (G3
/// delta 3, architect b1, leader decision): the second half asserted that an
/// edit blind to K's create (and delete) equals a missing base and lives in
/// M, and that a K-addressed edit blind to M's create is an orphan of K. An
/// edit that names `base_op_id` X has seen X by construction; causality
/// across a fork range is decided only by the edit-vs-delete `saw`/obs rule,
/// not by lineage. So the blind edit joins K's lineage like the sighted one,
/// and K's delete, which did not see it, is restored by it (§4.4 edit vs
/// delete); the blind K-addressed edit joins M's lineage like the sighted one.
#[test]
fn g3_p5_r3_cross_memo_base_into_a_deleted_memo() {
    let kc = at(mc(10, "k", 50, &[], K, "kay"), 1);
    let kdel = at(md(11, "k", 60, &[("k", 1)], K), 2);
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let to_k = |base: u64, ctx: &[(&str, u64)]| at(me(5, "a", 900, ctx, M, base, "bad"), 1);

    let ctx = vec![("b", 1), ("k", 2)];
    let w = vec![kc.clone(), kdel.clone(), c.clone(), to_k(10, &ctx)];
    let p = order_free(&w, 200, 127);
    assert!(is_deleted(&p, Kind::Memo, K), "saw K's delete: K stays deleted");
    assert!(!is_restored(&p, Kind::Memo, K), "saw K's delete: K is not restored");
    assert_eq!(memo_of(&p, M).unwrap().text, "base", "saw K's delete: M is untouched");
    assert_eq!(p.memo_lineage[&oid(5)], K, "saw K's delete: joins K's lineage");
    assert!(copies(&p).is_empty(), "saw K's delete: {:?}", copies(&p));
    // `hidden_in` keys on the addressed memo M; the edit is stale K content.
    assert_eq!(hidden(&w, &p), vec![oid(5)], "saw K's delete");
    assert!(invisible(&w, &p).is_empty(), "saw K's delete: a tombstone owns it");

    for (name, ctx) in [("blind to K's create", vec![("b", 1)]), ("blind to K's delete", vec![("b", 1), ("k", 1)])] {
        let w = vec![kc.clone(), kdel.clone(), c.clone(), to_k(10, &ctx)];
        let p = order_free(&w, 200, 127);
        assert!(is_restored(&p, Kind::Memo, K), "{name}: the delete did not see the edit, K is restored");
        assert_eq!(memo_of(&p, K).unwrap().text, "bad", "{name}");
        assert_eq!(memo_of(&p, M).unwrap().text, "base", "{name}: M is untouched");
        assert_eq!(p.memo_lineage[&oid(5)], K, "{name}: joins K's lineage");
        assert!(copies(&p).is_empty(), "{name}: {:?}", copies(&p));
        // `hidden` would list K's create: its ctx-blind child supersedes it
        // structurally, so the structural oracle is the check here.
        assert!(invisible(&w, &p).is_empty(), "{name}");
    }

    // No create of M: an edit that saw K's delete is stale K content, so
    // nothing shows M; one blind to K's delete restores K.
    let w = vec![kc.clone(), kdel.clone(), to_k(10, &[("k", 2)])];
    let p = order_free(&w, 200, 131);
    assert!(is_deleted(&p, Kind::Memo, K));
    assert!(memo_of(&p, M).is_none());
    let w = vec![kc.clone(), kdel.clone(), to_k(10, &[])];
    let p = order_free(&w, 200, 133);
    assert!(is_restored(&p, Kind::Memo, K));
    assert_eq!(memo_of(&p, K).unwrap().text, "bad");
    assert!(memo_of(&p, M).is_none());
    assert!(invisible(&w, &p).is_empty());

    // An edit of deleted K naming M's create joins M's lineage whether or
    // not its ctx covers that create; K stays deleted, nothing is a copy.
    let of_k = |ctx: &[(&str, u64)]| at(me(5, "a", 900, ctx, K, 1, "bad"), 1);
    let sighted = vec![kc.clone(), kdel.clone(), c.clone(), of_k(&[("b", 1)])];
    let p = order_free(&sighted, 200, 141);
    assert!(is_deleted(&p, Kind::Memo, K), "K stays deleted");
    assert_eq!(memo_of(&p, M).unwrap().text, "bad", "joins M's lineage");
    assert_eq!(p.memo_lineage[&oid(5)], M);
    assert!(copies(&p).is_empty());
    let blind = vec![kc, kdel, c, of_k(&[])];
    let pb = order_free(&blind, 200, 137);
    assert_eq!(view(&pb), view(&p), "blind to M's create: joins M's lineage all the same");
    // `hidden` would list M's create (superseded by its ctx-blind child).
    assert!(invisible(&blind, &pb).is_empty());
}

/// A self-based edit f next to a concurrent honest edit y: whichever is
/// newer is shown, the other is a copy; causal order wins over HLC; an
/// honest child h of f follows f. Every variant equals the same world with
/// f's base missing.
#[test]
fn g3_p5_r4_self_base_with_a_concurrent_honest_edit() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let y = |ctx: &[(&str, u64)]| at(me(2, "d", 500, ctx, M, 1, "y"), 1);
    let f = |base: u64, wall: u64, ctx: &[(&str, u64)]| at(me(5, "a", wall, ctx, M, base, "self"), 1);
    let h = |base: u64| at(me(6, "a", 950, &[("b", 1), ("a", 1)], M, base, "child"), 2);
    type Case<'a> = (&'a str, Vec<(&'a str, u64)>, u64, Vec<(&'a str, u64)>, bool, &'a str, Option<u64>);
    let cases: Vec<Case> = vec![
        ("f newer", vec![("b", 1)], 900, vec![("b", 1)], false, "self", Some(2)),
        ("y newer", vec![("b", 1)], 300, vec![("b", 1)], false, "y", Some(5)),
        ("f saw y", vec![("b", 1)], 300, vec![("b", 1), ("d", 1)], false, "self", None),
        ("y saw f", vec![("b", 1), ("a", 1)], 900, vec![("b", 1)], false, "y", None),
        ("f newer with child", vec![("b", 1)], 900, vec![("b", 1)], true, "child", Some(2)),
        ("y newer than f, f's child newest", vec![("b", 1)], 300, vec![("b", 1)], true, "child", Some(2)),
    ];
    for (name, y_ctx, wall, f_ctx, child, shown, copy_of) in cases {
        let world = |base: u64| {
            let mut w = vec![c.clone(), y(&y_ctx), f(base, wall, &f_ctx)];
            if child {
                w.push(h(5));
            }
            w
        };
        let w = world(5);
        let p = order_free(&w, 200, 149);
        let missing = order_free(&world(99), 50, 151);
        assert_eq!(
            (&p.entities, &p.notices, &p.memo_lineage),
            (&missing.entities, &missing.notices, &missing.memo_lineage),
            "{name}"
        );
        assert_eq!(memo_of(&p, M).unwrap().text, shown, "{name}");
        match copy_of {
            Some(op) => assert_eq!(copies(&p), vec![(conflict_copy_id(M, &oid(op)), oid(op))], "{name}"),
            None => assert!(copies(&p).is_empty(), "{name}: {:?}", copies(&p)),
        }
        assert_eq!(p.memo_lineage[&oid(1)], M, "{name}: the create never moves");
        assert!(hidden(&w, &p).is_empty(), "{name}");
    }
}

/// Every forged form at once (self base, cross-memo base both ways, missing
/// base, pin base, a 2-cycle with a descendant, a 6-ring) beside honest
/// lineages of M and K, with and without blind deletes: 200 random orders
/// give one hash, notices and routing; nothing is invisible.
///
/// Re-baselined with the structural forest (G3 delta 3, leader decision):
/// this asserted that cyc25 stays shown because its descendant desc27 was
/// flattened into an orphan head, and that crossM (ctx blind to K's create)
/// stays in M or a copy of M. Only cycle members become roots: desc27 stays
/// a child of cyc25 and supersedes it in its own lineage; crossM names K's
/// create and joins K's lineage. An edit that names `base_op_id` X has seen
/// X by construction; causality across a fork range is decided only by the
/// edit-vs-delete `saw`/obs rule, not by lineage.
#[test]
fn g3_p5_r5_every_forged_form_under_200_permutations() {
    let mut w = vec![
        at(mc(1, "b", 100, &[], M, "base"), 1),
        at(mp(2, "b", 110, &[("b", 1)], M, true), 2),
        at(me(3, "d", 300, &[("b", 1)], M, 1, "y"), 1),
        at(me(4, "d", 310, &[("b", 1), ("d", 1)], M, 3, "z"), 2),
        at(mc(10, "k", 50, &[], K, "kay"), 1),
        at(me(11, "k", 60, &[("k", 1)], K, 10, "kay2"), 2),
        // self, cross (M → K's create), cross (K → M's edit), missing, pin.
        at(me(20, "s", 420, &[("b", 1)], M, 20, "self"), 1),
        at(me(21, "x", 430, &[("b", 1)], M, 10, "crossM"), 1),
        at(me(22, "x", 440, &[("x", 1), ("k", 1)], K, 3, "crossK"), 2),
        at(me(23, "m", 450, &[], M, 99, "missing"), 1),
        at(me(24, "p", 460, &[("b", 2)], M, 2, "pinbase"), 1),
        // 2-cycle with a descendant.
        at(me(25, "e", 470, &[("b", 1)], M, 26, "cyc25"), 1),
        at(me(26, "g", 480, &[("b", 1)], M, 25, "cyc26"), 1),
        at(me(27, "e", 490, &[("e", 1)], M, 25, "desc27"), 2),
        // A cycle across memos: M's edit names K's edit and back.
        at(me(28, "u", 495, &[("b", 1)], M, 29, "xcycM"), 1),
        at(me(29, "v", 496, &[("k", 1)], K, 28, "xcycK"), 1),
    ];
    w.extend(ring(100, 6, true));
    let p = order_free(&w, 200, 157);
    let texts = memo_texts(&p);
    for t in ["z", "kay2", "self", "crossM", "crossK", "missing", "pinbase", "cyc26", "desc27", "ring5", "xcycM", "xcycK"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert_eq!(p.memo_lineage[&oid(25)], p.memo_lineage[&oid(27)], "desc27 continues cyc25");
    assert!(hidden(&w, &p).is_empty(), "hidden: {:?}", hidden(&w, &p));
    assert!(invisible(&w, &p).is_empty(), "invisible: {:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty(), "unrouted: {:?}", unrouted(&w, &p));
    let copy_ids: Vec<String> = copies(&p).into_iter().map(|(c, _)| c).collect();
    // Each op lives in the memo of its base (crossM: K, crossK: M) or a copy
    // of it; cycle members in their own memo or a copy of it.
    for (op, owner) in [(21, K), (22, M), (28, M), (29, K)] {
        let routed = &p.memo_lineage[&oid(op)];
        assert!(routed == owner || copy_ids.contains(routed), "op {op} routed to {routed}");
    }

    let mut wd = w.clone();
    wd.push(at(md(30, "z", 2_000, &[("b", 2), ("d", 2), ("s", 1)], M), 1));
    wd.push(at(md(31, "y", 2_000, &[("k", 2), ("x", 2)], K), 1));
    let pd = order_free(&wd, 200, 163);
    assert!(hidden(&wd, &pd).is_empty());
    assert!(invisible(&wd, &pd).is_empty(), "invisible: {:?}", invisible(&wd, &pd));
    // The deletes missed forged ops of their memo: each missed text survives
    // in the restored memo or in a copy.
    let texts = memo_texts(&pd);
    for t in ["crossM", "missing", "pinbase", "cyc26", "desc27", "ring5", "xcycM", "xcycK"] {
        assert!(texts.contains(t), "{t} survives a delete that missed it: {texts:?}");
    }
}

/// Delete fate against orphaned forged edits. Concurrent forged heads
/// (self f, cycle e6 ↔ e7) plus a cross-memo edit x naming K's create: a
/// delete of M that saw all of M's content deletes M while the losing
/// copies stay; a delete that missed any one keeps that text visible (M
/// restored when it was M's head). Forged edits in sequence on one device:
/// a delete that saw the last of M's content deletes M; one that missed it
/// restores M with the last text.
///
/// Re-baselined when the unrecorded causal-past filter was removed (G3
/// delta 3, architect b1, leader decision): x (ctx b:1, blind to K's
/// create) was an orphan of M, so M showed "cross" and a delete of M that
/// missed x was restored by it. An edit that names `base_op_id` X has seen X
/// by construction; causality across a fork range is decided only by the
/// edit-vs-delete `saw`/obs rule, not by lineage. So x joins K's lineage
/// (K shows "cross"), M shows its newest forged head "cyc7", and x is K's
/// content in fate accounting: an M delete that missed x stays deleted.
#[test]
fn g3_p5_r6_delete_fate_with_orphaned_forged_edits() {
    let c = at(mc(1, "b", 100, &[], M, "base"), 1);
    let kc = at(mc(10, "k", 50, &[], K, "kay"), 1);
    let concurrent = vec![
        c.clone(),
        kc.clone(),
        at(me(5, "a", 300, &[("b", 1)], M, 5, "self"), 1),
        at(me(6, "e", 310, &[("b", 1)], M, 7, "cyc6"), 1),
        at(me(7, "g", 315, &[("b", 1)], M, 6, "cyc7"), 1),
        at(me(8, "x", 320, &[("b", 1)], M, 10, "cross"), 1),
    ];
    let p = order_free(&concurrent, 100, 167);
    assert_eq!(memo_of(&p, M).unwrap().text, "cyc7");
    assert_eq!(memo_of(&p, K).unwrap().text, "cross");
    assert!(invisible(&concurrent, &p).is_empty(), "{:?}", invisible(&concurrent, &p));
    let full: Vec<(&str, u64)> = vec![("b", 1), ("a", 1), ("e", 1), ("g", 1), ("x", 1)];
    let del = |ctx: &[(&str, u64)]| at(md(9, "z", 2_000, ctx, M), 1);
    let mut w = concurrent.clone();
    w.push(del(&full));
    let p = order_free(&w, 200, 173);
    assert!(is_deleted(&p, Kind::Memo, M), "the delete saw every forged edit");
    for t in ["self", "cyc6"] {
        assert!(memo_texts(&p).contains(t), "{t}: losing copies stay");
    }
    assert_eq!(memo_of(&p, K).unwrap().text, "cross");
    assert!(invisible(&w, &p).is_empty(), "{:?}", invisible(&w, &p));
    for (dev, text, restores) in [("a", "self", false), ("e", "cyc6", false), ("g", "cyc7", true), ("x", "cross", false), ("b", "cyc7", true)] {
        let ctx: Vec<(&str, u64)> = full.iter().copied().filter(|(d, _)| *d != dev).collect();
        let mut w = concurrent.clone();
        w.push(del(&ctx));
        let p = order_free(&w, 200, 179);
        assert!(memo_texts(&p).contains(text), "missed {dev}: {text} visible");
        assert_eq!(is_restored(&p, Kind::Memo, M), restores, "missed {dev}: M restored");
        assert_eq!(is_deleted(&p, Kind::Memo, M), !restores, "missed {dev}: M deleted");
        assert!(invisible(&w, &p).is_empty(), "missed {dev}: {:?}", invisible(&w, &p));
    }

    let seq = vec![
        c,
        kc,
        at(me(5, "a", 300, &[("b", 1)], M, 5, "self"), 1),
        at(me(6, "a", 310, &[("b", 1), ("a", 1)], M, 7, "cyc6"), 2),
        at(me(7, "a", 320, &[("b", 1), ("a", 2)], M, 6, "cyc7"), 3),
        at(me(8, "a", 330, &[("b", 1), ("a", 3)], M, 10, "cross"), 4),
    ];
    let p = order_free(&seq, 100, 181);
    assert_eq!(memo_of(&p, M).unwrap().text, "cyc7");
    assert_eq!(memo_of(&p, K).unwrap().text, "cross");
    assert!(copies(&p).is_empty());
    for (name, ctx, deleted) in [
        ("saw all", vec![("b", 1), ("a", 4)], true),
        ("missed only K's content", vec![("b", 1), ("a", 3)], true),
        ("missed the last of M", vec![("b", 1), ("a", 2)], false),
        ("saw only the create", vec![("b", 1)], false),
    ] {
        let mut w = seq.clone();
        w.push(del(&ctx));
        let p = order_free(&w, 200, 191);
        assert_eq!(is_deleted(&p, Kind::Memo, M), deleted, "{name}");
        assert_eq!(is_restored(&p, Kind::Memo, M), !deleted, "{name}");
        if !deleted {
            assert_eq!(memo_of(&p, M).unwrap().text, "cyc7", "{name}");
        }
        assert_eq!(memo_of(&p, K).unwrap().text, "cross", "{name}");
        assert!(invisible(&w, &p).is_empty(), "{name}: {:?}", invisible(&w, &p));
    }
}

// ------------------------------------------------------ MAX_STORED (p4)

/// The one bound `ops::MAX_STORED` (= i64::MAX) at every entry point: a seq
/// or hlc above it on the plain, sealed, forked and fork-variant paths is
/// never stored and never retried (held); exactly MAX_STORED is a valid
/// seq (lag / gap, never quarantined as invalid); a local insert above the
/// bound is refused; own-stream bookkeeping calls with out-of-range seqs
/// return Err and change nothing; a snapshot carrying out-of-range dots,
/// frontier or forks never verifies and never panics.
#[test]
fn g3_p4_m1_max_stored_bound_at_every_entry_point() {
    let max = MAX_STORED;
    assert_eq!(max, i64::MAX as u64);
    let w = lag_world();
    let forked = || run(&[Feed::Many(w.v.to_vec()), one(&w.c[0])]);
    let at_seq = |seq: u64, n: u64| link("a", seq, &head(&w.c[0]), mc(n, "a", T0 + n, &[("a", 2)], &format!("memo-{n}"), "x"));
    for (name, seq) in [("max + 1", max + 1), ("u64::MAX", u64::MAX)] {
        let (s, s2) = (at_seq(seq, 70), at_seq(seq, 71));
        // Plain stream (cursor at 0): a gap, held, nothing stored.
        let mut r = device("r");
        let out = r.apply_segment(VAULT, "a", seq, seq, &s).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} plain: {out:?}");
        assert!(op_ids(&r).is_empty(), "{name} plain");
        // Sealed under that address.
        let mut r = device("r");
        let out = r.apply_sealed(VAULT, &seg_key(), &addr("a", seq, seq), &sealed(&s)).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. } | ApplyOutcome::Quarantined { .. }), "{name} sealed: {out:?}");
        assert!(op_ids(&r).is_empty(), "{name} sealed");
        // Forked continuation.
        let mut r = forked();
        let ids = op_ids(&r);
        let out = r.apply_segment(VAULT, "a", seq, seq, &s).unwrap();
        assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} forked: {out:?}");
        assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name} forked");
        assert_eq!(op_ids(&r), ids, "{name} forked");
        // Fork variants on a fresh, a plain and a forked cursor.
        for (label, mut r) in [("fresh", device("r")), ("plain", run(&[one(&w.v[0])])), ("forked", forked())] {
            let ids = op_ids(&r);
            let out = r.apply_fork_variant(VAULT, "a", seq, seq, &[s.clone(), s2.clone()]).unwrap();
            assert!(matches!(out, ApplyOutcome::Held { .. }), "{name} variants {label}: {out:?}");
            assert_eq!(r.quarantine_rows(VAULT).unwrap().len(), 1, "{name} variants {label}");
            assert_eq!(op_ids(&r), ids, "{name} variants {label}");
            assert_eq!(r.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Held, "{name} variants {label}");
        }
    }
    // Exactly MAX_STORED is a valid seq: lag on a forked stream, a gap on a
    // plain one; never quarantined.
    // (A seq-max segment naming a known head is a chain fault instead.)
    let lag_at = |n: u64| link("a", max, &"ab".repeat(32), mc(n, "a", T0 + n, &[("a", 2)], &format!("memo-{n}"), "x"));
    let s = lag_at(72);
    assert_eq!(s.validate(), Ok(()));
    let mut r = forked();
    let out = r.apply_segment(VAULT, "a", max, max, &at_seq(max, 75)).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. }), "max after a known head: {out:?}");
    let mut r = forked();
    let before = dump_all(&r, &[]);
    let out = r.apply_segment(VAULT, "a", max, max, &s).unwrap();
    assert!(matches!(out, ApplyOutcome::Deferred { .. }), "max forked: {out:?}");
    assert_eq!(dump_all(&r, &[]), before);
    let out = r.apply_fork_variant(VAULT, "a", max, max, &[s.clone(), lag_at(73)]).unwrap();
    assert!(matches!(out, ApplyOutcome::Deferred { .. }), "max variants forked: {out:?}");
    assert_eq!(dump_all(&r, &[]), before);
    let mut r = device("r");
    let out = r.apply_segment(VAULT, "a", max, max, &s).unwrap();
    assert!(matches!(out, ApplyOutcome::Held { .. }), "max plain: {out:?}");
    assert!(r.quarantine_rows(VAULT).unwrap().is_empty(), "a gap blames nothing");

    // hlc above the bound on the forked and variant paths.
    let mut hot = mc(74, "a", T0, &[("a", 2)], "memo-74", "hot");
    hot.hlc.wall_ms = max + 1;
    let hot = link("a", 3, &head(&w.c[0]), hot);
    let mut r = forked();
    let ids = op_ids(&r);
    let out = feed(&mut r, &one(&hot));
    assert!(matches!(out, ApplyOutcome::Held { .. }), "hlc forked: {out:?}");
    assert_eq!(op_ids(&r), ids);
    let mut r = forked();
    let out = feed(&mut r, &Feed::Many(vec![w.d[0].clone(), hot.clone()]));
    assert!(matches!(out, ApplyOutcome::Held { .. }), "hlc variants: {out:?}");
    assert_eq!(op_ids(&r), ids, "no variant applied");

    // Local insert.
    let mut a = device("a");
    local(&mut a, create(1, 1_000, M, "one"));
    let before = a.dump_state().unwrap();
    let mut bad = create(2, max + 1, K, "late");
    bad.origin_device = "a".to_owned();
    assert!(a.insert_local_op(VAULT, bad, UploadState::Never).is_err());
    assert_eq!(a.dump_state().unwrap(), before, "a refused local insert changes nothing");

    // Own-stream bookkeeping with out-of-range seqs.
    let mut remote = Remote::new(true);
    publish(&mut a, &mut remote).unwrap();
    let before = a.dump_state().unwrap();
    for first in [max + 1, u64::MAX] {
        let calls = catch_unwind(AssertUnwindSafe(|| {
            assert!(a.mark_attempted(VAULT, "a", first).is_err());
            assert!(a.mark_published(VAULT, "a", first).is_err());
            assert!(a.mark_rejected(VAULT, "a", first).is_err());
            assert!(!matches!(a.sealed_bytes(VAULT, "a", first), Ok(Some(_))));
            assert!(a.forget_segments(VAULT, "a", &[(first, first)]).is_err());
            a.offer_deletable(VAULT, "a", first).unwrap()
        }));
        assert_eq!(calls.ok(), Some(vec![(1, 1)]), "first {first}: no panic; the limit is not an SQL bind");
    }
    assert_eq!(a.dump_state().unwrap(), before);

    // Snapshot fields beyond the bound.
    let r = forked();
    let snap = build_snapshot(&r, VAULT).unwrap();
    assert!(verify_snapshot(&r, VAULT, &snap).unwrap());
    let mut tampered = Vec::new();
    let mut t = snap.clone();
    t.ops[0].dots[0].seq = max + 1;
    tampered.push(("dot", t));
    let mut t = snap.clone();
    t.frontier.insert("a".to_owned(), u64::MAX);
    tampered.push(("frontier", t));
    let mut t = snap.clone();
    t.forks.insert("a".to_owned(), max + 1);
    tampered.push(("forks", t));
    for (name, t) in &tampered {
        assert!(!verify_snapshot(&r, VAULT, t).unwrap_or(false), "{name}: never verifies");
        assert!(catch_unwind(AssertUnwindSafe(|| restore_equivalent(t))).is_ok(), "{name}: restore never panics");
        assert!(catch_unwind(AssertUnwindSafe(|| t.encode().map(|b| Snapshot::decode(&b)))).is_ok(), "{name}: decode never panics");
    }
}

// ------------------------------------- copy-addressed edits (p6 delta)

/// Live memos (not tombstones).
fn live_memos(p: &Projection) -> usize {
    p.entities.values().filter(|e| matches!(e, EntityState::Memo(_))).count()
}

type View<'a> = (&'a BTreeMap<(Kind, String), EntityState>, &'a Vec<Notice>, &'a BTreeMap<String, String>);

fn view(p: &Projection) -> View<'_> {
    (&p.entities, &p.notices, &p.memo_lineage)
}

/// The G3 delta d1 world with ids shifted by `off`: create c; a's a1 and
/// b's b1 race on c (b1 wins, a1 → copy C); on C, a's x1 and b's y1 race on
/// a1 while a's x2 continues x1 with the newest HLC. `addr` names the memo
/// each of x1, x2, y1 addresses.
fn d1_world(off: u64, addr: [&str; 3]) -> Vec<Pub> {
    vec![
        at(mc(off + 1, "a", T0, &[], M, "base"), 1),
        at(me(off + 2, "a", T0 + 1, &[("a", 1)], M, off + 1, "a1"), 2),
        at(me(off + 3, "b", T0 + 2, &[("a", 1)], M, off + 1, "b1"), 1),
        at(me(off + 5, "a", T0 + 4, &[("a", 2), ("b", 1)], addr[0], off + 2, "x1"), 3),
        at(me(off + 6, "a", T0 + 10, &[("a", 3), ("b", 1)], addr[1], off + 5, "x2"), 4),
        at(me(off + 7, "b", T0 + 6, &[("a", 2), ("b", 1)], addr[2], off + 2, "y1"), 2),
    ]
}

/// §4.4 result of `d1_world`: M = b1, C = y1, C's copy (loser x1) = x2.
fn assert_d1_shape(p: &Projection, off: u64, name: &str) {
    let copy = conflict_copy_id(M, &oid(off + 2));
    let nested = conflict_copy_id(&copy, &oid(off + 5));
    assert_eq!(memo_of(p, M).map(|m| m.text), Some("b1".to_owned()), "{name}");
    let c = memo_of(p, &copy).unwrap_or_else(|| panic!("{name}: copy missing; texts {:?}", memo_texts(p)));
    assert_eq!((c.text.as_str(), c.head_op_id, c.conflict_of.as_deref()), ("y1", oid(off + 7), Some(M)), "{name}");
    let n = memo_of(p, &nested).unwrap_or_else(|| panic!("{name}: nested copy missing; texts {:?}", memo_texts(p)));
    assert_eq!((n.text.as_str(), n.head_op_id, n.conflict_of.as_deref()), ("x2", oid(off + 6), Some(copy.as_str())), "{name}");
    let k_live = usize::from(memo_of(p, K).is_some());
    assert_eq!(live_memos(p) - k_live, 3, "{name}: M, C, C's copy besides K; texts {:?}", memo_texts(p));
    for (n, owner) in [(1, M), (3, M), (2, copy.as_str()), (7, copy.as_str()), (5, nested.as_str()), (6, nested.as_str())] {
        assert_eq!(p.memo_lineage[&oid(off + n)], owner, "{name}: op {}", off + n);
    }
}

/// d1 itself across 300 permutations, with x1/x2/y1 addressed to the copy
/// (as a UI holding `head_op_id` writes them), to M, to the nested copy
/// (x2 written after x1 lost), and mixed: one §4.4 result, one hash.
#[test]
fn g3_p6_r1_d1_reproduction_under_300_permutations_and_every_addressing() {
    let copy = conflict_copy_id(M, &oid(2));
    let nested = conflict_copy_id(&copy, &oid(5));
    let reference = order_free(&d1_world(0, [&copy, &copy, &copy]), 300, 0xD1);
    assert_d1_shape(&reference, 0, "copy-addressed");
    for (name, addr) in [
        ("M-addressed", [M, M, M]),
        ("x2 on the nested copy", [copy.as_str(), nested.as_str(), copy.as_str()]),
        ("mixed", [M, copy.as_str(), M]),
        ("mixed 2", [copy.as_str(), M, copy.as_str()]),
    ] {
        let p = order_free(&d1_world(0, addr), 100, 0xD2);
        assert_d1_shape(&p, 0, name);
        assert_eq!(view(&p), view(&reference), "{name}");
        assert_eq!(p.hash, reference.hash, "{name}");
    }
}

/// Three levels of copies, each with a chain whose tip carries the newest
/// HLC of its level, every follow-up addressed to the copy its writer saw.
/// Each race is decided at its shared base (§4.4), never by a chain tip.
fn deep_world(addr_all: Option<&str>) -> (Vec<Pub>, [String; 3]) {
    let c1 = conflict_copy_id(M, &oid(2));
    let c2 = conflict_copy_id(&c1, &oid(5));
    let c3 = conflict_copy_id(&c2, &oid(9));
    let a = |own: &str| addr_all.unwrap_or(own).to_owned();
    let (a1, a2) = (a(&c1), a(&c2));
    let w = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "a", T0 + 1, &[("a", 1)], M, 1, "a1"), 2),
        at(me(3, "b", T0 + 2, &[("a", 1)], M, 1, "b1"), 1),
        // Level C1: x1 vs y1 on a1; x2 continues x1 (newest of the level).
        at(me(5, "a", T0 + 4, &[("a", 2), ("b", 1)], &a1, 2, "x1"), 3),
        at(me(6, "a", T0 + 10, &[("a", 3), ("b", 1)], &a1, 5, "x2"), 4),
        at(me(7, "b", T0 + 6, &[("a", 2), ("b", 1)], &a1, 2, "y1"), 2),
        // Level C2 (C1's copy holding x1 ← x2): z1 vs w1 on x2; w2 continues
        // w1 with the newest HLC; z2 continues z1.
        at(me(8, "c", T0 + 20, &[("a", 4), ("b", 2)], &a2, 6, "z1"), 1),
        at(me(9, "d", T0 + 15, &[("a", 4), ("b", 2)], &a2, 6, "w1"), 1),
        at(me(10, "d", T0 + 40, &[("a", 4), ("b", 2), ("d", 1)], &a2, 9, "w2"), 2),
        at(me(11, "c", T0 + 21, &[("a", 4), ("b", 2), ("c", 1)], &a2, 8, "z2"), 2),
    ];
    (w, [c1, c2, c3])
}

#[test]
fn g3_p6_r2_copies_of_copies_with_chains() {
    let (w, [c1, c2, c3]) = deep_world(None);
    let p = order_free(&w, 300, 0xC0C0);
    let expect = [(M, "b1", 3, None), (c1.as_str(), "y1", 7, Some(M)), (c2.as_str(), "z2", 11, Some(c1.as_str())), (c3.as_str(), "w2", 10, Some(c2.as_str()))];
    for (id, text, head, of) in expect {
        let m = memo_of(&p, id).unwrap_or_else(|| panic!("{id} missing; texts {:?}", memo_texts(&p)));
        assert_eq!((m.text.as_str(), m.head_op_id, m.conflict_of.as_deref()), (text, oid(head), of), "{id}");
    }
    assert_eq!(live_memos(&p), 4, "texts {:?}", memo_texts(&p));
    for (ops, owner) in [(&[1, 3][..], M), (&[2, 7], c1.as_str()), (&[5, 6, 8, 11], c2.as_str()), (&[9, 10], c3.as_str())] {
        for n in ops {
            assert_eq!(p.memo_lineage[&oid(*n)], owner, "op {n}");
        }
    }
    assert_eq!(copies(&p).len(), 3);
    assert!(hidden(&w, &p).is_empty());
    // Addressing every follow-up to M, or to C1, routes identically.
    for all in [M, c1.as_str()] {
        let (wa, _) = deep_world(Some(all));
        let q = order_free(&wa, 100, 0xC0C1);
        assert_eq!(view(&q), view(&p), "all addressed to {all}");
    }
}

/// Forged cross-memo bases whose target is a copy (or a copy edit naming
/// another memo).
///
/// Adapted to the structural rule (`_sync/plan-amendments.md`, "Memo
/// lineage is structural"); this asserted entity-based routing (equal to a
/// missing base). Each forged op saw its base, so it projects exactly like
/// the same op addressed to the memo its base lives in (`entity_id` does
/// not matter), deterministically and visibly.
#[test]
fn g3_p6_r3_cross_memo_forged_base_into_a_copy() {
    let (honest, [c1, c2, _]) = deep_world(None);
    let kc = at(mc(40, "k", T0 - 5, &[], K, "kay"), 1);
    let mut base = honest.clone();
    base.push(kc);
    let reference = order_free(&base, 50, 0xF0);
    type Forged<'a> = (&'a str, &'a str, u64, &'a str);
    let forged: [Forged; 5] = [
        ("K edit on C1's head", K, 7, "fk1"),
        ("K edit on C2's race base", K, 6, "fk2"),
        ("K edit on C3's chain", K, 9, "fk3"),
        ("C1 edit on K's create", &c1, 40, "fc1"),
        ("C2 edit on M's head", &c2, 3, "fc2"),
    ];
    for (name, entity, b, text) in forged {
        let mk = |memo: &str| at(me(50, "f", T0 + 100, &[("a", 4), ("b", 2), ("c", 2), ("d", 2), ("k", 1)], memo, b, text), 1);
        let mut w = base.clone();
        w.push(mk(entity));
        let p = order_free(&w, 200, 0xF1);
        let mut wr = base.clone();
        wr.push(mk(&reference.memo_lineage[&oid(b)]));
        let readdressed = order_free(&wr, 50, 0xF2);
        assert_eq!(view(&p), view(&readdressed), "{name}");
        assert!(hidden(&w, &p).is_empty(), "{name}");
        assert!(unrouted(&w, &p).is_empty(), "{name}");
        assert!(memo_texts(&p).contains(text), "{name}");
    }
}

/// Misroutes plus cycles: a 2-cycle of C1-addressed edits, a forged K edit
/// hanging off a cycle member, a cycle across M and K, and a misrouted K
/// edit with an honest K continuation. Nothing invisible; 300 orders, one
/// hash.
///
/// Adapted to the structural rule (`_sync/plan-amendments.md`, "Memo
/// lineage is structural"); this asserted entity-based routing (equal to
/// every forged base missing). Cycle members stay orphans of their own
/// memo; offcycle hangs off cycle member 20 and stays its child, so it
/// lives in 20's memo (C1 or a copy of C1). misK (saw y1) with its
/// continuation misK2 projects exactly like the same ops addressed to the
/// memo that shows them (`entity_id` does not matter), and that memo is in
/// the lineage of its base (C1 or one of its copies).
///
/// Re-baselined with the structural forest (G3 delta 3, leader decision):
/// this asserted that offcycle is flattened into an orphan head of K. Only
/// cycle members become roots; a descendant of a cycle member keeps its
/// `base_op_id` parent (plan-amendments "Memo lineage is structural"). An
/// edit that names `base_op_id` X has seen X by construction; causality
/// across a fork range is decided only by the edit-vs-delete `saw`/obs
/// rule, not by lineage.
#[test]
fn g3_p6_r4_forged_entity_address_plus_cycle() {
    let (honest, [c1, c2, c3]) = deep_world(None);
    let world = |off: &str, mis: &str| {
        let mut w = honest.clone();
        w.push(at(mc(40, "k", T0 - 5, &[], K, "kay"), 1));
        w.extend([
            at(me(20, "e", T0 + 60, &[("a", 2), ("b", 1)], &c1, 21, "cyc20"), 1),
            at(me(21, "g", T0 + 61, &[("a", 2), ("b", 1)], &c1, 20, "cyc21"), 1),
            at(me(23, "u", T0 + 63, &[("a", 1)], M, 24, "xcycM"), 1),
            at(me(24, "v", T0 + 64, &[("k", 1)], K, 23, "xcycK"), 1),
        ]);
        let tail = [
            at(me(22, "e", T0 + 62, &[("e", 1)], off, 20, "offcycle"), 2),
            at(me(25, "k", T0 + 65, &[("k", 1), ("b", 2)], mis, 7, "misK"), 2),
            at(me(26, "k", T0 + 66, &[("k", 2), ("b", 2)], mis, 25, "misK2"), 3),
        ];
        (w, tail)
    };
    let (mut w, tail) = world(K, K);
    w.extend(tail);
    let p = order_free(&w, 300, 0xCC);
    let (off, mis) = (p.memo_lineage[&oid(22)].clone(), p.memo_lineage[&oid(25)].clone());
    let copy_of = |owner: &str| memo_of(&p, owner).and_then(|m| m.conflict_of);
    assert!(off == c1 || copy_of(&off).as_deref() == Some(c1.as_str()), "offcycle in C1 or a copy of C1: {off}");
    assert_eq!(off, p.memo_lineage[&oid(20)], "offcycle continues cycle member 20");
    assert!(mis == c1 || copy_of(&mis).as_deref() == Some(c1.as_str()), "misK in C1 or a copy of C1: {mis}");
    let (mut wr, tail) = world(K, &mis);
    wr.extend(tail);
    let readdressed = order_free(&wr, 50, 0xCE);
    assert_eq!(view(&p), view(&readdressed));
    assert!(hidden(&w, &p).is_empty(), "hidden: {:?}", hidden(&w, &p));
    assert!(invisible(&w, &p).is_empty(), "invisible: {:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty(), "unrouted: {:?}", unrouted(&w, &p));
    let texts = memo_texts(&p);
    for t in ["cyc21", "offcycle", "xcycM", "xcycK", "misK2", "z2", "w2"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert_eq!(memo_of(&p, &c2).unwrap().text, "z2");
    assert_eq!(memo_of(&p, &c3).unwrap().text, "w2");
    assert_eq!(p.memo_lineage[&oid(26)], p.memo_lineage[&oid(25)], "misK2 follows misK");
    for n in [20, 21] {
        let owner = &p.memo_lineage[&oid(n)];
        assert!(*owner == c1 || copy_of(owner).as_deref() == Some(c1.as_str()), "cycle member {n} in C1 or a copy of C1: {owner}");
    }
}

/// Misroute plus a same-base race without a name collision: a forged K edit
/// with the newest HLC on C1's race base a1, and ones that tie a loser's or
/// the winner's text.
///
/// Adapted to the structural rule (`_sync/plan-amendments.md`, "Memo
/// lineage is structural"); this asserted entity-based routing (equal to a
/// missing base). Now the edit projects exactly like the same op addressed
/// to the memo its base lives in: `entity_id` does not matter.
///
/// Re-baselined when the unrecorded causal-past filter was removed (G3
/// delta 3, architect b1, leader decision): its ctx (a, 1) covers M's create
/// but not a1 (a, 2), so the a1 cases asserted an orphan of K equal to a
/// missing base. An edit that names `base_op_id` X has seen X by
/// construction; causality across a fork range is decided only by the
/// edit-vs-delete `saw`/obs rule, not by lineage. So every case joins the
/// race on its base: the newest "zz" wins C (y1 and x1 each become a copy of
/// C); a loser's or the winner's text merges into that text's group, keeping
/// the d1 shape (larger id: no rename).
#[test]
fn g3_p6_r5a_misroute_plus_same_base_race() {
    let copy = conflict_copy_id(M, &oid(102));
    let honest = d1_world(100, [&copy, &copy, &copy]);
    for (name, n, b, wall, text) in [
        ("newest on a1", 150, 102, T0 + 99, "zz"),
        ("loser text x1, larger id", 150, 102, T0 + 3, "x1"),
        ("winner text y1", 150, 102, T0 + 3, "y1"),
        ("winner text b1 on c", 150, 101, T0 + 3, "b1"),
    ] {
        let mk = |memo: &str| at(me(n, "f", wall, &[("a", 1)], memo, b, text), 1);
        let mut w = honest.clone();
        w.push(mk(K));
        let p = order_free(&w, 200, 0xE1);
        let mut wr = honest.clone();
        wr.push(mk(if b == 101 { M } else { copy.as_str() }));
        let reference = order_free(&wr, 50, 0xE2);
        assert_eq!(view(&p), view(&reference), "{name}");
        assert_eq!(p.hash, reference.hash, "{name}");
        assert!(unrouted(&w, &p).is_empty(), "{name}");
        assert!(invisible(&w, &p).is_empty(), "{name}: {:?}", invisible(&w, &p));
        if text == "zz" {
            let c = memo_of(&p, &copy).unwrap();
            assert_eq!((c.text.as_str(), c.head_op_id, c.conflict_of.as_deref()), ("zz", oid(150), Some(M)), "{name}");
            let x = memo_of(&p, &conflict_copy_id(&copy, &oid(105))).unwrap();
            assert_eq!((x.text.as_str(), x.conflict_of.as_deref()), ("x2", Some(copy.as_str())), "{name}");
            let y = memo_of(&p, &conflict_copy_id(&copy, &oid(107))).unwrap();
            assert_eq!((y.text.as_str(), y.conflict_of.as_deref()), ("y1", Some(copy.as_str())), "{name}");
            assert_eq!(memo_of(&p, M).unwrap().text, "b1", "{name}");
            assert_eq!(live_memos(&p), 4, "{name}");
        } else {
            assert_d1_shape(&p, 100, name);
        }
    }
}

/// A forged cross-memo edit F (memo K, base M's create) whose text equals
/// the losing a1 and whose op id is smaller.
///
/// History: G3 blocker p6-b1. The removed misroute pass cut the honest
/// edits addressed to copy(M, a1), so x2 (chain tip) won the copy instead
/// of y1.
///
/// Adapted to the structural rule (`_sync/plan-amendments.md`, "Memo
/// lineage is structural"): F saw M's create, so it joins M's lineage and is
/// an ordinary same-text loser next to a1. The copy is named after the
/// smallest loser of the group, F (§4.4 / AC 2; the provisional-id case
/// handed to G8/G9), and the honest x1/x2/y1 still compete on a1: the copy
/// shows y1 and its own copy (loser x1) shows x2, in every order.
#[test]
fn g3_p6_r5b_forged_name_collision_does_not_cut_honest_copy_edits() {
    let copy = conflict_copy_id(M, &oid(102));
    let honest = d1_world(100, [&copy, &copy, &copy]);
    let mk = |base_op: u64| at(me(100, "f", T0 + 1, &[("a", 1)], K, base_op, "a1"), 1);
    let mut wm = honest.clone();
    wm.push(mk(99));
    let missing = order_free(&wm, 50, 0xE4);
    assert_d1_shape(&missing, 100, "missing base");
    let mut w = honest;
    w.push(mk(101));
    let p = order_free(&w, 300, 0xE3);
    let named = conflict_copy_id(M, &oid(100));
    assert_eq!(memo_of(&p, M).map(|m| m.text), Some("b1".to_owned()));
    let c = memo_of(&p, &named).unwrap_or_else(|| panic!("copy missing; texts {:?}", memo_texts(&p)));
    assert_eq!((c.text.as_str(), c.head_op_id, c.conflict_of.as_deref()), ("y1", oid(107), Some(M)));
    let n = memo_of(&p, &conflict_copy_id(&named, &oid(105))).unwrap_or_else(|| panic!("nested missing; texts {:?}", memo_texts(&p)));
    assert_eq!((n.text.as_str(), n.head_op_id), ("x2", oid(106)));
    assert_eq!(live_memos(&p), 3, "texts {:?}", memo_texts(&p));
    for n in [100, 102, 107] {
        assert_eq!(p.memo_lineage[&oid(n)], named, "op {n}");
    }
    // F merges into the copy as a1's text; y1 built on a1 did not see F.
    // That is the pre-existing same-text rule (g3_r12/r13, as op 12 in
    // g3_p6_r6), and F's text is visible in no memo only because it equals
    // a1's superseded text.
    assert_eq!(hidden(&w, &p), vec![oid(100)], "hidden");
    assert!(unrouted(&w, &p).is_empty());
}

/// r5c, the same collision one level down: inside C, x1 and y1 race on a1
/// and x1 loses into C's copy N = copy(C, x1); on N, x2 and v1 race on x1
/// (x2 newer) and v2 continues v1 with the newest HLC. A forged K edit with
/// x1's text, a smaller id than x1 and base a1 joins the race on a1.
///
/// History: under the removed misroute pass the forged edit named N's group
/// after itself for one pass, cutting x2/v1 so v2 won N; delta 3 resolved it
/// only because a causal-past filter made the forged edit (ctx a:1, blind to
/// a1 at (a, 2)) an orphan of K.
///
/// Re-baselined when that unrecorded causal-past filter was removed (G3
/// delta 3, architect b1, leader decision): an edit that names `base_op_id`
/// X has seen X by construction; causality across a fork range is decided
/// only by the edit-vs-delete `saw`/obs rule, not by lineage. The forged edit
/// is now a same-text loser next to x1, so the nested copy is named after
/// it, copy(C, forged) (§4.4 / AC 2 naming; the provisional-id case owned by
/// G8/G9), and still shows x2 with v2 in its own copy; it equals the same op
/// addressed to C.
#[test]
fn g3_p6_r5c_forged_name_collision_on_a_nested_copy() {
    let copy = conflict_copy_id(M, &oid(102));
    let nested = conflict_copy_id(&copy, &oid(105));
    let mut honest = d1_world(100, [&copy, &nested, &copy]);
    honest.push(at(me(108, "c", T0 + 8, &[("a", 3), ("b", 1)], &nested, 105, "v1"), 1));
    honest.push(at(me(109, "c", T0 + 30, &[("a", 3), ("b", 1), ("c", 1)], &nested, 108, "v2"), 2));
    let mk = |memo: &str, base_op: u64| at(me(104, "f", T0 + 3, &[("a", 1)], memo, base_op, "x1"), 1);
    let check = |p: &Projection, nested: &str, name: &str| {
        assert_eq!(memo_of(p, &copy).map(|m| m.text), Some("y1".to_owned()), "{name}");
        assert_eq!(memo_of(p, nested).map(|m| m.text), Some("x2".to_owned()), "{name}: texts {:?}", memo_texts(p));
        assert_eq!(memo_of(p, &conflict_copy_id(nested, &oid(108))).map(|m| m.text), Some("v2".to_owned()), "{name}");
    };
    let mut wm = honest.clone();
    wm.push(mk(K, 99));
    let missing = order_free(&wm, 50, 0xE6);
    check(&missing, &nested, "missing base");
    let mut w = honest.clone();
    w.push(mk(K, 102));
    let p = order_free(&w, 300, 0xE5);
    check(&p, &conflict_copy_id(&copy, &oid(104)), "forged base on a1");
    assert!(memo_of(&p, &nested).is_none(), "the nested copy is renamed, not duplicated");
    assert_eq!(live_memos(&p), 4, "M, C, the nested copy and its copy; K has no create: {:?}", memo_texts(&p));
    assert!(invisible(&w, &p).is_empty(), "{:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty());
    let mut wc = honest;
    wc.push(mk(&copy, 102));
    assert_eq!(view(&p), view(&order_free(&wc, 50, 0xE7)), "equals the same op addressed to C");
}

/// r5d (honest history, architect advisory a3): a late concurrent edit h of
/// M with a1's text and a smaller op id is a legitimate same-text loser, so
/// M's copy is now named copy(M, h) (§4.4: identical losing texts share the
/// copy of the smallest loser; the provisional-id case owned by G8/G9). The
/// follow-ups x1/x2/y1 written earlier against copy(M, a1) follow their base
/// structurally into copy(M, h): it shows y1 with `conflict_of` M, its own
/// copy shows x2, and no stale copy(M, a1) is left (3 live memos).
///
/// History: the removed misroute pass used to cut those follow-ups into a
/// standalone copy(M, a1); kept as a regression.
#[test]
fn g3_p6_r5d_late_same_text_loser_renames_the_copy_and_keeps_its_edits() {
    let old = conflict_copy_id(M, &oid(102));
    let mut w = d1_world(100, [&old, &old, &old]);
    w.push(at(me(100, "h", T0 + 1, &[("a", 1)], M, 101, "a1"), 1));
    let p = order_free(&w, 300, 0xE7);
    let new = conflict_copy_id(M, &oid(100));
    let m = memo_of(&p, &new).unwrap_or_else(|| panic!("renamed copy missing; texts {:?}", memo_texts(&p)));
    let n = memo_of(&p, &conflict_copy_id(&new, &oid(105)));
    assert_eq!(
        (m.text.as_str(), m.conflict_of.as_deref(), n.map(|n| n.text), live_memos(&p)),
        ("y1", Some(M), Some("x2".to_owned()), 3),
        "texts {:?}, stale entity {:?}",
        memo_texts(&p),
        memo_of(&p, &old)
    );
}

/// 300 random permutations of honest copy edits at three levels mixed with
/// every forged form above (cross-memo into copies, copy edits on other
/// memos, cycles, misroutes with continuations, name collisions, a late
/// same-text loser): one hash, notices and routing; nothing invisible.
///
/// Re-baselined when the unrecorded causal-past filter was removed (G3
/// delta 3, architect b1, leader decision): this asserted "b1" visible
/// because fc2 (ctx f:3, blind to b1) was an orphan of C2. An edit that
/// names `base_op_id` X has seen X by construction; causality across a fork
/// range is decided only by the edit-vs-delete `saw`/obs rule, not by
/// lineage. So fc2 continues M's lineage on b1 and supersedes it there, and
/// fk1 (blind to y1) races misK on y1 instead of being an orphan of K.
#[test]
fn g3_p6_r6_honest_copy_edits_and_forged_bases_under_300_permutations() {
    let (mut w, [c1, c2, _]) = deep_world(None);
    w.extend([
        at(mc(40, "k", T0 - 5, &[], K, "kay"), 1),
        at(me(50, "f", T0 + 100, &[("a", 4)], K, 7, "fk1"), 1),
        at(me(51, "f", T0 + 101, &[("f", 1)], K, 6, "fk2"), 2),
        at(me(52, "f", T0 + 102, &[("f", 2)], &c1, 40, "fc1"), 3),
        at(me(53, "f", T0 + 103, &[("f", 3)], &c2, 3, "fc2"), 4),
        at(me(20, "e", T0 + 60, &[("a", 2), ("b", 1)], &c1, 21, "cyc20"), 1),
        at(me(21, "g", T0 + 61, &[("a", 2), ("b", 1)], &c1, 20, "cyc21"), 1),
        at(me(23, "u", T0 + 63, &[("a", 1)], M, 24, "xcycM"), 1),
        at(me(24, "v", T0 + 64, &[("k", 1)], K, 23, "xcycK"), 1),
        at(me(25, "k", T0 + 65, &[("k", 1), ("b", 2)], K, 7, "misK"), 2),
        at(me(26, "k", T0 + 66, &[("k", 2), ("b", 2)], K, 25, "misK2"), 3),
        // Collision: K edit with x1's text and an id below x1 on a1.
        at(me(4, "q", T0 + 3, &[("a", 2)], K, 2, "x1"), 1),
        // A late honest same-text loser of a1 (larger id: no rename).
        at(me(12, "h", T0 + 1, &[("a", 1)], M, 1, "a1"), 1),
    ]);
    let p = order_free(&w, 300, 0x300);
    // h (op 12) merges into a1's copy as the same text; y1 built on a1 did
    // not see h. That is the pre-existing same-text rule (g3_r12/r13), not a
    // delta effect: the honest world with h alone reports the same.
    // Structural rule (`_sync/plan-amendments.md`, "Memo lineage is
    // structural"): the K collision op 4 saw a1, so it joins the race on a1
    // as a same-text loser next to x1 (op 5); x2 built on x1 did not see op
    // 4. The same pre-existing rule, so the honest world with op 4 addressed
    // to C1 hides it too. This asserted only op 12 while op 4 was misrouted.
    assert_eq!(hidden(&w, &p), vec![oid(4), oid(12)], "hidden");
    let (mut hw, _) = deep_world(None);
    hw.push(at(me(12, "h", T0 + 1, &[("a", 1)], M, 1, "a1"), 1));
    assert_eq!(hidden(&hw, &order_free(&hw, 20, 0x301)), vec![oid(12)], "honest world with h");
    let (mut hw, _) = deep_world(None);
    hw.push(at(me(4, "q", T0 + 3, &[("a", 2)], &c1, 2, "x1"), 1));
    assert_eq!(hidden(&hw, &order_free(&hw, 20, 0x302)), vec![oid(4)], "honest world with op 4 on C1");
    assert!(unrouted(&w, &p).is_empty(), "unrouted: {:?}", unrouted(&w, &p));
    let texts = memo_texts(&p);
    // fk2 saw fk1 (same device), so fk1 is superseded, not hidden. Under the
    // structural rule misK saw y1 (b, 2) and continues C1's lineage on it,
    // so y1 is superseded by misK2 (no longer listed), not hidden: `hidden`
    // above is empty for it.
    for t in ["z2", "w2", "fk2", "fc1", "fc2", "cyc20", "cyc21", "xcycM", "xcycK", "misK2"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    // fk1 (ctx a:4, blind to y1 at (b, 2)) was an orphan of K; it now races
    // misK on y1 and wins on HLC, so misK (with misK2) is a copy of y1's memo.
    let y1_owner = p.memo_lineage[&oid(7)].clone();
    assert_eq!(p.memo_lineage[&oid(50)], y1_owner, "fk1 continues y1's lineage");
    let mis = memo_of(&p, &p.memo_lineage[&oid(25)]).unwrap();
    assert_eq!((mis.text.as_str(), mis.conflict_of), ("misK2", Some(y1_owner)), "misK loses on y1 into a copy");
    assert_eq!(p.memo_lineage[&oid(26)], p.memo_lineage[&oid(25)], "misK2 follows misK");
    assert_eq!((p.memo_lineage[&oid(53)].as_str(), p.memo_lineage[&oid(3)].as_str()), (M, M), "fc2 continues b1 in M");
    assert_eq!(memo_of(&p, M).unwrap().text, "fc2");
    assert!(invisible(&w, &p).is_empty(), "invisible: {:?}", invisible(&w, &p));
}

// ------------------------------------ structural lineage (p7, delta 3)

/// Id offset of `deep7`; ids below it are free for late same-text losers.
const OFF: u64 = 10;

/// Four copy levels with chains under root memo `m` (ids `OFF+1..OFF+15`).
/// Every follow-up addresses the memo its writer saw, or `addr` for all.
/// §4.4 result: m = b2, C1 = y1, C2 = z2, C3 = r1, C4 = t2, each race
/// decided at its shared base, never by a chain tip.
fn deep7(m: &str, addr: Option<&str>) -> (Vec<Pub>, [String; 4]) {
    let n = |k: u64| OFF + k;
    let c1 = conflict_copy_id(m, &oid(n(2)));
    let c2 = conflict_copy_id(&c1, &oid(n(5)));
    let c3 = conflict_copy_id(&c2, &oid(n(9)));
    let c4 = conflict_copy_id(&c3, &oid(n(13)));
    let a = |own: &str| addr.unwrap_or(own).to_owned();
    let (am, a1, a2, a3, a4) = (a(m), a(&c1), a(&c2), a(&c3), a(&c4));
    let w = vec![
        at(mc(n(1), "a", T0, &[], m, "base"), 1),
        at(me(n(2), "a", T0 + 1, &[("a", 1)], &am, n(1), "a1"), 2),
        at(me(n(3), "b", T0 + 2, &[("a", 1)], &am, n(1), "b1"), 1),
        at(me(n(15), "b", T0 + 7, &[("a", 2), ("b", 2)], &am, n(3), "b2"), 3),
        // C1: x1 vs y1 on a1; x2 continues x1 with the newest HLC.
        at(me(n(5), "a", T0 + 4, &[("a", 2), ("b", 1)], &a1, n(2), "x1"), 3),
        at(me(n(6), "a", T0 + 10, &[("a", 3), ("b", 1)], &a1, n(5), "x2"), 4),
        at(me(n(7), "b", T0 + 6, &[("a", 2), ("b", 1)], &a1, n(2), "y1"), 2),
        // C2 (x1 ← x2): z1 vs w1 on x2; w2 continues w1, z2 continues z1.
        at(me(n(8), "c", T0 + 20, &[("a", 4), ("b", 2)], &a2, n(6), "z1"), 1),
        at(me(n(9), "d", T0 + 15, &[("a", 4), ("b", 2)], &a2, n(6), "w1"), 1),
        at(me(n(10), "d", T0 + 40, &[("a", 4), ("b", 2), ("d", 1)], &a2, n(9), "w2"), 2),
        at(me(n(11), "c", T0 + 21, &[("a", 4), ("b", 2), ("c", 1)], &a2, n(8), "z2"), 2),
        // C3 (w1 ← w2): r1 vs t1 on w2; t2 continues t1 with the newest HLC.
        at(me(n(12), "r", T0 + 50, &[("a", 4), ("b", 2), ("d", 2)], &a3, n(10), "r1"), 1),
        at(me(n(13), "t", T0 + 45, &[("a", 4), ("b", 2), ("d", 2)], &a3, n(10), "t1"), 1),
        at(me(n(14), "t", T0 + 70, &[("a", 4), ("b", 2), ("d", 2), ("t", 1)], &a4, n(13), "t2"), 2),
    ];
    (w, [c1, c2, c3, c4])
}

/// Late concurrent same-text losers, one per level of `deep7(m)`, each with
/// an id below the loser it joins, addressed to the (old) memo it saw.
fn late7(m: &str) -> Vec<Pub> {
    let (_, [c1, c2, c3, _]) = deep7(m, None);
    vec![
        at(me(3, "h1", T0 + 1, &[("a", 1)], m, OFF + 1, "a1"), 1),
        at(me(4, "h2", T0 + 3, &[("a", 2), ("b", 1)], &c1, OFF + 2, "x1"), 1),
        at(me(5, "h3", T0 + 14, &[("a", 4), ("b", 2)], &c2, OFF + 6, "w1"), 1),
        at(me(6, "h4", T0 + 44, &[("a", 4), ("b", 2), ("d", 2)], &c3, OFF + 10, "t1"), 1),
    ]
}

/// §4.4 shape of `deep7` under copy names `c`.
fn assert_deep7(p: &Projection, m: &str, c: &[String; 4], name: &str) {
    let expect = [
        (m, "b2", OFF + 15, None),
        (c[0].as_str(), "y1", OFF + 7, Some(m)),
        (c[1].as_str(), "z2", OFF + 11, Some(c[0].as_str())),
        (c[2].as_str(), "r1", OFF + 12, Some(c[1].as_str())),
        (c[3].as_str(), "t2", OFF + 14, Some(c[2].as_str())),
    ];
    for (id, text, head, of) in expect {
        let got = memo_of(p, id).unwrap_or_else(|| panic!("{name}: {id} missing; texts {:?}", memo_texts(p)));
        assert_eq!((got.text.as_str(), got.head_op_id, got.conflict_of.as_deref()), (text, oid(head), of), "{name}: {id}");
    }
    let owners: [(&[u64], &str); 5] =
        [(&[1, 3, 15], m), (&[2, 7], &c[0]), (&[5, 6, 8, 11], &c[1]), (&[9, 10, 12], &c[2]), (&[13, 14], &c[3])];
    for (ops, owner) in owners {
        for k in ops {
            assert_eq!(p.memo_lineage[&oid(OFF + k)], owner, "{name}: op {}", OFF + k);
        }
    }
}

/// `w` with every memo edit outside `keep` re-addressed to a random memo of
/// `pool`. Under the structural rule this never changes the projection of
/// an edit whose base is a seen lineage op.
fn readdress(w: &[Pub], keep: &BTreeSet<String>, pool: &[&str], rng: &mut Lcg) -> Vec<Pub> {
    w.iter()
        .map(|(b, d)| {
            let mut b = b.clone();
            if b.kind == Kind::Memo && b.op_type == "edit" && !keep.contains(&b.op_id) {
                b.entity_id = pool[(rng.step() % pool.len() as u64) as usize].to_owned();
            }
            (b, d.clone())
        })
        .collect()
}

/// Memo id `id` named by the path of loser op ids from a root label, so two
/// projections that differ only in root memo ids can be compared.
fn canon_name(id: &str, roots: &[(&str, &str)], parent: &BTreeMap<String, (String, String)>, depth: usize) -> String {
    if let Some((_, label)) = roots.iter().find(|(r, _)| *r == id) {
        return (*label).to_owned();
    }
    match parent.get(id) {
        Some((memo, loser)) if depth < 32 => format!("{}/{loser}", canon_name(memo, roots, parent, depth + 1)),
        _ => format!("?{id}"),
    }
}

type Shape = (BTreeSet<(String, String, String, Option<String>)>, BTreeSet<String>, BTreeMap<String, String>);

fn shape(p: &Projection, roots: &[(&str, &str)]) -> Shape {
    let parent: BTreeMap<String, (String, String)> = p
        .notices
        .iter()
        .filter_map(|n| match n {
            Notice::MemoConflictCopy { memo_id, copy_id, loser_op_id } => {
                Some((copy_id.clone(), (memo_id.clone(), loser_op_id.clone())))
            }
            _ => None,
        })
        .collect();
    let name = |id: &str| canon_name(id, roots, &parent, 0);
    let entities = p
        .entities
        .iter()
        .map(|((kind, id), st)| match st {
            EntityState::Memo(m) => (name(id), m.text.clone(), m.head_op_id.clone(), m.conflict_of.as_deref().map(name)),
            EntityState::Deleted => (name(id), format!("<deleted {}>", kind.as_str()), String::new(), None),
            other => (name(id), format!("{other:?}"), String::new(), None),
        })
        .collect();
    let notices = p
        .notices
        .iter()
        .map(|n| match n {
            Notice::MemoConflictCopy { memo_id, copy_id, loser_op_id } => {
                format!("copy {} -> {} ({loser_op_id})", name(memo_id), name(copy_id))
            }
            Notice::RestoredAfterDeleteConflict { kind, entity_id } => format!("restored {} {}", kind.as_str(), name(entity_id)),
        })
        .collect();
    let lineage = p.memo_lineage.iter().map(|(op, owner)| (op.clone(), name(owner))).collect();
    (entities, notices, lineage)
}

/// Root of `id`'s copy chain (follows `conflict_of`).
fn copy_root(p: &Projection, id: &str) -> String {
    let mut cur = id.to_owned();
    for _ in 0..32 {
        match memo_of(p, &cur).and_then(|m| m.conflict_of) {
            Some(up) => cur = up,
            None => break,
        }
    }
    cur
}

fn ids(ns: &[u64]) -> Vec<String> {
    let mut v: Vec<String> = ns.iter().map(|n| oid(*n)).collect();
    v.sort();
    v
}

/// Honest copy edits across four levels with chains, addressed as their
/// writers saw them, to one memo for all, and per op at random (including
/// another memo and a memo that does not exist): one §4.4 result.
#[test]
fn g3_p7_s1_four_copy_levels_route_by_base_alone_under_any_addressing() {
    let (w, c) = deep7(M, None);
    let p = order_free(&w, 400, 0x7001);
    assert_deep7(&p, M, &c, "copy-addressed");
    assert_eq!(live_memos(&p), 5, "texts {:?}", memo_texts(&p));
    assert_eq!(copies(&p).len(), 4);
    assert!(hidden(&w, &p).is_empty(), "hidden {:?}", hidden(&w, &p));
    assert!(unrouted(&w, &p).is_empty());
    for all in [M, K, c[0].as_str(), c[3].as_str(), "ghost-memo"] {
        let (wa, _) = deep7(M, Some(all));
        assert_eq!(view(&order_free(&wa, 50, 0x7002)), view(&p), "all addressed to {all}");
    }
    let pool = [M, K, c[0].as_str(), c[1].as_str(), c[2].as_str(), c[3].as_str(), "ghost-memo"];
    let mut rng = Lcg(0x7003);
    for round in 0..40 {
        let wr = readdress(&w, &BTreeSet::new(), &pool, &mut rng);
        assert_eq!(view(&order_free(&wr, 5, 0x7004 + round)), view(&p), "random addressing round {round}");
    }
}

/// A late same-text loser with a smaller op id at every level renames that
/// level's copy and, through the copy-of-copy naming, every copy below it.
/// The follow-ups addressed to the old names keep the §4.4 shape; the old
/// names vanish. The late losers themselves merge as identical text (the
/// pre-existing same-text rule: each copy head never saw them).
#[test]
fn g3_p7_s2_late_same_text_losers_at_every_level_rename_the_whole_chain() {
    let (mut w, old) = deep7(M, None);
    w.extend(late7(M));
    let p = order_free(&w, 400, 0x7010);
    let l1 = conflict_copy_id(M, &oid(3));
    let l2 = conflict_copy_id(&l1, &oid(4));
    let l3 = conflict_copy_id(&l2, &oid(5));
    let l4 = conflict_copy_id(&l3, &oid(6));
    let renamed = [l1, l2, l3, l4];
    assert_deep7(&p, M, &renamed, "late losers");
    for (k, owner) in renamed.iter().enumerate() {
        assert_eq!(&p.memo_lineage[&oid(3 + k as u64)], owner, "late loser {}", 3 + k);
    }
    assert_eq!(live_memos(&p), 5, "texts {:?}", memo_texts(&p));
    assert_eq!(copies(&p).len(), 4, "{:?}", copies(&p));
    for id in &old {
        assert!(memo_of(&p, id).is_none(), "stale copy {id} still shown");
    }
    assert_eq!(hidden(&w, &p), ids(&[3, 4, 5, 6]), "only the merged late losers");
    assert!(unrouted(&w, &p).is_empty());
    let pool = [M, K, old[0].as_str(), renamed[2].as_str(), "ghost-memo"];
    let mut rng = Lcg(0x7011);
    for round in 0..40 {
        let wr = readdress(&w, &BTreeSet::new(), &pool, &mut rng);
        assert_eq!(view(&order_free(&wr, 5, 0x7012 + round)), view(&p), "random addressing round {round}");
    }
}

/// Every forged cross-memo base into the copies of `deep7(m)` at once, from
/// one device that saw everything (with a forged chain and a forged edit
/// with the winner's text on C1's race base), plus op 56 from a device that
/// saw little: its base is memo node w2, so it is w2's child in C3 by
/// structure (an edit naming a base has seen it). Returns the forged ops and
/// op 56's id.
fn forged7(m: &str, k: &str) -> (Vec<Pub>, String) {
    let (_, [c1, c2, c3, _]) = deep7(m, None);
    let seen = [("a", 4), ("b", 3), ("c", 2), ("d", 2), ("r", 1), ("t", 2), ("k", 1)];
    let f = |n: u64, seq: u64, memo: &str, base: u64, text: &str| {
        let mut ctx = seen.to_vec();
        if seq > 1 {
            ctx.push(("f", seq - 1));
        }
        at(me(n, "f", T0 + 100 + seq, &ctx, memo, base, text), seq)
    };
    let w = vec![
        at(mc(40, "k", T0 - 5, &[], k, "kay"), 1),
        f(50, 1, k, OFF + 7, "fk1"),
        f(51, 2, k, OFF + 6, "fk2"),
        f(52, 3, k, OFF + 14, "fk3"),
        f(53, 4, &c1, 40, "fc1"),
        f(54, 5, &c2, OFF + 15, "fc2"),
        f(55, 6, k, 50, "fk1b"),
        f(57, 7, k, OFF + 2, "y1"),
        at(me(56, "g", T0 + 120, &[("a", 1)], &c3, OFF + 10, "blind"), 1),
    ];
    (w, oid(56))
}

/// Forged cross-memo bases into copies, all at once: entity_id never
/// matters for a seen base (random re-addressing gives one projection), a
/// edit's lineage follows its base whatever its ctx says (op 56 is w2's
/// child and wins C3's race on HLC), nothing is
/// unrouted, and the only "hidden" op is op 57, the winner of C1's race
/// whose text equals y1's: y1's child continues the lineage without having
/// seen it (the pre-existing same-text rule).
#[test]
fn g3_p7_s3_forged_cross_memo_bases_into_copies_all_at_once() {
    let (mut w, [c1, c2, c3, c4]) = deep7(M, None);
    let (forged, blind) = forged7(M, K);
    w.extend(forged);
    let p = order_free(&w, 400, 0x7020);
    assert!(unrouted(&w, &p).is_empty(), "unrouted {:?}", unrouted(&w, &p));
    assert_eq!(hidden(&w, &p), ids(&[57]), "hidden");
    let texts = memo_texts(&p);
    for t in ["fk2", "fk3", "fc1", "fc2", "fk1b", "blind"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert_eq!(p.memo_lineage[&oid(53)], K, "fc1 joins K's lineage");
    assert_eq!(p.memo_lineage[&oid(54)], M, "fc2 continues M's head");
    assert_eq!(p.memo_lineage[&blind], c3, "op 56 is w2's child and shows in C3");
    let keep: BTreeSet<String> = [blind.clone()].into();
    let pool = [M, K, c1.as_str(), c2.as_str(), c3.as_str(), c4.as_str(), "ghost-memo"];
    let mut rng = Lcg(0x7021);
    for round in 0..40 {
        let wr = readdress(&w, &keep, &pool, &mut rng);
        assert_eq!(view(&order_free(&wr, 5, 0x7022 + round)), view(&p), "random addressing round {round}");
    }
    let missing: Vec<Pub> = w
        .iter()
        .map(|(b, d)| if b.op_id == blind { (me(56, "g", T0 + 120, &[("a", 1)], &c3, 99, "blind"), d.clone()) } else { (b.clone(), d.clone()) })
        .collect();
    // Not a rule: with its base missing, op 56 is an orphan of C3 and lands in
    // the same place only because r1 and t1 route identically in both shapes.
    assert_eq!(view(&order_free(&missing, 50, 0x7023)), view(&p), "same result here by coincidence");
}

/// Cycles spanning two (and three) memos: M ↔ K, M → K → C1 → M, a cycle
/// whose members claim to have seen each other, a member with K's current
/// text, and a descendant of a cycle member addressed to C2.
fn cycles7(m: &str, k: &str, bases: Option<[u64; 7]>) -> Vec<Pub> {
    let (_, [c1, c2, _, _]) = deep7(m, None);
    let b = |i: usize, real: u64| bases.map_or(real, |bs| bs[i]);
    vec![
        at(me(60, "u", T0 + 80, &[("a", 2), ("b", 1)], m, b(0, 61), "xm"), 1),
        at(me(61, "v", T0 + 81, &[("k", 1)], k, b(1, 60), "xk"), 1),
        at(me(62, "w1", T0 + 82, &[("a", 1)], m, b(2, 63), "ym"), 1),
        at(me(63, "w2", T0 + 83, &[("k", 1)], k, b(3, 64), "kay"), 1),
        at(me(64, "w3", T0 + 84, &[("a", 2), ("b", 1)], &c1, b(4, 62), "yc"), 1),
        at(me(65, "z1", T0 + 85, &[("z2", 1)], m, b(5, 66), "zm"), 1),
        at(me(66, "z2", T0 + 86, &[("z1", 1)], k, b(6, 65), "zk"), 1),
    ]
    .into_iter()
    .chain(bases.is_none().then(|| at(me(67, "u", T0 + 87, &[("u", 1)], &c2, 60, "desc"), 2)))
    .collect()
}

/// Re-baselined with the structural forest (G3 delta 3, leader decision):
/// this asserted "xm" shown because the cycle descendant `desc` (base xm)
/// was flattened into an orphan head of C2. Only cycle members become
/// roots; `desc` stays a child of xm and supersedes it in xm's own lineage
/// (plan-amendments "Memo lineage is structural"). An edit that names
/// `base_op_id` X has seen X by construction; causality across a fork range
/// is decided only by the edit-vs-delete `saw`/obs rule, not by lineage.
#[test]
fn g3_p7_s4_cycles_spanning_two_memos() {
    let (honest, _) = deep7(M, None);
    let kc = at(mc(40, "k", T0 - 5, &[], K, "kay"), 1);
    let mut w = honest.clone();
    w.push(kc.clone());
    w.extend(cycles7(M, K, None));
    let p = order_free(&w, 400, 0x7030);
    assert!(hidden(&w, &p).is_empty(), "hidden {:?}", hidden(&w, &p));
    assert!(unrouted(&w, &p).is_empty(), "unrouted {:?}", unrouted(&w, &p));
    let texts = memo_texts(&p);
    for t in ["xk", "ym", "kay", "yc", "zm", "zk", "desc"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert!(invisible(&w, &p).is_empty(), "invisible {:?}", invisible(&w, &p));
    assert_eq!(p.memo_lineage[&oid(67)], p.memo_lineage[&oid(60)], "desc continues cycle member xm");
    // Each cycle member stays with its own memo or a copy of it.
    for (n, own) in [(60, M), (61, K), (62, M), (63, K), (65, M), (66, K)] {
        assert_eq!(copy_root(&p, &p.memo_lineage[&oid(n)]), own, "cycle member {n}");
    }
    let (_, [c1, _, _, _]) = deep7(M, None);
    assert_eq!(copy_root(&p, &p.memo_lineage[&oid(64)]), M, "C1 member is in C1's family");
    let c1_family = |id: &str| {
        let mut cur = id.to_owned();
        for _ in 0..32 {
            if cur == c1 {
                return true;
            }
            match memo_of(&p, &cur).and_then(|m| m.conflict_of) {
                Some(up) => cur = up,
                None => return false,
            }
        }
        false
    };
    assert!(c1_family(&p.memo_lineage[&oid(64)]), "C1-addressed member in C1 or a copy of C1");
    // `desc` (addressed to C2) follows its base xm, not its `entity_id`.
    assert_eq!(copy_root(&p, &p.memo_lineage[&oid(67)]), M, "desc in xm's memo M or a copy of M");
    // Without the descendant, a cycle is exactly its members with missing bases.
    let mut cyc = honest.clone();
    cyc.push(kc.clone());
    cyc.extend(cycles7(M, K, Some([61, 60, 63, 64, 62, 66, 65])));
    let mut miss = honest;
    miss.push(kc);
    miss.extend(cycles7(M, K, Some([991, 992, 993, 994, 995, 996, 997])));
    let pc = order_free(&cyc, 200, 0x7031);
    assert_eq!(view(&pc), view(&order_free(&miss, 50, 0x7032)), "cycle == missing bases");
}

/// The honest world of `self_race`: R's create, a1 and b1 race on it (b1
/// wins, a1 → copy C); in C, y1 continues a1 with the newest HLC and text
/// "q". O1 ("p") and O2 ("q") are edits of C that saw a1 but whose base is
/// themselves (`self_base`) or missing (a held stream). Neither saw y1.
/// With `seen`, O2 has its own text "r" instead, and y1 saw O2.
fn self_race(r: &str, self_base: bool, seen: bool) -> (Vec<Pub>, String) {
    let c = conflict_copy_id(r, &oid(2));
    let ob = |n: u64, missing: u64| if self_base { n } else { missing };
    let w = vec![
        at(mc(1, "a", T0, &[], r, "base"), 1),
        at(me(2, "a", T0 + 1, &[("a", 1)], r, 1, "a1"), 2),
        at(me(3, "b", T0 + 2, &[("a", 1)], r, 1, "b1"), 1),
        at(me(4, "a", T0 + 60, if seen { &[("a", 2), ("b", 1), ("g", 1)] } else { &[("a", 2), ("b", 1)] }, &c, 2, "q"), 3),
        at(me(5, "e", T0 + 30, &[("a", 2), ("b", 1)], &c, ob(5, 97), "p"), 1),
        at(me(6, "g", T0 + 20, &[("a", 2), ("b", 1)], &c, ob(6, 98), if seen { "r" } else { "q" }), 1),
    ];
    (w, c)
}

/// A self base next to a same-base race, on the root memo: equal to a
/// missing base, visible, order-free.
#[test]
fn g3_p7_s5a_self_base_with_a_same_base_race_on_the_root() {
    let world = |b4: u64, b5: u64, b6: u64| {
        vec![
            at(mc(1, "a", T0, &[], M, "base"), 1),
            at(me(2, "a", T0 + 1, &[("a", 1)], M, 1, "a1"), 2),
            at(me(3, "b", T0 + 2, &[("a", 1)], M, 1, "b1"), 1),
            at(me(4, "s", T0 + 3, &[("a", 1)], M, b4, "s"), 1),
            at(me(5, "s2", T0 + 1, &[("a", 1)], M, b5, "a1"), 1),
            at(me(6, "s3", T0 + 9, &[("a", 2), ("b", 1)], M, b6, "s3"), 1),
        ]
    };
    let w = world(4, 5, 6);
    let p = order_free(&w, 400, 0x7040);
    let missing = order_free(&world(94, 95, 96), 100, 0x7041);
    assert_eq!(view(&p), view(&missing), "self base == missing base");
    assert!(unrouted(&w, &p).is_empty());
    let texts = memo_texts(&p);
    for t in ["s", "s3", "a1"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert_eq!(memo_of(&p, M).unwrap().text, "s3", "s3 saw b1 and is newest");
}

/// Orphan heads of copy C (O1 "p", O2 with the text of C's winner y1, or
/// with its own text "r" that y1 saw), under root ids that sort before and
/// after C: C shows "q", only O1 gets a copy (§4.4 / AC 2: O2 is same-text
/// or superseded), 3 live memos, one shape for every root id. Honest with
/// a missing base (held stream); also with a self base.
///
/// History: G3 blocker p7-b1. The removed queue router walked C
/// orphan-only when C's id sorted before R's, split O2 into copy(C, O2)
/// and never retracted that walk or its notice, so a stale "q" (or "r")
/// copy appeared depending only on how memo ids sort. The fixpoint routing
/// (G3 delta 4) resolves every entity afresh each round, so nothing from an
/// earlier round survives.
#[test]
fn g3_p7_s5b_copy_resolved_before_its_parent_leaves_no_stale_orphan_copy() {
    let mut failures = Vec::new();
    for (self_base, seen) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut shapes = Vec::new();
        for r in ["0", M, "zz"] {
            let (w, c) = self_race(r, self_base, seen);
            let p = order_free(&w, 100, 0x7050);
            let o1 = conflict_copy_id(&c, &oid(5));
            let o2 = conflict_copy_id(&c, &oid(6));
            let got = (
                memo_of(&p, r).map(|m| m.text),
                memo_of(&p, &c).map(|m| (m.text, m.conflict_of)),
                memo_of(&p, &o1).map(|m| m.text),
                memo_of(&p, &o2).map(|m| m.text),
                live_memos(&p),
            );
            let want = (
                Some("b1".to_owned()),
                Some(("q".to_owned(), Some(r.to_owned()))),
                Some("p".to_owned()),
                None,
                3,
            );
            if got != want {
                let sorts = if c.as_str() < r { "first" } else { "after" };
                failures.push(format!("root {r} (C sorts {sorts}), self base {self_base}, seen {seen}: {got:?}"));
            }
            shapes.push(shape(&p, &[(r, "R")]));
        }
        if shapes.windows(2).any(|s| s[0] != s[1]) {
            failures.push(format!("self base {self_base}, seen {seen}: shape depends on the root id"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Delete fate when a cross-memo edit joins memo K: the edit is K's
/// content (never M's), so a K delete blind to it is restored, one that saw
/// it wins, an M delete ignores it, and an edit that saw K's delete is set
/// aside. Every case equals the same edit addressed to K.
#[test]
fn g3_p7_s6_delete_fate_when_a_cross_memo_edit_joins_k() {
    let c = at(mc(1, "a", T0, &[], M, "base"), 1);
    let kc = at(mc(40, "k", T0 - 5, &[], K, "kay"), 1);
    let e = |memo: &str, ctx: &[(&str, u64)]| at(me(41, "e", T0 + 30, ctx, memo, 40, "e"), 1);
    let f = at(me(44, "k2", T0 + 40, &[("k", 1)], K, 40, "f"), 1);
    let d_k = |ctx: &[(&str, u64)]| at(md(42, "k", T0 + 20, ctx, K), 2);
    let d_m = at(md(43, "a", T0 + 35, &[("a", 1)], M), 2);
    type Case = (&'static str, Vec<(&'static str, u64)>, Vec<Pub>);
    let cases: Vec<Case> = vec![
        ("K delete blind to E", vec![("k", 1)], vec![d_k(&[("k", 1)])]),
        ("K delete saw E", vec![("k", 1)], vec![d_k(&[("k", 1), ("e", 1)])]),
        ("M delete blind to E", vec![("k", 1)], vec![d_m.clone()]),
        ("E saw K delete", vec![("k", 2)], vec![d_k(&[("k", 1)])]),
        ("E loses to f, K delete blind to both", vec![("k", 1)], vec![f.clone(), d_k(&[("k", 1)])]),
        ("E loses to f, K delete saw E only", vec![("k", 1)], vec![f.clone(), d_k(&[("k", 1), ("e", 1)])]),
        ("E loses to f, K delete saw f only", vec![("k", 1)], vec![f.clone(), d_k(&[("k", 1), ("k2", 1)])]),
    ];
    let mut out = BTreeMap::new();
    for (name, ectx, rest) in &cases {
        let build = |memo: &str| {
            let mut w = vec![c.clone(), kc.clone(), e(memo, ectx)];
            w.extend(rest.iter().cloned());
            w
        };
        let w = build(M);
        let p = order_free(&w, 100, 0x7060);
        let k_addressed = order_free(&build(K), 50, 0x7061);
        assert_eq!(view(&p), view(&k_addressed), "{name}: equals E addressed to K");
        assert_eq!(copy_root(&p, &p.memo_lineage[&oid(41)]), K, "{name}: E is in K's family");
        out.insert(*name, p);
    }
    let p = &out["K delete blind to E"];
    assert_eq!(memo_of(p, K).map(|m| m.text), Some("e".to_owned()));
    assert!(is_restored(p, Kind::Memo, K));
    assert_eq!(memo_of(p, M).map(|m| m.text), Some("base".to_owned()));
    let p = &out["K delete saw E"];
    assert!(is_deleted(p, Kind::Memo, K));
    assert!(p.notices.is_empty(), "{:?}", p.notices);
    let p = &out["M delete blind to E"];
    assert!(is_deleted(p, Kind::Memo, M), "E is K's content, not M's");
    assert_eq!(memo_of(p, K).map(|m| m.text), Some("e".to_owned()));
    assert!(!is_restored(p, Kind::Memo, M));
    let p = &out["E saw K delete"];
    assert!(is_deleted(p, Kind::Memo, K), "content that saw the delete is ignored");
    let p = &out["E loses to f, K delete blind to both"];
    assert_eq!(memo_of(p, K).map(|m| m.text), Some("f".to_owned()));
    assert!(is_restored(p, Kind::Memo, K));
    assert_eq!(memo_of(p, &conflict_copy_id(K, &oid(41))).map(|m| m.text), Some("e".to_owned()));
    let p = &out["E loses to f, K delete saw f only"];
    assert!(is_deleted(p, Kind::Memo, K));
    assert_eq!(memo_of(p, &conflict_copy_id(K, &oid(41))).map(|m| m.text), Some("e".to_owned()), "E's copy is visible");
    for (name, p) in &out {
        if *name != "E saw K delete" && *name != "K delete saw E" {
            assert!(memo_texts(p).contains("e"), "{name}: E visible");
        }
    }
}

/// Everything above in one op set (four copy levels, late same-text losers,
/// forged cross-memo bases into copies, cross-memo cycles with a
/// descendant, self bases next to a race, a cross-memo edit joining K with
/// a blind K delete), under memo ids `m` and `k`.
fn mixed7(m: &str, k: &str) -> (Vec<Pub>, BTreeSet<String>) {
    let (mut w, [c1, ..]) = deep7(m, None);
    w.extend(late7(m));
    let (forged, blind) = forged7(m, k);
    w.extend(forged);
    w.extend(cycles7(m, k, None));
    w.push(at(me(70, "s1", T0 + 3, &[("a", 1)], m, 70, "self-m"), 1));
    w.push(at(me(71, "s2", T0 + 5, &[("a", 2), ("b", 1)], &c1, 71, "self-c"), 1));
    w.push(at(me(80, "e", T0 + 90, &[("k", 1)], m, 40, "e"), 1));
    w.push(at(md(81, "k", T0 + 95, &[("k", 1)], k), 2));
    let odd: BTreeSet<String> = [blind].into_iter().chain([60, 61, 62, 63, 64, 65, 66, 67, 70, 71].iter().map(|n| oid(*n))).collect();
    (w, odd)
}

/// Re-baselined with the structural forest (G3 delta 3, leader decision):
/// this asserted "xm" shown because the cycle descendant `desc` (base xm)
/// was flattened into an orphan head of C2. Only cycle members become
/// roots; `desc` stays a child of xm and supersedes it in xm's own lineage
/// (plan-amendments "Memo lineage is structural"). An edit that names
/// `base_op_id` X has seen X by construction; causality across a fork range
/// is decided only by the edit-vs-delete `saw`/obs rule, not by lineage.
#[test]
fn g3_p7_s7_everything_mixed_under_400_permutations() {
    let (w, odd) = mixed7(M, K);
    let p = order_free(&w, 400, 0x7070);
    assert!(unrouted(&w, &p).is_empty(), "unrouted {:?}", unrouted(&w, &p));
    let allowed = ids(&[3, 4, 5, 6, 57]);
    let hid = hidden(&w, &p);
    assert!(hid.iter().all(|h| allowed.contains(h)), "hidden {hid:?}");
    let texts = memo_texts(&p);
    for t in ["fk2", "fk3", "fc2", "fk1b", "blind", "xk", "ym", "yc", "zm", "zk", "desc", "self-m", "self-c", "e"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert!(invisible(&w, &p).is_empty(), "invisible {:?}", invisible(&w, &p));
    assert_eq!(p.memo_lineage[&oid(67)], p.memo_lineage[&oid(60)], "desc continues cycle member xm");
    assert!(is_restored(&p, Kind::Memo, K), "the blind K delete is restored by unseen K content");
    let (_, [c1, c2, c3, c4]) = deep7(M, None);
    let pool = [M, K, c1.as_str(), c2.as_str(), c3.as_str(), c4.as_str(), "ghost-memo"];
    let mut rng = Lcg(0x7071);
    for round in 0..20 {
        let wr = readdress(&w, &odd, &pool, &mut rng);
        assert_eq!(view(&order_free(&wr, 5, 0x7072 + round)), view(&p), "random addressing round {round}");
    }
}

/// The mixed world under five root-id labelings (sorting before, between
/// and after their copy ids): one structure (regression lock for p7-b1
/// together with `g3_p7_s5b`).
#[test]
fn g3_p7_s8_mixed_world_shape_is_independent_of_memo_id_order() {
    let mut shapes = Vec::new();
    for (m, k) in [(M, K), ("0", "1"), ("zz", "zy"), ("0", "zz"), ("zz", "0")] {
        let (w, _) = mixed7(m, k);
        let p = order_free(&w, 20, 0x7080);
        // The pre-rename copy ids some edits still address (orphans keep them).
        let (_, [c1, c2, c3, c4]) = deep7(m, None);
        let roots = [(m, "M"), (k, "K"), (c1.as_str(), "C1"), (c2.as_str(), "C2"), (c3.as_str(), "C3"), (c4.as_str(), "C4")];
        shapes.push(((m, k), shape(&p, &roots)));
    }
    let diffs: Vec<String> = shapes
        .windows(2)
        .filter(|s| s[0].1 != s[1].1)
        .map(|s| {
            let only = |a: &Shape, b: &Shape| a.0.difference(&b.0).cloned().collect::<Vec<_>>();
            format!("{:?} vs {:?}: {:?} | {:?}", s[0].0, s[1].0, only(&s[0].1, &s[1].1), only(&s[1].1, &s[0].1))
        })
        .collect();
    assert!(diffs.is_empty(), "{diffs:#?}");
}

/// Architect delta2 b1 reproduction (advisory a2), default suite: an honest
/// copy-addressed edit whose source op later merges back into M by
/// identical text. C = copy(M, oid2). With {1, 2, 3, 5}: M = "b", C = "a2".
/// With the late same-text op 4 too, every order gives the strict §4.4
/// result: 4 wins at op 1, 2 merges back by text, 5 continues M (M = "a2",
/// head 5), 3 becomes copy(M, oid3) = "b" with a notice, and no stale C.
#[test]
fn g3_p7_s9_copy_addressed_edit_merging_back_into_its_source_memo() {
    let c = conflict_copy_id(M, &oid(2));
    let ops = |late: bool| {
        let mut w = vec![
            at(mc(1, "b", 100, &[], M, "base"), 1),
            at(me(2, "a", 200, &[("b", 1)], M, 1, "a"), 1),
            at(me(3, "d", 300, &[("b", 1)], M, 1, "b"), 1),
            at(me(5, "a", 400, &[("a", 1), ("b", 1), ("d", 1)], &c, 2, "a2"), 2),
        ];
        if late {
            w.push(at(me(4, "e", 500, &[("b", 1)], M, 1, "a"), 1));
        }
        w
    };
    let w = ops(false);
    let p = order_free(&w, 100, 0x7090);
    assert_eq!(memo_of(&p, M).map(|m| m.text), Some("b".to_owned()));
    let copy = memo_of(&p, &c).unwrap();
    assert_eq!((copy.text.as_str(), copy.conflict_of.as_deref()), ("a2", Some(M)));
    assert_eq!(live_memos(&p), 2);
    assert!(invisible(&w, &p).is_empty());

    let w = ops(true);
    let p = order_free(&w, 300, 0x7091);
    let m = memo_of(&p, M).unwrap();
    assert_eq!((m.text.as_str(), m.head_op_id), ("a2", oid(5)));
    let b_copy = conflict_copy_id(M, &oid(3));
    let b = memo_of(&p, &b_copy).unwrap();
    assert_eq!((b.text.as_str(), b.conflict_of.as_deref()), ("b", Some(M)));
    assert!(memo_of(&p, &c).is_none(), "no stale copy(M, oid2)");
    assert_eq!(live_memos(&p), 2);
    assert_eq!(copies(&p), vec![(b_copy, oid(3))]);
    for n in [1, 2, 4, 5] {
        assert_eq!(p.memo_lineage[&oid(n)], M, "op {n}");
    }
    assert!(invisible(&w, &p).is_empty(), "{:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty());
}

// ------------------------------------------- fixpoint routing (p8, delta 4)

/// First op id of the `chain5` family.
const Q: u64 = 200;

/// Copy ids of `chain5(m)`: C_k = copy(C_{k-1}, l_k), C_0 = m.
fn chain5_ids(m: &str) -> [String; 5] {
    let mut cur = m.to_owned();
    [1u64, 2, 3, 4, 5].map(|k| {
        cur = conflict_copy_id(&cur, &oid(Q + 10 * k + 1));
        cur.clone()
    })
}

/// Device/seq pairs of every chain op an edit at level `k` has seen.
fn chain5_ctx(k: u64) -> Vec<(String, u64)> {
    let mut seen = vec![("a".to_owned(), 1)];
    seen.extend((1..k).map(|j| (format!("l{j}"), 1)));
    seen
}

fn ctx_ref(owned: &[(String, u64)]) -> Vec<(&str, u64)> {
    owned.iter().map(|(d, s)| (d.as_str(), *s)).collect()
}

/// Five copy levels under `m`: the create (op Q); at level k, w_k (newer)
/// and l_k race on l_{k-1} (the create for k = 1), each addressed to the
/// memo its writer saw it in (or `addr` for every edit). l_k starts C_k.
fn chain5(m: &str, addr: Option<&str>) -> (Vec<Pub>, [String; 5]) {
    let c = chain5_ids(m);
    let mut w = vec![at(mc(Q, "a", T0, &[], m, "base"), 1)];
    for k in 1..=5u64 {
        let home = addr.map_or_else(|| if k == 1 { m.to_owned() } else { c[k as usize - 2].clone() }, str::to_owned);
        let parent = if k == 1 { Q } else { Q + 10 * (k - 1) + 1 };
        let seen = chain5_ctx(k);
        let ctx = ctx_ref(&seen);
        w.push(at(me(Q + 10 * k, &format!("w{k}"), T0 + 100 * k + 50, &ctx, &home, parent, &format!("w{k}")), 1));
        w.push(at(me(Q + 10 * k + 1, &format!("l{k}"), T0 + 100 * k, &ctx, &home, parent, &format!("l{k}")), 1));
    }
    (w, c)
}

/// §4.4 shape of `chain5(m)`: m shows w1, C_k shows w_{k+1}, C_5 shows l5.
fn assert_chain5(p: &Projection, m: &str, c: &[String; 5], name: &str) {
    let got = memo_of(p, m).unwrap_or_else(|| panic!("{name}: {m} missing; {:?}", memo_texts(p)));
    assert_eq!((got.text.as_str(), got.head_op_id, got.conflict_of), ("w1", oid(Q + 10), None), "{name}: root");
    for k in 1..=5usize {
        let got = memo_of(p, &c[k - 1]).unwrap_or_else(|| panic!("{name}: C{k} missing; {:?}", memo_texts(p)));
        let of = if k == 1 { m.to_owned() } else { c[k - 2].clone() };
        let (text, head) = if k < 5 { (format!("w{}", k + 1), Q + 10 * (k as u64 + 1)) } else { ("l5".to_owned(), Q + 51) };
        assert_eq!((got.text, got.head_op_id, got.conflict_of), (text, oid(head), Some(of)), "{name}: C{k}");
    }
    assert_eq!(p.memo_lineage[&oid(Q)], m, "{name}: create");
    assert_eq!(p.memo_lineage[&oid(Q + 10)], m, "{name}: w1");
    for k in 1..=5u64 {
        assert_eq!(p.memo_lineage[&oid(Q + 10 * k + 1)], c[k as usize - 1], "{name}: l{k}");
        if k < 5 {
            assert_eq!(p.memo_lineage[&oid(Q + 10 * (k + 1))], c[k as usize - 1], "{name}: w{}", k + 1);
        }
    }
}

/// Depth-5 copy chain: copies resolve only after five fixpoint rounds, under
/// honest, all-to-root, all-to-ghost and random addressing, and under root
/// ids that sort before, between and after their copy ids (copies resolved
/// before their parents): one §4.4 result, 500 orders, nothing invisible.
#[test]
fn g3_p8_f1_depth_five_copy_chain_under_any_addressing_and_id_order() {
    let (w, c) = chain5(M, None);
    let p = order_free(&w, 500, 0x8001);
    assert_chain5(&p, M, &c, "honest");
    assert_eq!((live_memos(&p), copies(&p).len()), (6, 5));
    assert!(invisible(&w, &p).is_empty(), "invisible {:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty());
    for addr in [M, "ghost-memo", c[4].as_str(), c[0].as_str()] {
        let (wa, _) = chain5(M, Some(addr));
        assert_eq!(view(&order_free(&wa, 50, 0x8002)), view(&p), "all addressed to {addr}");
    }
    let pool = [M, K, c[0].as_str(), c[2].as_str(), c[4].as_str(), "ghost-memo"];
    let mut rng = Lcg(0x8003);
    for round in 0..20 {
        let wr = readdress(&w, &BTreeSet::new(), &pool, &mut rng);
        assert_eq!(view(&order_free(&wr, 5, 0x8004 + round)), view(&p), "random addressing round {round}");
    }
    let mut shapes = Vec::new();
    for m in ["0", M, "1", "zz"] {
        let (w, c) = chain5(m, None);
        let p = order_free(&w, 50, 0x8005);
        assert_chain5(&p, m, &c, m);
        let sorts: Vec<bool> = c.iter().map(|ci| ci.as_str() < m).collect();
        shapes.push((m, sorts, shape(&p, &[(m, "R")])));
    }
    assert!(shapes.iter().any(|s| s.1.iter().all(|b| *b)), "some root sorts after every copy");
    assert!(shapes.windows(2).all(|s| s[0].2 == s[1].2), "shape depends on the root id: {shapes:#?}");
}

/// Orphans homed at C_3 and C_5, ids that exist only after three and five
/// fixpoint rounds: o5 saw l5 and is newest (shows in C_5, no copy for l5),
/// o3 is blind and older (copy of C_3), o5b has a self base (copy of C_5).
/// `base5`: o5's base (missing, or l5 once it arrives).
fn late_orphans(m: &str, base5: u64, small_ids: bool) -> (Vec<Pub>, [String; 5], [u64; 3]) {
    let (mut w, c) = chain5(m, None);
    let [n5, n3, n5b] = if small_ids { [1, 2, 3] } else { [990, 992, 993] };
    let mut seen = chain5_ctx(6);
    seen.extend((1..=5).map(|j| (format!("w{j}"), 1)));
    let base5 = if base5 == 0 { 991 } else { base5 };
    w.push(at(me(n5, "o5", T0 + 999, &ctx_ref(&seen), &c[4], base5, "o5"), 1));
    w.push(at(me(n3, "o3", T0 + 1, &[("a", 1), ("l1", 1), ("l2", 1)], &c[2], 994, "o3"), 1));
    w.push(at(me(n5b, "o5b", T0 + 2, &[("a", 1)], &c[4], n5b, "o5b"), 1));
    (w, c, [n5, n3, n5b])
}

#[test]
fn g3_p8_f2_orphans_homed_at_copies_that_appear_only_after_fixpoint_rounds() {
    let mut shapes = Vec::new();
    for (m, small) in [(M, false), (M, true), ("0", false), ("zz", true)] {
        let name = format!("root {m}, small ids {small}");
        let (w, c, [n5, n3, n5b]) = late_orphans(m, 0, small);
        let p = order_free(&w, 300, 0x8010);
        let c5 = memo_of(&p, &c[4]).unwrap();
        assert_eq!((c5.text.as_str(), c5.head_op_id, c5.conflict_of.as_deref()), ("o5", oid(n5), Some(c[3].as_str())), "{name}");
        let o3 = conflict_copy_id(&c[2], &oid(n3));
        assert_eq!(memo_of(&p, &o3).map(|x| (x.text, x.conflict_of)), Some(("o3".to_owned(), Some(c[2].clone()))), "{name}");
        let o5b = conflict_copy_id(&c[4], &oid(n5b));
        assert_eq!(memo_of(&p, &o5b).map(|x| (x.text, x.conflict_of)), Some(("o5b".to_owned(), Some(c[4].clone()))), "{name}");
        assert_eq!(memo_of(&p, &c[2]).map(|x| x.text), Some("w4".to_owned()), "{name}: C3 keeps w4");
        assert!(memo_of(&p, &conflict_copy_id(&c[4], &oid(Q + 51))).is_none(), "{name}: o5 saw l5, no copy");
        assert_eq!(live_memos(&p), 8, "{name}: {:?}", memo_texts(&p));
        assert_eq!((p.memo_lineage[&oid(n5)].as_str(), p.memo_lineage[&oid(Q + 51)].as_str()), (c[4].as_str(), c[4].as_str()));
        assert!(invisible(&w, &p).is_empty(), "{name}: invisible {:?}", invisible(&w, &p));
        assert!(unrouted(&w, &p).is_empty(), "{name}");
        // Once o5's base l5 arrives, o5 is an ordinary child of l5: same view.
        let (wb, _, _) = late_orphans(m, Q + 51, small);
        assert_eq!(view(&order_free(&wb, 100, 0x8011)), view(&p), "{name}: base arrival");
        if !small {
            shapes.push(shape(&p, &[(m, "R")]));
        }
    }
    assert!(shapes.windows(2).all(|s| s[0] == s[1]), "shape depends on the root id");
}

/// Base cycles whose members address different copies: x (C_2) ↔ y (C_4)
/// with z (C_1) continuing x; u (m) → v (C_3) → t (C_5) → u. `win`: the
/// cycle members are newest and win where they are homed.
fn copy_cycles(m: &str, win: bool) -> (Vec<Pub>, [String; 5]) {
    let (mut w, c) = chain5(m, None);
    let wall = |k: u64| if win { T0 + 5_000 + k } else { T0 + k };
    w.extend([
        at(me(300, "x", wall(3), &[("a", 1)], &c[1], 301, "x"), 1),
        at(me(301, "y", wall(4), &[("a", 1)], &c[3], 300, "y"), 1),
        at(me(302, "z", wall(5), &[("a", 1)], &c[0], 300, "z"), 1),
        at(me(310, "u", wall(6), &[("a", 1)], m, 311, "u"), 1),
        at(me(311, "v", wall(7), &[("a", 1)], &c[2], 312, "v"), 1),
        at(me(312, "t", wall(8), &[("a", 1)], &c[4], 310, "t"), 1),
    ]);
    (w, c)
}

#[test]
fn g3_p8_f3_cycles_across_copies() {
    let (w, c) = copy_cycles(M, false);
    let p = order_free(&w, 500, 0x8020);
    assert_chain5(&p, M, &c, "losing cycles");
    let expect = [(&c[1], 302, "z"), (&c[3], 301, "y"), (&M.to_owned(), 310, "u"), (&c[2], 311, "v"), (&c[4], 312, "t")];
    for (home, loser, text) in expect {
        let copy = conflict_copy_id(home, &oid(loser));
        let got = memo_of(&p, &copy).map(|x| (x.text, x.conflict_of));
        assert_eq!(got, Some((text.to_owned(), Some(home.clone()))), "cycle member {text} homed at its own entity");
    }
    assert_eq!(p.memo_lineage[&oid(300)], p.memo_lineage[&oid(302)], "z continues cycle member x");
    assert_eq!(live_memos(&p), 11, "{:?}", memo_texts(&p));
    assert!(invisible(&w, &p).is_empty(), "invisible {:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty());

    let (w, _) = copy_cycles(M, true);
    let p = order_free(&w, 500, 0x8021);
    let texts = memo_texts(&p);
    for t in ["z", "y", "u", "v", "t", "w1", "w2", "w3", "w4", "w5", "l5"] {
        assert!(texts.contains(t), "winning cycles: {t} visible: {texts:?}");
    }
    assert!(invisible(&w, &p).is_empty(), "winning cycles: invisible {:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty());
    let mut shapes = Vec::new();
    for m in ["0", M, "zz"] {
        let (w, _) = copy_cycles(m, true);
        shapes.push(shape(&order_free(&w, 30, 0x8022), &[(m, "R")]));
    }
    assert!(shapes.windows(2).all(|s| s[0] == s[1]), "winning cycles: shape depends on the root id");
}

/// Memo `k`: create 400; fa (401) and fb (402) are fork variants at dot
/// (f, 1) on the create; g (403) on the create saw both by ctx (clamped:
/// fork range) and, per `g_obs`, by obs; h (404) continues fa. `del`: a
/// delete of k that saw everything by ctx, with `del_obs`.
fn fork_kids(k: &str, g_obs: &[u64], del: Option<&[u64]>) -> Vec<Pub> {
    let mut w = vec![
        at(mc(400, "ka", T0, &[], k, "kbase"), 1),
        at(me(401, "f", T0 + 50, &[("ka", 1)], k, 400, "fa"), 1),
        at(me(402, "f", T0 + 40, &[("ka", 1)], k, 400, "fb"), 1),
        at(with_obs(me(403, "g", T0 + 10, &[("ka", 1), ("f", 1)], k, 400, "g"), g_obs), 1),
        at(me(404, "h", T0 + 20, &[("ka", 1), ("f", 1)], k, 401, "h"), 1),
    ];
    if let Some(obs) = del {
        w.push(at(with_obs(md(405, "dk", T0 + 90, &[("ka", 1), ("f", 9), ("g", 1), ("h", 1)], k), obs), 1));
    }
    w
}

#[test]
fn g3_p8_f4_fork_children_with_and_without_obs() {
    let copy = |n: u64| conflict_copy_id(K, &oid(n));
    let text = |p: &Projection, id: &str| memo_of(p, id).map(|x| x.text);
    // No obs: all three concurrent; fa (newest) wins, h continues it.
    let w = fork_kids(K, &[], None);
    let p = order_free(&w, 300, 0x8030);
    assert_eq!(set_of(&w).forks(), &Forks::from([("f".to_owned(), 1)]));
    assert_eq!(text(&p, K).as_deref(), Some("h"));
    assert_eq!((text(&p, &copy(402)).as_deref(), text(&p, &copy(403)).as_deref()), (Some("fb"), Some("g")));
    assert!(invisible(&w, &p).is_empty(), "{:?}", invisible(&w, &p));
    // obs of both variants: g saw them and wins; fa's copy carries h.
    let w = fork_kids(K, &[401, 402], None);
    let p = order_free(&w, 300, 0x8031);
    assert_eq!(text(&p, K).as_deref(), Some("g"));
    assert_eq!((text(&p, &copy(401)).as_deref(), text(&p, &copy(402)).as_deref()), (Some("h"), Some("fb")));
    assert_eq!(p.memo_lineage[&oid(404)], copy(401));
    assert!(invisible(&w, &p).is_empty(), "{:?}", invisible(&w, &p));
    // obs of fa only: fb stays concurrent with g and wins on the tie key.
    let w = fork_kids(K, &[401], None);
    let p = order_free(&w, 300, 0x8032);
    assert_eq!(text(&p, K).as_deref(), Some("fb"));
    assert_eq!((text(&p, &copy(401)).as_deref(), text(&p, &copy(403)).as_deref()), (Some("h"), Some("g")));
    assert!(invisible(&w, &p).is_empty(), "{:?}", invisible(&w, &p));
    // Deletes: ctx never sees a fork-range op; obs does.
    let p = order_free(&fork_kids(K, &[], Some(&[])), 200, 0x8033);
    assert!(is_restored(&p, Kind::Memo, K), "fa (fork range) unseen without obs");
    let p = order_free(&fork_kids(K, &[], Some(&[401])), 200, 0x8034);
    assert!(is_deleted(&p, Kind::Memo, K), "K's content is create, fa, h: all seen");
    assert!(text(&p, &copy(402)).is_some() && text(&p, &copy(403)).is_some(), "copies keep their own lifecycle");
    let p = order_free(&fork_kids(K, &[401, 402], Some(&[])), 200, 0x8035);
    assert!(is_deleted(&p, Kind::Memo, K), "K's content is create and g: both seen by ctx");
    assert_eq!(text(&p, &copy(401)).as_deref(), Some("h"));
}

/// Everything at once: the depth-5 chain, late orphans on C_3/C_5, cycles
/// across copies, fork children on K, a delete of C_3 that saw its content
/// and a blind delete of C_5. 500 orders: one hash, nothing invisible.
#[test]
fn g3_p8_f5_everything_mixed_under_500_permutations() {
    let (mut w, c, _) = late_orphans(M, 0, false);
    let (cy, _) = copy_cycles(M, false);
    let cycle_ids: BTreeSet<String> = [300, 301, 302, 310, 311, 312].map(oid).into_iter().collect();
    w.extend(cy.into_iter().filter(|(b, _)| cycle_ids.contains(&b.op_id)));
    w.extend(fork_kids(K, &[401], Some(&[])));
    let mut seen = chain5_ctx(6);
    seen.extend((1..=5).map(|j| (format!("w{j}"), 1)));
    w.push(at(md(500, "d3", T0 + 3_000, &ctx_ref(&seen), &c[2]), 1));
    w.push(at(md(501, "d5", T0 + 3_000, &ctx_ref(&seen), &c[4]), 1));
    assert_eq!(w.len(), 11 + 3 + 6 + 6 + 2);
    let p = order_free(&w, 500, 0x8040);
    assert!(invisible(&w, &p).is_empty(), "invisible {:?}", invisible(&w, &p));
    assert!(unrouted(&w, &p).is_empty(), "unrouted {:?}", unrouted(&w, &p));
    assert!(is_deleted(&p, Kind::Memo, &c[2]), "the C_3 delete saw l3 and w4");
    assert!(is_restored(&p, Kind::Memo, &c[4]), "o5 is C_5 content the delete did not see");
    assert!(is_restored(&p, Kind::Memo, K), "fb (fork range) unseen by the K delete");
    let texts = memo_texts(&p);
    for t in ["w1", "w2", "w3", "w5", "o5", "o3", "o5b", "z", "y", "u", "v", "t", "fb", "g", "h"] {
        assert!(texts.contains(t), "{t} visible: {texts:?}");
    }
    assert_eq!(memo_of(&p, &c[3]).map(|x| x.conflict_of), Some(Some(c[2].clone())), "C_4 outlives its deleted source");
}

/// Random memo histories: random bases (seen node, missing, self, a later
/// op, a pin), random addressing (root, copy ids, ghost), random ctx, obs,
/// creates, pins, deletes and fork variants. 50 worlds × 10 orders: one
/// hash per world, nothing invisible or unrouted.
fn random_memo_world(seed: u64) -> Vec<Pub> {
    let mut rng = Lcg(seed);
    let mut pick = |n: u64| rng.step() % n.max(1);
    let devs = ["a", "b", "c", "d", "e", "f"];
    let mut seqs: BTreeMap<&str, u64> = BTreeMap::new();
    let mut ops: Vec<(Pub, bool)> = Vec::new();
    let total = 14 + pick(12);
    for i in 0..total {
        let n = 1_000 + i;
        let dev = devs[pick(6) as usize];
        let prev = *seqs.get(dev).unwrap_or(&0);
        let fork = prev > 0 && pick(10) == 0;
        let seq = if fork { prev } else { prev + 1 };
        seqs.insert(dev, seq);
        let nodes: Vec<&(Pub, bool)> = ops.iter().filter(|(_, node)| *node).collect();
        let mut ctx: BTreeMap<String, u64> = BTreeMap::new();
        let mut see = |d: &Dot| {
            let slot = ctx.entry(d.device.clone()).or_insert(0);
            *slot = (*slot).max(d.seq);
        };
        for ((_, d), _) in &ops {
            if d.device != dev && pick(2) == 0 {
                see(d);
            }
        }
        let roll = pick(100);
        let (body, node) = if i < 2 || (roll < 6) {
            let memo = if i == 1 || pick(2) == 1 { K } else { M };
            (mc(n, dev, T0 + pick(1_000), &[], memo, &format!("t{}", pick(6))), true)
        } else if roll < 14 {
            let target = if pick(2) == 0 { M } else { K };
            (mp(n, dev, T0 + pick(1_000), &[], target, pick(2) == 0), false)
        } else if roll < 24 {
            let entity = if pick(3) == 0 { K.to_owned() } else { nodes[pick(nodes.len() as u64) as usize].0 .0.entity_id.clone() };
            (md(n, dev, T0 + pick(1_000), &[], &entity), false)
        } else {
            let form = pick(10);
            let base = match form {
                0 => 5_000 + i,
                1 => n,
                2 => n + 1 + pick(3),
                3 => ops.iter().find(|((b, _), _)| b.op_type == "pin").map_or(6_000, |((b, _), _)| {
                    1_000 + ops.iter().position(|((x, _), _)| x.op_id == b.op_id).unwrap() as u64
                }),
                _ => {
                    let (b, d) = &nodes[pick(nodes.len() as u64) as usize].0;
                    see(d);
                    1_000 + ops.iter().position(|((x, _), _)| x.op_id == b.op_id).unwrap() as u64
                }
            };
            let pool_pick = pick(10);
            let entity = if pool_pick < 4 {
                nodes[pick(nodes.len() as u64) as usize].0 .0.entity_id.clone()
            } else if pool_pick < 6 {
                if pick(2) == 0 { M.to_owned() } else { K.to_owned() }
            } else if pool_pick < 9 {
                let src = nodes[pick(nodes.len() as u64) as usize].0 .0.entity_id.clone();
                conflict_copy_id(&src, &nodes[pick(nodes.len() as u64) as usize].0 .0.op_id)
            } else {
                "ghost-memo".to_owned()
            };
            (me(n, dev, T0 + pick(1_000), &[], &entity, base, &format!("t{}", pick(6))), true)
        };
        let mut body = body;
        if seq > 1 {
            ctx.insert(dev.to_owned(), seq - 1);
        }
        ctx.retain(|_, s| *s > 0);
        body.ctx = ctx;
        if body.op_type == "delete" && pick(3) == 0 {
            let mut obs: Vec<String> = ops.iter().filter(|_| pick(3) == 0).map(|((b, _), _)| b.op_id.clone()).collect();
            obs.sort();
            obs.dedup();
            body.obs = obs;
        }
        assert_eq!(body.validate(), Ok(()), "seed {seed} op {n}");
        ops.push((at(body, seq), node));
    }
    ops.into_iter().map(|(p, _)| p).collect()
}

/// `invisible` minus same-text twins (identical text is not a conflict,
/// §4.4 / AC 2): a node is accounted for when a node with its text that is
/// not invisible has the same owner, or when a live memo shows its text
/// with a head from the same lineage component (same root home). The
/// walk merges same-text twins across forest parents (a multi-root
/// frontier, children of a merged twin), and a losing main head moves to
/// its copy alone while its merged twins stay with the source memo;
/// `invisible_memo_edits` (c) only accepts twins under one forest parent.
fn invisible_up_to_twins(w: &[Pub], p: &Projection) -> Vec<String> {
    let inv = invisible(w, p);
    let bodies: BTreeMap<&str, &OpBody> = w.iter().map(|(b, _)| (b.op_id.as_str(), b)).collect();
    let text = |b: &OpBody| b.payload.get("text").and_then(Value::as_str).map(str::to_owned);
    let is_node = |b: &OpBody| matches!(b.op_type.as_str(), "create" | "edit");
    let home = |id: &str| {
        let mut cur = bodies[id];
        for _ in 0..64 {
            match cur.payload.get("base_op_id").and_then(Value::as_str).and_then(|b| bodies.get(b)) {
                Some(up) if is_node(up) && up.op_id != cur.op_id => cur = up,
                _ => break,
            }
        }
        cur.entity_id.clone()
    };
    inv.iter()
        .filter(|e| {
            let eb = bodies[e.as_str()];
            let owner_twin = w.iter().any(|(d, _)| {
                d.op_id != eb.op_id
                    && is_node(d)
                    && !inv.contains(&d.op_id)
                    && text(d) == text(eb)
                    && p.memo_lineage.get(&d.op_id) == p.memo_lineage.get(&eb.op_id)
            });
            let shown_twin = p.entities.values().any(|st| match st {
                EntityState::Memo(m) => {
                    Some(&m.text) == text(eb).as_ref() && bodies.contains_key(m.head_op_id.as_str()) && home(&m.head_op_id) == home(e)
                }
                _ => false,
            });
            !owner_twin && !shown_twin
        })
        .cloned()
        .collect()
}

#[test]
fn g3_p8_f6_random_memo_histories_500_permutations() {
    let mut failures = Vec::new();
    let (mut forks, mut orphans, mut copies_seen) = (0, 0, 0);
    for world in 0..50u64 {
        let seed = 0x8100 + world;
        let w = random_memo_world(seed);
        let set = set_of(&w);
        forks += usize::from(!set.forks().is_empty());
        let p = order_free(&w, 10, seed);
        copies_seen += copies(&p).len();
        orphans += w
            .iter()
            .filter(|(b, _)| b.op_type == "edit" && b.payload["base_op_id"].as_str().is_some_and(|x| set.get(x).is_none()))
            .count();
        let inv = invisible_up_to_twins(&w, &p);
        let unr = unrouted(&w, &p);
        if !inv.is_empty() || !unr.is_empty() {
            failures.push(format!("seed {seed:#x}: invisible {inv:?}, unrouted {unr:?}"));
        }
    }
    assert!(forks >= 5 && orphans >= 20 && copies_seen >= 30, "generator coverage: forks {forks}, orphans {orphans}, copies {copies_seen}");
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Found by `g3_p8_f6` (47 of 3000 random worlds before `invisible_up_to_twins`):
/// same-text twins merge across forest parents. Two same-text creates of M
/// (1, 2) are one frontier; 3 (on 1) and 4 (on 2) are same-text twins, 4
/// newer. A blind orphan o (newest) wins the heads, so the losing main
/// head 4 moves to copy(M, 4) alone; its twin 3 stays in M's lineage. The
/// text "t" is visible (in the copy), so no edit is invisible; but
/// `invisible_memo_edits` (c) only accepts twins under one forest parent
/// and flags 3. Observation: oracle gap, and 3 counts as M content in fate
/// accounting although M does not show its text.
#[test]
fn g3_p8_f7_same_text_twins_across_parents_stay_visible() {
    let w = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(mc(2, "b", T0 + 1, &[], M, "base"), 1),
        at(me(3, "c", T0 + 10, &[("a", 1), ("b", 1)], M, 1, "t"), 1),
        at(me(4, "d", T0 + 20, &[("a", 1), ("b", 1)], M, 2, "t"), 1),
        at(me(5, "o", T0 + 90, &[("a", 1), ("b", 1)], M, 99, "o"), 1),
    ];
    let p = order_free(&w, 300, 0x8060);
    let copy = conflict_copy_id(M, &oid(4));
    assert_eq!(memo_of(&p, M).map(|m| m.text), Some("o".to_owned()));
    assert_eq!(memo_of(&p, &copy).map(|m| (m.text, m.head_op_id)), Some(("t".to_owned(), oid(4))));
    assert_eq!(live_memos(&p), 2);
    assert!(unrouted(&w, &p).is_empty());
    assert!(invisible_up_to_twins(&w, &p).is_empty());
    assert_eq!(invisible(&w, &p), vec![oid(3)], "oracle gap: twin 3 has another forest parent than 4");
    assert_eq!(p.memo_lineage[&oid(3)], M, "the twin stays with the source memo");
    // A delete of M that saw everything but the twin 3 restores M.
    let mut wd = w.clone();
    wd.push(at(md(6, "x", T0 + 95, &[("a", 1), ("b", 1), ("d", 1), ("o", 1)], M), 1));
    let p = order_free(&wd, 100, 0x8061);
    assert!(is_restored(&p, Kind::Memo, M), "3 is M content the delete did not see");
}

/// A same-base sibling y that saw x (an edit session opened on the old
/// base, committed after x arrived) wins causally; x's text stays visible
/// (as a copy at the walk level, unlike a seen orphan head, which is
/// superseded), in every order, also when y and x are fork-range ops.
#[test]
fn g3_p8_f8_a_seen_same_base_sibling_stays_visible() {
    let w = vec![
        at(mc(1, "a", T0, &[], M, "base"), 1),
        at(me(2, "x", T0 + 50, &[("a", 1)], M, 1, "x"), 1),
        at(me(3, "y", T0 + 10, &[("a", 1), ("x", 1)], M, 1, "y"), 1),
    ];
    let p = order_free(&w, 100, 0x8070);
    assert_eq!(memo_of(&p, M).map(|m| m.text), Some("y".to_owned()), "y saw x");
    assert!(memo_texts(&p).contains("x"));
    assert!(invisible(&w, &p).is_empty());
    let forked = project_with(&w, &Forks::from([("x".to_owned(), 1)]));
    assert_eq!(memo_of(&forked, M).map(|m| m.text), Some("x".to_owned()), "fork range: ctx no longer sees x");
    assert!(memo_texts(&forked).contains("y"));
    let mut wo = w.clone();
    wo[2].0.obs = vec![oid(2)];
    let p = project_with(&wo, &Forks::from([("x".to_owned(), 1)]));
    assert_eq!(memo_of(&p, M).map(|m| m.text), Some("y".to_owned()), "obs sees the fork-range x");
    assert!(memo_texts(&p).contains("x"));
}

// ------------------------------- respawned carried heads (p9, w1 fix)

/// Op id of the probe delete (`ownership_probe`).
const PROBE: u64 = 9_999;

/// Observable form of "every lineage op has exactly one owner". Fate
/// content is a resolution's `own` plus notes, and `memo_lineage` is built
/// from the same `own` sets (a later owner overwrites), so for every live
/// memo E of `w` with its deletes removed: a blind delete of E whose `obs`
/// lists exactly the ops routed to E (and E's notes) must delete E and
/// change nothing else, and dropping any one routed op from `obs` must
/// restore E. An op owned twice is routed to one owner only, so the other
/// owner's probe is restored: the w1 symptom. Returns the failures.
fn ownership_probe(w: &[Pub]) -> Vec<String> {
    let base: Vec<Pub> = w.iter().filter(|(b, _)| b.op_type != "delete").cloned().collect();
    let p = project_all(&base);
    let probe = |id: &str, obs: Vec<String>| {
        let mut del = md(PROBE, "zprobe", T0 + 50_000, &[], id);
        del.obs = obs;
        del.obs.sort();
        assert_eq!(del.validate(), Ok(()), "probe delete of {id}");
        let mut wd = base.clone();
        wd.push(at(del, 1));
        project_all(&wd)
    };
    let mut failures = Vec::new();
    for (key, state) in &p.entities {
        if key.0 != Kind::Memo || !matches!(state, EntityState::Memo(_)) {
            continue;
        }
        let id = key.1.as_str();
        let owned: Vec<String> =
            p.memo_lineage.iter().filter(|(_, owner)| owner.as_str() == id).map(|(op, _)| op.clone()).collect();
        if owned.is_empty() {
            failures.push(format!("{id}: live memo with no routed op"));
            continue;
        }
        let notes: Vec<String> = base
            .iter()
            .filter(|(b, _)| b.kind == Kind::Memo && b.entity_id == id && b.op_type == "note")
            .map(|(b, _)| b.op_id.clone())
            .collect();
        let all = probe(id, owned.iter().chain(&notes).cloned().collect());
        if !is_deleted(&all, Kind::Memo, id) || is_restored(&all, Kind::Memo, id) {
            failures.push(format!("{id}: a delete that saw all {} routed ops did not delete it", owned.len()));
        }
        let others = |q: &Projection| q.entities.iter().filter(|(k, _)| *k != key).map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>();
        if others(&all) != others(&p) || all.memo_lineage != p.memo_lineage {
            failures.push(format!("{id}: its delete changed another entity or the routing"));
        }
        for x in &owned {
            let rest = owned.iter().filter(|o| *o != x).chain(&notes).cloned().collect();
            if !is_restored(&probe(id, rest), Kind::Memo, id) {
                failures.push(format!("{id}: {x} is routed here but is not its fate content"));
            }
        }
    }
    failures
}

/// The architect's w1 world under root `m`: b's create 1 and main edit 3;
/// a's orphan chain 4 → 5 (base 99 missing, "x" → "y") loses at m to 3, so
/// C = copy(m, 5) carries {4, 5}; c's blind orphan 6 ("z", base 98
/// missing), addressed to C and newest, wins in C and respawns 5 into
/// D = copy(C, 5).
fn w1_world(m: &str) -> (Vec<Pub>, String, String) {
    let c = conflict_copy_id(m, &oid(5));
    let d = conflict_copy_id(&c, &oid(5));
    let w = vec![
        at(mc(1, "b", T0 + 100, &[], m, "base"), 1),
        at(me(3, "b", T0 + 900, &[("b", 1)], m, 1, "main"), 2),
        at(me(4, "a", T0 + 150, &[], m, 99, "x"), 1),
        at(me(5, "a", T0 + 200, &[("a", 1)], m, 4, "y"), 2),
        at(me(6, "c", T0 + 1_000, &[("b", 2)], &c, 98, "z"), 1),
    ];
    (w, c, d)
}

/// w1 regression under 500 orders and three root ids (sorting before and
/// after their copy ids, which used to pick the `memo_lineage` owner): the
/// chain moves to D whole, C keeps only 6, every op has one owner, and a
/// delete of C that saw 6 (all C shows) deletes C while D stays live.
#[test]
fn g3_p9_w1_respawned_carried_head_is_owned_once_under_500_orders() {
    for (m, rounds) in [(M, 500), ("0", 150), ("zz", 150)] {
        let (w, c, d) = w1_world(m);
        let p = order_free(&w, rounds, 0x9001);
        assert_eq!(memo_of(&p, m).map(|x| x.text), Some("main".to_owned()), "{m}");
        let cm = memo_of(&p, &c).unwrap();
        assert_eq!((cm.text.as_str(), cm.head_op_id, cm.conflict_of.as_deref()), ("z", oid(6), Some(m)), "{m}: C");
        let dm = memo_of(&p, &d).unwrap();
        assert_eq!((dm.text.as_str(), dm.head_op_id, dm.conflict_of.as_deref()), ("y", oid(5), Some(c.as_str())), "{m}: D");
        assert_eq!(live_memos(&p), 3, "{m}");
        let got: BTreeSet<(String, String)> = copies(&p).into_iter().collect();
        assert_eq!(got, BTreeSet::from([(c.clone(), oid(5)), (d.clone(), oid(5))]), "{m}: copies");
        for (n, owner) in [(1, m), (3, m), (4, d.as_str()), (5, d.as_str()), (6, c.as_str())] {
            assert_eq!(p.memo_lineage[&oid(n)], owner, "{m}: op {n}");
        }
        assert!(invisible(&w, &p).is_empty(), "{m}: invisible {:?}", invisible(&w, &p));
        assert!(unrouted(&w, &p).is_empty(), "{m}");
        assert_eq!(ownership_probe(&w), Vec::<String>::new(), "{m}");

        // The architect's delete: saw 6 and the main lineage, not the chain.
        let mut wd = w.clone();
        wd.push(at(md(7, "c", T0 + 1_100, &[("b", 2), ("c", 1)], &c), 2));
        let p = order_free(&wd, rounds, 0x9002);
        assert!(is_deleted(&p, Kind::Memo, &c) && !is_restored(&p, Kind::Memo, &c), "{m}: C deleted");
        assert_eq!(memo_of(&p, &d).map(|x| (x.text, x.conflict_of)), Some(("y".to_owned(), Some(c.clone()))), "{m}: D outlives C");
        assert_eq!(memo_of(&p, m).map(|x| x.text), Some("main".to_owned()));

        // D's content is the whole chain: a delete blind to 4 is restored.
        for (obs, deleted) in [(vec![5], false), (vec![4], false), (vec![4, 5], true)] {
            let mut wd = w.clone();
            wd.push(at(with_obs(md(7, "x", T0 + 1_100, &[], &d), &obs), 1));
            let p = order_free(&wd, 50, 0x9003);
            assert_eq!(is_deleted(&p, Kind::Memo, &d), deleted, "{m}: D delete obs {obs:?}");
            assert_eq!(is_restored(&p, Kind::Memo, &d), !deleted, "{m}: D delete obs {obs:?}");
            assert_eq!(memo_of(&p, &c).map(|x| x.text), Some("z".to_owned()), "{m}: C untouched");
        }
    }
}

/// Respawn ladder under root `m`: b's create 1 and main edit 3; a's orphan
/// chain 4 → 5 → 6 (base 99 missing, head 6 "y") loses at m, so
/// C_1 = copy(m, 6) carries it. At level k = 1..=`levels` a blind orphan
/// x_k (op 10 + k, "z{k}", base missing) addressed to C_k is newer than
/// the chain head and respawns 6 into C_{k+1} = copy(C_k, 6). At level
/// `old`, x_k is older than 6 instead: the chain stops there and x_k
/// becomes copy(C_k, x_k). Returns ids [m, C_1, C_2, ...].
fn ladder(m: &str, levels: u64, old: Option<u64>) -> (Vec<Pub>, Vec<String>) {
    let mut ids = vec![m.to_owned()];
    let mut w = vec![
        at(mc(1, "b", T0 + 100, &[], m, "base"), 1),
        at(me(3, "b", T0 + 900, &[("b", 1)], m, 1, "main"), 2),
        at(me(4, "a", T0 + 150, &[], m, 99, "p"), 1),
        at(me(5, "a", T0 + 160, &[("a", 1)], m, 4, "q"), 2),
        at(me(6, "a", T0 + 200, &[("a", 2)], m, 5, "y"), 3),
    ];
    for k in 1..=levels {
        let ck = conflict_copy_id(&ids[k as usize - 1], &oid(6));
        ids.push(ck.clone());
        let wall = if old == Some(k) { T0 + 170 } else { T0 + 1_000 * k };
        w.push(at(me(10 + k, &format!("x{k}"), wall, &[("b", 2)], &ck, 90 + k, &format!("z{k}")), 1));
        if old == Some(k) {
            break;
        }
    }
    (w, ids)
}

/// The carried chain respawned across three copy levels (m → C_1 → C_2 →
/// C_3 → C_4), and ladders that stop at level 2 or 3: the chain moves whole
/// at every respawn and ends in exactly one copy; each C_k keeps only x_k;
/// one delete per C_k that saw x_k (all C_k shows) and none of the chain
/// deletes every C_k while the chain's copy stays live.
#[test]
fn g3_p9_w2_respawned_carried_head_across_three_copy_levels() {
    for m in [M, "0", "zz"] {
        for (levels, old) in [(3, None), (1, None), (2, None), (2, Some(2)), (3, Some(3))] {
            let name = format!("root {m}, levels {levels}, old {old:?}");
            let (w, ids) = ladder(m, levels, old);
            let p = order_free(&w, if m == M && old.is_none() && levels == 3 { 500 } else { 60 }, 0x9010 + levels);
            let top = levels as usize;
            // Where the chain ends: C_{levels+1} for a full ladder, else C_old.
            let (chain_home, chain_of) = match old {
                None => (conflict_copy_id(&ids[top], &oid(6)), ids[top].clone()),
                Some(_) => (ids[top].clone(), ids[top - 1].clone()),
            };
            let got = memo_of(&p, &chain_home).unwrap_or_else(|| panic!("{name}: chain copy missing; {:?}", memo_texts(&p)));
            assert_eq!((got.text.as_str(), got.head_op_id, got.conflict_of), ("y", oid(6), Some(chain_of)), "{name}");
            for n in [4, 5, 6] {
                assert_eq!(p.memo_lineage[&oid(n)], chain_home, "{name}: chain op {n} has one home");
            }
            for k in 1..=levels {
                let (home, text) = if old == Some(k) {
                    (conflict_copy_id(&ids[k as usize], &oid(10 + k)), format!("z{k}"))
                } else {
                    (ids[k as usize].clone(), format!("z{k}"))
                };
                assert_eq!(memo_of(&p, &home).map(|x| (x.text, x.head_op_id)), Some((text, oid(10 + k))), "{name}: x{k}");
                assert_eq!(p.memo_lineage[&oid(10 + k)], home, "{name}: x{k}");
            }
            assert_eq!(live_memos(&p), levels as usize + 2, "{name}: {:?}", memo_texts(&p));
            assert_eq!(copies(&p).len(), levels as usize + 1, "{name}");
            assert!(invisible(&w, &p).is_empty(), "{name}: invisible {:?}", invisible(&w, &p));
            assert!(unrouted(&w, &p).is_empty(), "{name}");
            assert_eq!(ownership_probe(&w), Vec::<String>::new(), "{name}");

            // Delete every C_k that only shows x_k, each seeing x_k alone.
            let shows_x: Vec<u64> = (1..=levels).filter(|k| old != Some(*k)).collect();
            let mut wd = w.clone();
            for &k in &shows_x {
                wd.push(at(with_obs(md(20 + k, &format!("d{k}"), T0 + 9_000, &[("b", 2)], &ids[k as usize]), &[10 + k]), 1));
            }
            let pd = order_free(&wd, 60, 0x9020 + levels);
            for &k in &shows_x {
                assert!(is_deleted(&pd, Kind::Memo, &ids[k as usize]), "{name}: C_{k} deleted");
                assert!(!is_restored(&pd, Kind::Memo, &ids[k as usize]), "{name}: C_{k} not restored");
            }
            assert_eq!(memo_of(&pd, &chain_home).map(|x| x.text), Some("y".to_owned()), "{name}: the chain copy outlives its sources");
            assert_eq!(memo_of(&pd, m).map(|x| x.text), Some("main".to_owned()), "{name}");
        }
    }
}

/// Orphan chains that lose twice (at m, then again in their copy).
/// (a) two chains with different head texts both lose at m and both are
/// respawned out of their copies; then an orphan older than chain A's head
/// in A's second copy loses there instead (the chain wins its third race).
/// (b) a chain that loses at m and then to an orphan that saw its head:
/// superseded, no respawn, the chain stays C's content.
/// (c) a chain that grew a child edit addressed to its (stale) copy id:
/// lineage is structural, so the child already heads the chain at m, the
/// copy is named after it, and the whole chain respawns with it.
/// (d) two chains with one head text form one copy group; in C the merged
/// twin head that loses takes its own chain, and the other twin's chain
/// stays with C (same-text twins, p8-o1 family) — pinned.
#[test]
fn g3_p9_w3_orphan_chains_that_lose_twice() {
    let base = || {
        vec![
            at(mc(1, "b", T0 + 100, &[], M, "base"), 1),
            at(me(3, "b", T0 + 900, &[("b", 1)], M, 1, "main"), 2),
        ]
    };
    let check = |name: &str, w: &[Pub], rounds: usize| {
        let p = order_free(w, rounds, 0x9030);
        assert!(invisible_up_to_twins(w, &p).is_empty(), "{name}: invisible {:?}", invisible_up_to_twins(w, &p));
        assert!(unrouted(w, &p).is_empty(), "{name}");
        assert_eq!(ownership_probe(w), Vec::<String>::new(), "{name}");
        p
    };

    // (a)
    let ca = conflict_copy_id(M, &oid(5));
    let cb = conflict_copy_id(M, &oid(7));
    let (da, db) = (conflict_copy_id(&ca, &oid(5)), conflict_copy_id(&cb, &oid(7)));
    let mut w = base();
    w.extend([
        at(me(4, "a", T0 + 150, &[], M, 99, "a1"), 1),
        at(me(5, "a", T0 + 200, &[("a", 1)], M, 4, "ya"), 2),
        at(me(6, "e", T0 + 151, &[], M, 97, "b1"), 1),
        at(me(7, "e", T0 + 210, &[("e", 1)], M, 6, "yb"), 2),
        at(me(8, "x", T0 + 1_000, &[("b", 2)], &ca, 96, "xa"), 1),
        at(me(9, "y", T0 + 1_001, &[("b", 2)], &cb, 95, "xb"), 1),
        at(me(10, "z", T0 + 160, &[("b", 2)], &da, 94, "old"), 1),
    ]);
    let p = check("a", &w, 500);
    assert_eq!(memo_of(&p, &da).map(|x| (x.text, x.conflict_of)), Some(("ya".to_owned(), Some(ca.clone()))));
    assert_eq!(memo_of(&p, &db).map(|x| (x.text, x.conflict_of)), Some(("yb".to_owned(), Some(cb.clone()))));
    assert_eq!(memo_of(&p, &conflict_copy_id(&da, &oid(10))).map(|x| x.text), Some("old".to_owned()));
    for (n, owner) in [(4, &da), (5, &da), (6, &db), (7, &db), (8, &ca), (9, &cb)] {
        assert_eq!(&p.memo_lineage[&oid(n)], owner, "a: op {n}");
    }
    assert_eq!(live_memos(&p), 6, "a: {:?}", memo_texts(&p));

    // (b)
    let c = conflict_copy_id(M, &oid(5));
    let mut w = base();
    w.extend([
        at(me(4, "a", T0 + 150, &[], M, 99, "x"), 1),
        at(me(5, "a", T0 + 200, &[("a", 1)], M, 4, "y"), 2),
        at(me(6, "c", T0 + 1_000, &[("b", 2), ("a", 2)], &c, 98, "z"), 1),
    ]);
    let p = check("b", &w, 300);
    assert_eq!(memo_of(&p, &c).map(|x| x.text), Some("z".to_owned()));
    assert!(memo_of(&p, &conflict_copy_id(&c, &oid(5))).is_none(), "b: a seen head is superseded, no respawn");
    assert_eq!(live_memos(&p), 2);
    for n in [4, 5, 6] {
        assert_eq!(p.memo_lineage[&oid(n)], c, "b: op {n}");
    }
    for (obs, deleted) in [(vec![6], false), (vec![4, 5, 6], true)] {
        let mut wd = w.clone();
        wd.push(at(with_obs(md(7, "d", T0 + 2_000, &[], &c), &obs), 1));
        let pd = order_free(&wd, 50, 0x9031);
        assert_eq!(is_deleted(&pd, Kind::Memo, &c), deleted, "b: obs {obs:?}");
    }

    // (c)
    let stale = c.clone();
    let c = conflict_copy_id(M, &oid(7));
    let d = conflict_copy_id(&c, &oid(7));
    let mut w = base();
    w.extend([
        at(me(4, "a", T0 + 150, &[], M, 99, "x"), 1),
        at(me(5, "a", T0 + 200, &[("a", 1)], M, 4, "y"), 2),
        at(me(7, "f", T0 + 300, &[("a", 2)], &stale, 5, "y2"), 1),
        at(me(6, "c", T0 + 1_000, &[("b", 2)], &c, 98, "z"), 1),
    ]);
    let p = check("c", &w, 300);
    assert!(memo_of(&p, &stale).is_none(), "c: 7 heads the chain at M, so copy(M, 5) never exists");
    assert_eq!(memo_of(&p, &c).map(|x| (x.text, x.conflict_of)), Some(("z".to_owned(), Some(M.to_owned()))));
    assert_eq!(memo_of(&p, &d).map(|x| (x.text, x.conflict_of)), Some(("y2".to_owned(), Some(c.clone()))));
    for (n, owner) in [(4, &d), (5, &d), (7, &d), (6, &c)] {
        assert_eq!(&p.memo_lineage[&oid(n)], owner, "c: op {n}");
    }
    assert_eq!(live_memos(&p), 3);

    // (d)
    let c = conflict_copy_id(M, &oid(5));
    let d = conflict_copy_id(&c, &oid(7));
    let mut w = base();
    w.extend([
        at(me(4, "a", T0 + 150, &[], M, 99, "a1"), 1),
        at(me(5, "a", T0 + 200, &[("a", 1)], M, 4, "y"), 2),
        at(me(6, "e", T0 + 151, &[], M, 97, "b1"), 1),
        at(me(7, "e", T0 + 210, &[("e", 1)], M, 6, "y"), 2),
        at(me(8, "x", T0 + 1_000, &[("b", 2)], &c, 96, "z"), 1),
    ]);
    let p = check("d", &w, 300);
    assert_eq!(copies(&p).iter().filter(|(_, l)| *l == oid(5)).count(), 1, "d: one copy for the twin group");
    assert_eq!(memo_of(&p, &c).map(|x| x.text), Some("z".to_owned()));
    assert_eq!(memo_of(&p, &d).map(|x| (x.text, x.head_op_id)), Some(("y".to_owned(), oid(7))));
    for (n, owner) in [(4, &c), (5, &c), (8, &c), (6, &d), (7, &d)] {
        assert_eq!(&p.memo_lineage[&oid(n)], owner, "d: op {n}");
    }
    assert_eq!(invisible(&w, &p), vec![oid(5)], "d: raw oracle flags the merged twin (p8-o1 family)");
}

/// Random respawn-heavy worlds: root M with create 1 and main edit 2; one
/// to three orphan chains (length 1-3, missing first base, M or ghost
/// address, random texts); three to seven orphans, each addressed (3 in 4)
/// to a copy that currently shows a chain head, so ladders of respawns
/// build up, else to a guessed id one to three levels down a chain head's
/// copy ladder (missing base, a chain op, or the main edit; random walls,
/// ctx and texts); child edits of chain ops addressed to a copy; random
/// deletes with ctx and obs. One device per op. Returns the chain heads.
fn respawn_world(seed: u64) -> (Vec<Pub>, BTreeSet<String>) {
    let mut rng = Lcg(seed);
    let mut pick = |n: u64| rng.step() % n.max(1);
    let texts = ["y", "q", "z", "main", "r"];
    let mut w = vec![
        at(mc(1, "m", T0, &[], M, "base"), 1),
        at(me(2, "e", T0 + 2_000 + pick(1_000), &[("m", 1)], M, 1, "main"), 1),
    ];
    let mut devs: Vec<String> = vec!["m".into(), "e".into()];
    let mut chain_ops: Vec<u64> = Vec::new();
    let mut heads: Vec<u64> = Vec::new();
    let mut next = 10u64;
    for _ in 0..1 + pick(3) {
        let addr = if pick(6) == 0 { "ghost-memo" } else { M };
        let mut ctx: Vec<(String, u64)> = if pick(4) == 0 { vec![("m".to_owned(), 1)] } else { Vec::new() };
        for i in 0..1 + pick(3) {
            let n = next;
            next += 1;
            let dev = format!("o{n}");
            let base = if i == 0 { 900 + n } else { n - 1 };
            w.push(at(me(n, &dev, T0 + pick(2_000), &ctx_ref(&ctx), addr, base, texts[pick(3) as usize]), 1));
            ctx.push((dev.clone(), 1));
            devs.push(dev);
            chain_ops.push(n);
        }
        heads.push(next - 1);
    }
    let head_ids: BTreeSet<String> = heads.iter().copied().map(oid).collect();
    let mut copy_ids = vec![M.to_owned()];
    for _ in 0..3 + pick(5) {
        // Mostly a copy that currently shows a chain head (so a newer blind
        // orphan respawns it, level after level), else a guessed ladder id.
        let now = project_all(&w);
        let shown: Vec<String> = copies(&now)
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| memo_of(&now, c).is_some_and(|m| head_ids.contains(&m.head_op_id)))
            .collect();
        let target = if pick(4) != 0 && !shown.is_empty() {
            shown[pick(shown.len() as u64) as usize].clone()
        } else {
            let h = heads[pick(heads.len() as u64) as usize];
            let mut target = M.to_owned();
            for _ in 0..1 + pick(3) {
                target = conflict_copy_id(&target, &oid(if pick(5) == 0 { heads[pick(heads.len() as u64) as usize] } else { h }));
            }
            target
        };
        copy_ids.push(target.clone());
        let n = next;
        next += 1;
        let dev = format!("x{n}");
        let ctx: Vec<(String, u64)> = devs.iter().filter(|_| pick(8) == 0).map(|d| (d.clone(), 1)).collect();
        let base = match pick(8) {
            0 => chain_ops[pick(chain_ops.len() as u64) as usize],
            1 => 2,
            _ => 900 + n,
        };
        w.push(at(me(n, &dev, T0 + 1_500 + pick(4_000), &ctx_ref(&ctx), &target, base, ["s", "t", "r", "q"][pick(4) as usize]), 1));
        devs.push(dev);
    }
    if pick(2) == 0 {
        let n = next;
        next += 1;
        let base = chain_ops[pick(chain_ops.len() as u64) as usize];
        let target = copy_ids[pick(copy_ids.len() as u64) as usize].clone();
        let ctx = [(format!("o{base}"), 1)];
        w.push(at(me(n, &format!("k{n}"), T0 + pick(4_000), &ctx_ref(&ctx), &target, base, texts[pick(5) as usize]), 1));
    }
    for _ in 0..pick(3) {
        let n = next;
        next += 1;
        let target = copy_ids[pick(copy_ids.len() as u64) as usize].clone();
        let ctx: Vec<(String, u64)> = devs.iter().filter(|_| pick(2) == 0).map(|d| (d.clone(), 1)).collect();
        let mut del = md(n, &format!("d{n}"), T0 + pick(5_000), &ctx_ref(&ctx), &target);
        let mut obs: Vec<String> = w.iter().filter(|_| pick(3) == 0).map(|(b, _)| b.op_id.clone()).collect();
        obs.sort();
        del.obs = obs;
        assert_eq!(del.validate(), Ok(()), "seed {seed} delete {n}");
        w.push(at(del, 1));
    }
    (w, head_ids)
}

/// `invisible_up_to_twins`, also accepting a node X when a same-text node T
/// that is not invisible is owned by a conflict copy descending (by
/// `MemoConflictCopy` notices) from X's owner. Identical text is not a
/// conflict (§4.4): the walk merges same-text twins into one frontier, and
/// when that frontier's head is respawned into a copy, its merged twins
/// stay with the source (observation p9-o1, `g3_p9_w6`); the text lives on
/// (shown, or superseded by an edit that saw it) in the descendant copy.
fn invisible_up_to_respawned_twins(w: &[Pub], p: &Projection) -> Vec<String> {
    let raw: BTreeSet<String> = invisible(w, p).into_iter().collect();
    let parent: BTreeMap<&str, &str> = p
        .notices
        .iter()
        .filter_map(|n| match n {
            Notice::MemoConflictCopy { memo_id, copy_id, .. } => Some((copy_id.as_str(), memo_id.as_str())),
            _ => None,
        })
        .collect();
    let descends = |copy: &str, from: &str| {
        let mut at = copy.to_owned();
        for _ in 0..64 {
            match parent.get(at.as_str()) {
                Some(up) if *up == from => return true,
                Some(up) => at = (*up).to_owned(),
                None => return false,
            }
        }
        false
    };
    let text = |b: &OpBody| b.payload.get("text").and_then(Value::as_str).map(str::to_owned);
    let bodies: BTreeMap<&str, &OpBody> = w.iter().map(|(b, _)| (b.op_id.as_str(), b)).collect();
    invisible_up_to_twins(w, p)
        .into_iter()
        .filter(|x| {
            let (xb, owner) = (bodies[x.as_str()], &p.memo_lineage[x]);
            !w.iter().any(|(t, _)| {
                t.op_id != xb.op_id
                    && matches!(t.op_type.as_str(), "create" | "edit")
                    && !raw.contains(&t.op_id)
                    && text(t) == text(xb)
                    && p.memo_lineage.get(&t.op_id).is_some_and(|o| descends(o, owner))
            })
        })
        .collect()
}

/// 500 mixed permutations of respawn-heavy worlds (50 × 10 orders), plus
/// 500 of earlier-generator worlds (`random_memo_world`, new seeds). Each
/// world: one hash, no unrouted op, no invisible edit up to same-text twins
/// (respawn worlds: up to respawned twins, p9-o1), and unique ownership
/// (`ownership_probe`). Also run once locally at 2000 + 1000 worlds: 2595
/// respawned carried heads, no failure, 23 worlds needing the
/// respawned-twin rule.
#[test]
fn g3_p9_w4_unique_ownership_same_hash_and_no_invisible_edit_500_permutations() {
    let mut failures = Vec::new();
    let (mut respawns, mut respawned_twins) = (0, 0);
    for world in 0..50u64 {
        let seed = 0x9100 + world;
        let (w, heads) = respawn_world(seed);
        let p = order_free(&w, 10, seed);
        respawns += p
            .notices
            .iter()
            .filter(|n| matches!(n, Notice::MemoConflictCopy { memo_id, loser_op_id, .. } if memo_id != M && memo_id != "ghost-memo" && heads.contains(loser_op_id)))
            .count();
        let (inv, unr, own) = (invisible_up_to_respawned_twins(&w, &p), unrouted(&w, &p), ownership_probe(&w));
        respawned_twins += usize::from(!invisible_up_to_twins(&w, &p).is_empty());
        if !inv.is_empty() || !unr.is_empty() || !own.is_empty() {
            failures.push(format!("respawn seed {seed:#x}: invisible {inv:?}, unrouted {unr:?}, ownership {own:?}"));
        }
    }
    for world in 0..50u64 {
        let seed = 0x9200 + world;
        let w = random_memo_world(seed);
        let p = order_free(&w, 10, seed);
        let (inv, unr, own) = (invisible_up_to_twins(&w, &p), unrouted(&w, &p), ownership_probe(&w));
        if !inv.is_empty() || !unr.is_empty() || !own.is_empty() {
            failures.push(format!("random seed {seed:#x}: invisible {inv:?}, unrouted {unr:?}, ownership {own:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(respawns >= 30, "generator coverage: {respawns} respawned carried heads");
    assert!(respawned_twins <= 3, "{respawned_twins} worlds need the respawned-twin rule (p9-o1)");
}

/// Every earlier memo family (p4-p8 world builders plus the rich world)
/// passes the ownership probe: no op owned twice anywhere.
#[test]
fn g3_p9_w5_earlier_memo_families_pass_the_ownership_probe() {
    let worlds: Vec<(&str, Vec<Pub>)> = vec![
        ("rich_world", rich_world()),
        ("d1_world", d1_world(0, [M, M, M])),
        ("deep_world", deep_world(None).0),
        ("deep7", deep7(M, None).0),
        ("late7", late7(M)),
        ("forged7", forged7(M, K).0),
        ("cycles7", cycles7(M, K, None)),
        ("self_race self", self_race(M, true, false).0),
        ("self_race seen", self_race(M, false, true).0),
        ("mixed7", mixed7(M, K).0),
        ("chain5", chain5(M, None).0),
        ("late_orphans", late_orphans(M, 0, false).0),
        ("copy_cycles lose", copy_cycles(M, false).0),
        ("copy_cycles win", copy_cycles(M, true).0),
        ("fork_kids", fork_kids(K, &[401], None)),
        ("w1_world", w1_world(M).0),
        ("ladder", ladder(M, 3, None).0),
    ];
    let mut failures = Vec::new();
    for (name, w) in &worlds {
        let got = ownership_probe(w);
        if !got.is_empty() {
            failures.push(format!("{name}: {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Observation p9-o1 (found by `g3_p9_w4`: 23 of 2000 respawn-heavy worlds
/// before `invisible_up_to_respawned_twins`), pinned. Chains A (4 → 5) and
/// B (6 → 7) both end in "z", lose at M as one same-text group, and share
/// C = copy(M, 5); C's walk merges the twins 5 and 7 (5 newer, head). A
/// blind newer orphan 8 wins in C, so 5 is respawned into D = copy(C, 5)
/// with its chain only; B's chain stays C content (the w1 fix moves the
/// losing head's chain, not the merged twins' chains). 9, addressed to D
/// and seeing 5 only, then supersedes "z" there. No op is owned twice and
/// the text "z" was seen by its superseder, but 7 is shown nowhere, the
/// raw oracle flags it, and a delete of C that saw 8 (all C shows) is
/// restored by the unseen 6 and 7 (p8-o1 / `g3_p8_f7` family).
#[test]
fn g3_p9_w6_merged_twin_chain_stays_in_the_source_copy() {
    let c = conflict_copy_id(M, &oid(5));
    let d = conflict_copy_id(&c, &oid(5));
    let w = vec![
        at(mc(1, "b", T0 + 100, &[], M, "base"), 1),
        at(me(3, "b", T0 + 900, &[("b", 1)], M, 1, "main"), 2),
        at(me(4, "a", T0 + 150, &[], M, 99, "a1"), 1),
        at(me(5, "a", T0 + 200, &[("a", 1)], M, 4, "z"), 2),
        at(me(6, "e", T0 + 151, &[], M, 97, "b1"), 1),
        at(me(7, "e", T0 + 180, &[("e", 1)], M, 6, "z"), 2),
        at(me(8, "x", T0 + 1_000, &[("b", 2)], &c, 96, "t"), 1),
        at(me(9, "y", T0 + 1_100, &[("a", 2)], &d, 95, "r"), 1),
    ];
    let p = order_free(&w, 300, 0x9060);
    assert_eq!(copies(&p).len(), 2, "one copy for the twin group, one respawn");
    assert_eq!(memo_of(&p, &c).map(|x| (x.text, x.head_op_id)), Some(("t".to_owned(), oid(8))));
    assert_eq!(memo_of(&p, &d).map(|x| (x.text, x.head_op_id, x.conflict_of)), Some(("r".to_owned(), oid(9), Some(c.clone()))));
    for (n, owner) in [(4, &d), (5, &d), (9, &d), (6, &c), (7, &c), (8, &c)] {
        assert_eq!(&p.memo_lineage[&oid(n)], owner, "op {n}");
    }
    assert!(!memo_texts(&p).contains("z"), "no memo shows z");
    assert_eq!(invisible(&w, &p), vec![oid(7)], "raw oracle: the stayed twin 7");
    assert_eq!(invisible_up_to_twins(&w, &p), vec![oid(7)], "the respawned twin 5 is superseded, not shown");
    assert!(invisible_up_to_respawned_twins(&w, &p).is_empty());
    assert!(unrouted(&w, &p).is_empty());
    assert_eq!(ownership_probe(&w), Vec::<String>::new(), "no op owned twice");
    let mut wd = w.clone();
    wd.push(at(with_obs(md(10, "d", T0 + 2_000, &[], &c), &[8]), 1));
    let pd = order_free(&wd, 100, 0x9061);
    assert!(is_restored(&pd, Kind::Memo, &c), "6 and 7 are C content the delete did not see");
}
