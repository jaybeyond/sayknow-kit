//! G10 red team: the uncommitted G10 fix + performance diff.
//!
//! 1. `Store::plan_local_batch` / `reproject_delta` must leave exactly the
//!    rows a full SQL load + rewrite leaves (`dump_state` twin comparison,
//!    plus "a full rewrite over the planned rows changes nothing").
//! 2. `StoreMemoSink` keeps its observable contract on the planned path.
//! 4. `apply_snapshot` moving `retired` cursors: never backwards, never a
//!    forked stream, and a covered range with other content is never applied
//!    twice.
//! 5. `seal_pending_capped(now)` stamps the caller's (cycle) clock, so the
//!    30-day trigger is judged by that clock alone.
//!
//! Surfaces 3 and 6 live at the end of `sim3.rs` (its helpers are private).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::compaction::{build_snapshot, compaction_due, Snapshot, MAX_SEGMENT_AGE_MS};
use super::identity::DeviceIdentity;
use super::memo_bridge::{MemoSink, StoreMemoSink};
use super::merge::EntityState;
use super::ops::{Dot, HlcClock, Kind, OpBody};
use super::rotation::UploadState;
use super::sim_tests::{
    all, create, delete, device, edit, identity, local, oid, op, publish, remote_key, seal_next, sync_stream, Remote, K, M,
    VAULT,
};
use super::store::{ApplyOutcome, CursorState, Store};
use crate::clipboard::{make_preview, ClipEntry, EntryKind};

const CONV: &str = "0190a000-0000-7000-8000-0000000000cc";
const TR: &str = "0190a000-0000-7000-8000-0000000000dd";
const POLICY: &str = "retention";
const VAULT2: &str = "vault-2";

fn count(store: &Store, sql: &str) -> i64 {
    store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn rows(store: &Store, sql: &str) -> Vec<String> {
    let mut stmt = store.conn().prepare(sql).unwrap();
    let n = stmt.column_count();
    let mut out = Vec::new();
    let mut q = stmt.query([]).unwrap();
    while let Some(row) = q.next().unwrap() {
        out.push((0..n).map(|i| format!("{:?}", row.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"));
    }
    out
}

fn msg(n: u64, wall: u64, id: &str, parent: Option<&str>) -> OpBody {
    op(
        n,
        wall,
        Kind::Conv,
        CONV,
        "msg.add",
        json!({"conv_id": CONV, "msg_id": id, "parent_id": parent, "role": "user",
               "content": id, "attachments": [], "ts": wall}),
    )
}

/// Two stores fed the same history: `planned` takes its batches through
/// `plan_local_batch`, `full` through sequential `insert_local_op`.
struct Twin {
    planned: Store,
    full: Store,
}

impl Twin {
    fn new(name: &str) -> Twin {
        Twin { planned: device(name), full: device(name) }
    }

    fn both(&mut self, f: impl Fn(&mut Store)) {
        f(&mut self.planned);
        f(&mut self.full);
    }

    /// Seals the same own range on both, publishes the planned copy's bytes
    /// (the plaintext and chain hashes agree; only the nonce differs).
    fn publish(&mut self, remote: &mut Remote) {
        let (seg, created) = publish(&mut self.planned, remote).expect("something to seal");
        assert!(created);
        let mine = seal_next(&mut self.full).expect("something to seal");
        assert_eq!((mine.first_seq, mine.last_seq), (seg.first_seq, seg.last_seq));
        self.full.mark_attempted(VAULT, &mine.device_id, mine.first_seq).unwrap();
        self.full.mark_published(VAULT, &mine.device_id, mine.first_seq).unwrap();
    }

    fn sync(&mut self, remote: &Remote, from: &str) {
        self.both(|s| {
            sync_stream(s, remote, from, &all);
        });
    }

    /// One planned batch against `vault` versus the same bodies one by one;
    /// the persisted rows, the projection hash, and a full rewrite over the
    /// planned rows must all agree. Returns the planned store's dump.
    fn batch(&mut self, vault: &str, bodies: impl Fn() -> Vec<OpBody>) -> String {
        let expected = bodies().len();
        let (ops, seen) = self.planned.plan_local_batch(vault, |p| (bodies(), p.entities.len())).unwrap();
        assert_eq!(ops.len(), expected);
        let mut last = None;
        for body in bodies() {
            last = Some(self.full.insert_local_op(vault, body, UploadState::Never).unwrap());
        }
        let planned_dump = self.planned.dump_state().unwrap();
        let full_dump = self.full.dump_state().unwrap();
        if let (Some(mine), Some(theirs)) = (ops.last(), last.as_ref()) {
            assert_eq!(mine.projection_hash, theirs.projection_hash, "projection hash");
            assert_eq!(mine.dot, theirs.dot, "same own seq consumed");
        }
        assert_dump_eq(&planned_dump, &full_dump, "planned vs full path");
        // A full rewrite over the planned rows must change nothing.
        self.planned.reproject(vault).unwrap();
        assert_dump_eq(&self.planned.dump_state().unwrap(), &planned_dump, "full rewrite over planned rows");
        let _ = seen;
        planned_dump
    }
}

/// Line diff on mismatch: the assertion message names the first rows that
/// differ instead of dumping two whole stores.
fn assert_dump_eq(left: &str, right: &str, what: &str) {
    if left == right {
        return;
    }
    let l: Vec<&str> = left.lines().collect();
    let r: Vec<&str> = right.lines().collect();
    let only_left: Vec<&&str> = l.iter().filter(|x| !r.contains(x)).collect();
    let only_right: Vec<&&str> = r.iter().filter(|x| !l.contains(x)).collect();
    panic!("{what}: rows differ\n  only in planned: {only_left:?}\n  only in full: {only_right:?}");
}

// ===========================================================================
// 1. plan_local_batch / reproject_delta vs the full-rewrite path
// ===========================================================================

#[test]
fn g10_1_memo_lifecycle_two_consecutive_planned_batches() {
    let mut t = Twin::new("a");
    t.both(|s| {
        local(s, create(1, 1_000, M, "m"));
        local(s, create(2, 2_000, K, "k"));
    });
    t.batch(VAULT, || {
        vec![
            edit(3, 3_000, M, 1, "m2"),
            delete(4, 4_000, K),
            op(5, 5_000, Kind::Memo, M, "pin", json!({"pinned": true})),
            op(6, 6_000, Kind::Memo, M, "note", json!({"note": "n"})),
        ]
    });
    assert_eq!(count(&t.planned, &format!("SELECT deleted FROM entities WHERE id = '{K}'")), 1);
    // Second planned batch on top of the first: an edit, a create under the
    // tombstoned id (seen the delete: stays deleted), a brand-new memo.
    t.batch(VAULT, || {
        vec![
            edit(7, 7_000, M, 3, "m3"),
            create(8, 8_000, K, "k-again"),
            create(9, 9_000, "0190a000-0000-7000-8000-0000000000ee", "new"),
        ]
    });
    assert!(matches!(
        t.planned.projection(VAULT).unwrap().entities.get(&(Kind::Memo, K.to_owned())),
        Some(EntityState::Deleted)
    ));
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM memo_lineage"), 6, "create/edit nodes: 1,2,3,7,8,9");
}

#[test]
fn g10_1_conv_messages_parent_chain_set_active_and_rename() {
    let mut t = Twin::new("a");
    t.both(|s| {
        local(s, op(1, 1_000, Kind::Conv, CONV, "create", json!({"title": "t"})));
        local(s, msg(2, 2_000, "m1", None));
    });
    t.batch(VAULT, || {
        vec![
            msg(3, 3_000, "m2", Some("m1")),
            msg(4, 4_000, "m3", Some("m1")),
            op(5, 5_000, Kind::Conv, CONV, "set_active", json!({"leaf": "m2"})),
            op(6, 6_000, Kind::Conv, CONV, "rename", json!({"title": "renamed"})),
        ]
    });
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM msg_tree"), 3);
    let Some(EntityState::Conv(conv)) = t.planned.projection(VAULT).unwrap().entities.get(&(Kind::Conv, CONV.to_owned())).cloned()
    else {
        panic!("conv missing")
    };
    assert_eq!((conv.title.as_str(), conv.active_leaf.as_deref()), ("renamed", Some("m2")));
}

/// A planned batch that deletes a conversation: the full rewrite leaves no
/// `msg_tree` rows for it (it rewrites the table from the projection, and a
/// tombstone carries no messages); the delta path must agree.
#[test]
fn g10_1_planned_batch_that_deletes_a_conv_drops_its_msg_tree_rows() {
    let mut t = Twin::new("a");
    t.both(|s| {
        local(s, op(1, 1_000, Kind::Conv, CONV, "create", json!({"title": "t"})));
        local(s, msg(2, 2_000, "m1", None));
        local(s, msg(3, 3_000, "m2", Some("m1")));
        local(s, create(4, 4_000, M, "m"));
    });
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM msg_tree"), 2);
    t.batch(VAULT, || vec![op(5, 5_000, Kind::Conv, CONV, "delete", json!({})), edit(6, 6_000, M, 4, "m2")]);
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM msg_tree"), 0, "a tombstoned conv has no message rows");
    assert_eq!(count(&t.full, "SELECT COUNT(*) FROM msg_tree"), 0);
}

#[test]
fn g10_1_tr_and_policy_ops_then_tr_delete() {
    let mut t = Twin::new("a");
    t.both(|s| local(s, create(1, 1_000, M, "m")));
    t.batch(VAULT, || {
        vec![
            op(2, 2_000, Kind::Tr, TR, "tr.add", json!({"src": "hello", "dst": "안녕", "pinned": false})),
            op(3, 3_000, Kind::Tr, TR, "tr.pin", json!({"pinned": true})),
            op(4, 4_000, Kind::Policy, POLICY, "policy.retention", json!({"days": 30})),
        ]
    });
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM entities WHERE kind = 'tr'"), 1);
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM entities WHERE kind = 'policy'"), 1);
    t.batch(VAULT, || {
        vec![
            op(5, 5_000, Kind::Tr, TR, "tr.delete", json!({})),
            op(6, 6_000, Kind::Policy, POLICY, "policy.retention", json!({"days": 7})),
        ]
    });
    assert_eq!(count(&t.planned, "SELECT deleted FROM entities WHERE kind = 'tr'"), 1);
}

#[test]
fn g10_1_evicted_local_flag_survives_an_edit_a_bystander_batch_and_a_delete() {
    let mut t = Twin::new("a");
    t.both(|s| {
        local(s, create(1, 1_000, M, "m"));
        local(s, create(2, 2_000, K, "k"));
        s.conn().execute("UPDATE entities SET evicted_local = 1 WHERE id = ?1", [K]).unwrap();
    });
    // The batch edits the evicted entity.
    t.batch(VAULT, || vec![edit(3, 3_000, K, 2, "k2")]);
    assert_eq!(count(&t.planned, &format!("SELECT evicted_local FROM entities WHERE id = '{K}'")), 1);
    // The batch leaves it alone.
    t.batch(VAULT, || vec![edit(4, 4_000, M, 1, "m2")]);
    assert_eq!(count(&t.planned, &format!("SELECT evicted_local FROM entities WHERE id = '{K}'")), 1);
    // The batch tombstones it: the flag is kept on the tombstone row.
    t.batch(VAULT, || vec![delete(5, 5_000, K)]);
    assert_eq!(
        rows(&t.planned, &format!("SELECT deleted, evicted_local FROM entities WHERE id = '{K}'")),
        rows(&t.full, &format!("SELECT deleted, evicted_local FROM entities WHERE id = '{K}'"))
    );
}

/// Concurrent edits of one base from two devices: the loser becomes a
/// conflict copy. A planned edit whose base is that losing node changes
/// lineage routes; the delta path must write the same `memo_lineage` rows.
#[test]
fn g10_1_lineage_edit_based_on_a_losing_conflict_copy() {
    let mut remote = Remote::new(true);
    let mut t = Twin::new("a");
    let mut b = device("b");
    t.both(|s| local(s, create(1, 1_000, M, "base")));
    t.publish(&mut remote);
    sync_stream(&mut b, &remote, "a", &all);
    local(&mut b, edit(20, 2_000, M, 1, "b-text"));
    assert!(publish(&mut b, &mut remote).unwrap().1);
    t.both(|s| local(s, edit(10, 2_500, M, 1, "a-text")));
    t.sync(&remote, "b");
    let projection = t.planned.projection(VAULT).unwrap();
    let copy = projection
        .entities
        .iter()
        .find_map(|((kind, id), s)| match s {
            EntityState::Memo(m) if *kind == Kind::Memo && m.conflict_of.as_deref() == Some(M) => {
                Some((id.clone(), m.head_op_id.clone()))
            }
            _ => None,
        })
        .expect("a conflict copy of M");
    let loser = copy.1.clone();
    assert!(loser == oid(10) || loser == oid(20), "{loser}");
    let lineage_before = rows(&t.planned, "SELECT op_id, routed_to FROM memo_lineage ORDER BY 1");
    assert_eq!(lineage_before.len(), 3);
    // Base the new edit on the losing node, addressed at M (not the copy).
    let loser_n = if loser == oid(10) { 10 } else { 20 };
    t.batch(VAULT, || vec![edit(30, 3_000, M, loser_n, "z")]);
    let lineage_after = rows(&t.planned, "SELECT op_id, routed_to FROM memo_lineage ORDER BY 1");
    assert_eq!(lineage_after.len(), 4);
    assert_ne!(lineage_after[..], lineage_before[..]);
    // And a second batch editing the copy entity itself.
    let copy_id = copy.0.clone();
    let head_after: String = match t.planned.projection(VAULT).unwrap().entities.get(&(Kind::Memo, copy_id.clone())) {
        Some(EntityState::Memo(m)) => m.head_op_id.clone(),
        other => panic!("copy {copy_id} is {other:?}"),
    };
    t.batch(VAULT, || {
        vec![op(31, 3_100, Kind::Memo, &copy_id, "edit", json!({"text": "copy-edit", "base_op_id": head_after}))]
    });
}

#[test]
fn g10_1_batch_against_a_non_active_vault_writes_lineage_but_no_entities() {
    let mut t = Twin::new("a");
    let other = DeviceIdentity { v: 1, vault_id: VAULT2.to_owned(), device_id: "a".to_owned(), install_fp: None, generation: 0 };
    t.both(|s| {
        s.init_identity(&other).unwrap();
        local(s, create(1, 1_000, M, "active vault memo"));
    });
    assert_eq!(t.planned.active_vault().unwrap().as_deref(), Some(VAULT));
    let entities_before = rows(&t.planned, "SELECT * FROM entities ORDER BY 1, 2");
    t.batch(VAULT2, || {
        vec![
            create(2, 2_000, K, "other vault memo"),
            op(3, 3_000, Kind::Conv, CONV, "create", json!({"title": "t"})),
            msg(4, 4_000, "m1", None),
            edit(5, 5_000, K, 2, "other vault memo 2"),
        ]
    });
    assert_eq!(rows(&t.planned, "SELECT * FROM entities ORDER BY 1, 2"), entities_before, "entities belong to the active vault");
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM msg_tree"), 0);
    assert_eq!(count(&t.planned, &format!("SELECT COUNT(*) FROM memo_lineage WHERE vault_id = '{VAULT2}'")), 2);
    // Back on the active vault: still equal.
    t.batch(VAULT, || vec![edit(6, 6_000, M, 1, "m2")]);
}

#[test]
fn g10_1_planned_batch_after_a_remote_segment_and_a_restore() {
    let mut remote = Remote::new(true);
    let mut t = Twin::new("a");
    let mut b = device("b");
    t.both(|s| local(s, create(1, 1_000, M, "base")));
    t.publish(&mut remote);
    sync_stream(&mut b, &remote, "a", &all);
    // The twins delete M; B edits it concurrently (never saw the delete) and
    // also brings a new memo and a conversation.
    t.both(|s| local(s, delete(2, 2_000, M)));
    local(&mut b, edit(20, 2_100, M, 1, "b-edit"));
    local(&mut b, create(21, 2_200, K, "b-memo"));
    local(&mut b, op(22, 2_300, Kind::Conv, CONV, "create", json!({"title": "b-conv"})));
    local(&mut b, msg(23, 2_400, "m1", None));
    assert!(publish(&mut b, &mut remote).unwrap().1);
    t.sync(&remote, "b");
    let p = t.planned.projection(VAULT).unwrap();
    assert!(matches!(p.entities.get(&(Kind::Memo, M.to_owned())), Some(EntityState::Memo(m)) if m.text == "b-edit"), "restored by the concurrent edit");
    // Planned batch on the restored memo, the remote memo and the remote conv.
    t.batch(VAULT, || {
        vec![
            edit(3, 3_000, M, 20, "after restore"),
            edit(4, 4_000, K, 21, "edit remote memo"),
            msg(5, 5_000, "m2", Some("m1")),
        ]
    });
    assert_eq!(count(&t.planned, "SELECT COUNT(*) FROM msg_tree"), 2);
}

#[test]
fn g10_1_planned_batch_after_apply_rotation() {
    let mut remote = Remote::new(true);
    let mut t = Twin::new("a");
    t.both(|s| local(s, create(1, 1_000, M, "base")));
    t.publish(&mut remote);
    // An unsynced own op above the verified prefix is rewritten by the rotation.
    t.both(|s| local(s, create(2, 2_000, K, "unsynced")));
    let remote_hashes = remote.hashes_by_seq("a");
    t.both(|s| {
        let source = s.rotation_input(VAULT, remote_hashes.clone()).unwrap();
        let plan = Store::plan_rotation(&source, "a2", || oid(99)).unwrap();
        s.apply_rotation(VAULT, &plan, &remote_hashes, &identity("a2")).unwrap();
        assert_eq!(s.device_id(VAULT).unwrap(), "a2");
    });
    assert_dump_eq(&t.planned.dump_state().unwrap(), &t.full.dump_state().unwrap(), "after rotation");
    t.batch(VAULT, || vec![edit(3, 3_000, M, 1, "post-rotation"), edit(4, 4_000, K, 2, "k2")]);
    assert_eq!(t.planned.cursor(VAULT, "a").unwrap().unwrap().state, CursorState::Retired);
}

// ===========================================================================
// 2. StoreMemoSink on the planned path
// ===========================================================================

const ENTRY: &str = "memo-0190a000-0000-7000-8000-0000000000aa";

fn memo(id: &str, text: &str) -> ClipEntry {
    ClipEntry {
        id: id.into(),
        text: text.into(),
        preview: make_preview(text),
        ts: 10,
        pinned: false,
        note: None,
        kind: EntryKind::Memo,
        conflict_of: None,
    }
}

fn sink(store: Store) -> StoreMemoSink {
    StoreMemoSink { store: Arc::new(Mutex::new(store)), vault: VAULT.to_owned(), clock: Arc::new(Mutex::new(HlcClock::default())) }
}

fn op_count(s: &StoreMemoSink) -> i64 {
    count(&s.store.lock().unwrap(), "SELECT COUNT(*) FROM ops")
}

fn projected(s: &StoreMemoSink, id: &str) -> Option<EntityState> {
    s.store.lock().unwrap().projection(VAULT).unwrap().entities.get(&(Kind::Memo, id.to_owned())).cloned()
}

#[test]
fn g10_2_sink_recreates_a_remotely_deleted_memo_under_a_fresh_id() {
    let mut remote = Remote::new(true);
    let s = sink(device("a"));
    let m = memo(ENTRY, "hello");
    assert_eq!(s.changed(None, Some(&m)).unwrap(), None);
    {
        let mut store = s.store.lock().unwrap();
        assert!(publish(&mut store, &mut remote).unwrap().1);
    }
    let mut b = device("b");
    sync_stream(&mut b, &remote, "a", &all);
    local(&mut b, op(50, 5_000, Kind::Memo, ENTRY, "delete", json!({})));
    assert!(publish(&mut b, &mut remote).unwrap().1);
    {
        let mut store = s.store.lock().unwrap();
        sync_stream(&mut store, &remote, "b", &all);
    }
    assert_eq!(projected(&s, ENTRY), Some(EntityState::Deleted));
    let before = op_count(&s);
    let mut edited = m.clone();
    edited.text = "still mine".into();
    let rebound = s.changed(Some(&m), Some(&edited)).unwrap().expect("rebound to a fresh id");
    assert_ne!(rebound, ENTRY);
    assert_eq!(op_count(&s), before + 1, "one create under the fresh id");
    assert!(matches!(projected(&s, &rebound), Some(EntityState::Memo(st)) if st.text == "still mine"));
    assert_eq!(projected(&s, ENTRY), Some(EntityState::Deleted), "the tombstone stays");
    // The delta path's persisted rows equal a full rewrite.
    let mut store = s.store.lock().unwrap();
    let dump = store.dump_state().unwrap();
    store.reproject(VAULT).unwrap();
    assert_dump_eq(&store.dump_state().unwrap(), &dump, "sink rows vs full rewrite");
}

#[test]
fn g10_2_sink_delete_of_unknown_ids_duplicates_and_id_mismatch() {
    let s = sink(device("a"));
    let a = memo(ENTRY, "a");
    s.changed(None, Some(&a)).unwrap();
    let base = op_count(&s);

    // `changed` to None for an id the store never saw: no live head, no op.
    let ghost = memo("memo-ghost", "ghost");
    assert_eq!(s.changed(Some(&ghost), None).unwrap(), None);
    assert_eq!(op_count(&s), base);

    // `delete_all` of an unknown id: one blind tombstone, committed.
    s.delete_all(&["memo-unknown".to_owned()]).unwrap();
    assert_eq!(op_count(&s), base + 1);
    assert!(matches!(projected(&s, ENTRY), Some(EntityState::Memo(_))), "the known memo is untouched");

    // Duplicates collapse to one delete per resolved id; the empty list is a no-op.
    let other = memo("memo-2", "b");
    s.changed(None, Some(&other)).unwrap();
    let before = op_count(&s);
    s.delete_all(&["memo-2".to_owned(), ENTRY.to_owned(), "memo-2".to_owned(), ENTRY.to_owned()]).unwrap();
    assert_eq!(op_count(&s), before + 2);
    assert_eq!(projected(&s, ENTRY), Some(EntityState::Deleted));
    assert_eq!(projected(&s, "memo-2"), Some(EntityState::Deleted));
    s.delete_all(&[]).unwrap();
    assert_eq!(op_count(&s), before + 2);

    // before/after naming different ids: `after` wins, `before` is ignored
    // (it is not the same entry), so a memo new to the store is created and
    // the other id is untouched.
    let x = memo("memo-x", "x");
    let y = memo("memo-y", "y");
    s.changed(None, Some(&x)).unwrap();
    let before = op_count(&s);
    assert_eq!(s.changed(Some(&x), Some(&y)).unwrap(), None);
    assert_eq!(op_count(&s), before + 1, "one create for y");
    assert!(matches!(projected(&s, "memo-x"), Some(EntityState::Memo(st)) if st.text == "x"));
    assert!(matches!(projected(&s, "memo-y"), Some(EntityState::Memo(st)) if st.text == "y"));
    // And a no-op change plans nothing (no seq consumed).
    assert_eq!(s.changed(Some(&y), Some(&y)).unwrap(), None);
    assert_eq!(op_count(&s), before + 1);
    let mut store = s.store.lock().unwrap();
    let dump = store.dump_state().unwrap();
    store.reproject(VAULT).unwrap();
    assert_dump_eq(&store.dump_state().unwrap(), &dump, "sink rows vs full rewrite");
}

// ===========================================================================
// 4. apply_snapshot and retired cursors
// ===========================================================================

fn set_retired(store: &Store, device: &str) {
    let n = store
        .conn()
        .execute("UPDATE cursors SET state = 'retired' WHERE vault_id = ?1 AND remote_device_id = ?2", [VAULT, device])
        .unwrap();
    assert_eq!(n, 1);
}

/// Writer `x` publishes seq 1, 2, 3 as three segments.
fn writer_x(remote: &mut Remote) -> Store {
    let mut x = device("x");
    local(&mut x, create(1, 1_000, M, "one"));
    assert!(publish(&mut x, remote).unwrap().1);
    local(&mut x, edit(2, 2_000, M, 1, "two"));
    assert!(publish(&mut x, remote).unwrap().1);
    local(&mut x, edit(3, 3_000, M, 2, "three"));
    assert!(publish(&mut x, remote).unwrap().1);
    x
}

fn snapshot_of_prefix(remote: &Remote, upto: u64) -> Snapshot {
    let mut s = device("s");
    sync_stream(&mut s, remote, "x", &|key, _| (1..=upto).any(|n| key == remote_key("x", n, n)));
    let snap = build_snapshot(&s, VAULT).unwrap();
    assert_eq!(snap.frontier.get("x"), Some(&upto), "{:?}", snap.frontier);
    snap
}

#[test]
fn g10_4_snapshot_below_a_retired_cursor_never_regresses_it() {
    let mut remote = Remote::new(true);
    let _x = writer_x(&mut remote);
    let mut r = device("r");
    sync_stream(&mut r, &remote, "x", &all);
    set_retired(&r, "x");
    let before = r.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!((before.state, before.applied_seq), (CursorState::Retired, 3));
    assert!(before.chain_head.is_some());
    let older = snapshot_of_prefix(&remote, 2);
    let outcome = r.apply_snapshot(VAULT, &older, "snap/older").unwrap();
    assert!(matches!(outcome, ApplyOutcome::Applied { conflicts: 0, fork_started: false, .. }), "{outcome:?}");
    let after = r.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!(after, before, "a frontier below the retired cursor leaves it untouched (incl. its chain head)");
    // Same for an `ok` cursor, and for an equal frontier.
    let mut r2 = device("r2");
    sync_stream(&mut r2, &remote, "x", &all);
    let before = r2.cursor(VAULT, "x").unwrap().unwrap();
    r2.apply_snapshot(VAULT, &older, "snap/older").unwrap();
    assert_eq!(r2.cursor(VAULT, "x").unwrap().unwrap(), before);
    let equal = snapshot_of_prefix(&remote, 3);
    r2.apply_snapshot(VAULT, &equal, "snap/equal").unwrap();
    assert_eq!(r2.cursor(VAULT, "x").unwrap().unwrap(), before);
}

#[test]
fn g10_4_newer_then_older_snapshot_keeps_ok_and_retired_cursors_monotone() {
    let mut remote = Remote::new(true);
    let _x = writer_x(&mut remote);
    let newer = snapshot_of_prefix(&remote, 3);
    let older = snapshot_of_prefix(&remote, 2);
    // A fresh reader: no cursor → the newer snapshot creates an `ok` cursor at 3.
    let mut r = device("r");
    r.apply_snapshot(VAULT, &newer, "snap/newer").unwrap();
    let c = r.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!((c.state, c.applied_seq, c.chain_head), (CursorState::Ok, 3, None));
    r.apply_snapshot(VAULT, &older, "snap/older").unwrap();
    assert_eq!(r.cursor(VAULT, "x").unwrap().unwrap().applied_seq, 3);
    // A retired cursor behind the snapshot moves up and stays retired; the
    // older snapshot afterwards does not move it back.
    let mut q = device("q");
    sync_stream(&mut q, &remote, "x", &|key, _| key == remote_key("x", 1, 1));
    assert_eq!(q.cursor(VAULT, "x").unwrap().unwrap().applied_seq, 1);
    set_retired(&q, "x");
    q.apply_snapshot(VAULT, &newer, "snap/newer").unwrap();
    let c = q.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!((c.state, c.applied_seq, c.chain_head), (CursorState::Retired, 3, None));
    q.apply_snapshot(VAULT, &older, "snap/older").unwrap();
    let c = q.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!((c.state, c.applied_seq), (CursorState::Retired, 3));
    // Idempotent: the newer one again changes nothing.
    let dump = q.dump_state().unwrap();
    q.apply_snapshot(VAULT, &newer, "snap/newer").unwrap();
    assert_eq!(q.dump_state().unwrap(), dump);
}

#[test]
fn g10_4_snapshot_with_a_fork_on_the_retired_stream_does_not_advance_it() {
    let mut remote = Remote::new(true);
    let _x = writer_x(&mut remote);
    let mut q = device("q");
    sync_stream(&mut q, &remote, "x", &|key, _| key == remote_key("x", 1, 1));
    set_retired(&q, "x");
    let mut forked = snapshot_of_prefix(&remote, 3);
    forked.forks.insert("x".to_owned(), 2);
    let outcome = q.apply_snapshot(VAULT, &forked, "snap/forked").unwrap();
    assert!(matches!(outcome, ApplyOutcome::Applied { .. }), "{outcome:?}");
    let c = q.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!((c.state, c.applied_seq), (CursorState::Retired, 1), "forked in the snapshot: never moved");
    assert_eq!(q.forks(VAULT).unwrap().get("x"), Some(&2));
    // Held cursors are never moved either.
    let mut h = device("h");
    sync_stream(&mut h, &remote, "x", &|key, _| key == remote_key("x", 1, 1));
    h.conn().execute("UPDATE cursors SET state = 'held' WHERE remote_device_id = 'x'", []).unwrap();
    let plain = snapshot_of_prefix(&remote, 3);
    h.apply_snapshot(VAULT, &plain, "snap/plain").unwrap();
    let c = h.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!((c.state, c.applied_seq), (CursorState::Held, 1));
}

/// The retired cursor was moved to 3 by a snapshot (no chain head). A
/// segment of a clone of `x` with other content at the covered seq 2 arrives
/// later: it must not count as a duplicate, must not be applied twice, must
/// not move the cursor back, and a second read of it is a no-op.
#[test]
fn g10_4_other_content_at_a_covered_seq_after_a_snapshot_advance_is_not_applied_twice() {
    let mut remote = Remote::new(true);
    let _x = writer_x(&mut remote);
    let newer = snapshot_of_prefix(&remote, 3);
    let mut q = device("q");
    sync_stream(&mut q, &remote, "x", &|key, _| key == remote_key("x", 1, 1));
    set_retired(&q, "x");
    q.apply_snapshot(VAULT, &newer, "snap/newer").unwrap();
    assert_eq!(q.cursor(VAULT, "x").unwrap().unwrap().applied_seq, 3);
    let ops_before = count(&q, "SELECT COUNT(*) FROM ops");
    let dots_before = count(&q, "SELECT COUNT(*) FROM op_dots");

    // A clone of x (same id, same seq 1) writes something else at seq 2.
    let mut other = Remote::new(true);
    let mut y = device("x");
    local(&mut y, create(1, 1_000, M, "one"));
    assert!(publish(&mut y, &mut other).unwrap().1);
    local(&mut y, create(50, 5_000, K, "clone content"));
    assert!(publish(&mut y, &mut other).unwrap().1);
    let outcomes = sync_stream(&mut q, &other, "x", &|key, _| key == remote_key("x", 2, 2));
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(!matches!(outcomes[0], ApplyOutcome::Duplicate), "other content is not a duplicate: {outcomes:?}");
    assert!(
        matches!(outcomes[0], ApplyOutcome::Applied { fork_started: true, .. } | ApplyOutcome::Quarantined { .. }),
        "a second writer at a covered dot is a fork or quarantine, never a plain apply: {:?}",
        outcomes[0]
    );
    let c = q.cursor(VAULT, "x").unwrap().unwrap();
    assert_eq!(c.applied_seq, 3, "never backwards");
    assert_ne!(c.state, CursorState::Held);
    assert!(count(&q, "SELECT COUNT(*) FROM ops") <= ops_before + 1);
    assert!(count(&q, "SELECT COUNT(*) FROM op_dots") <= dots_before + 1);
    assert_eq!(q.dots_of(VAULT, &oid(2)).unwrap(), vec![Dot { device: "x".to_owned(), seq: 2 }], "x's own seq 2 keeps one dot");
    // Reading the same name again changes nothing.
    let dump = q.dump_state().unwrap();
    let again = sync_stream(&mut q, &other, "x", &|key, _| key == remote_key("x", 2, 2));
    assert!(matches!(again[0], ApplyOutcome::Duplicate | ApplyOutcome::Skipped(_) | ApplyOutcome::Applied { conflicts: 0, .. }), "{again:?}");
    assert_eq!(q.dump_state().unwrap(), dump, "a re-read is a no-op");
}

// ===========================================================================
// 5. seal_pending_capped(now): the compaction trigger follows the caller's clock
// ===========================================================================

#[test]
fn g10_5_compaction_is_judged_by_the_sealing_clock_not_the_wall_clock() {
    let mut store = device("a");
    local(&mut store, create(1, 1_000, M, "m"));
    // A cycle clock far behind the wall clock (as the simulator's is).
    let cycle_now: u64 = 1_000_000;
    let segs = store.seal_pending_capped(VAULT, usize::MAX, cycle_now, |_, plain| Ok(plain.to_vec())).unwrap();
    assert_eq!(segs.len(), 1);
    let pending = store.pending_segments(VAULT, "a").unwrap();
    assert_eq!(pending, vec![(1, 1, cycle_now)], "created_at is the caller's clock");
    assert!(!compaction_due(&store, VAULT, cycle_now).unwrap());
    assert!(!compaction_due(&store, VAULT, cycle_now + MAX_SEGMENT_AGE_MS - 1).unwrap());
    assert!(compaction_due(&store, VAULT, cycle_now + MAX_SEGMENT_AGE_MS).unwrap());
    // A clock *behind* the stamp is never due (no wrap-around).
    assert!(!compaction_due(&store, VAULT, 0).unwrap());
    // Two segments: the oldest stamp decides.
    local(&mut store, edit(2, 2_000, M, 1, "m2"));
    store.seal_pending_capped(VAULT, usize::MAX, cycle_now + 5_000, |_, plain| Ok(plain.to_vec())).unwrap();
    let stamps: BTreeMap<u64, u64> = store.pending_segments(VAULT, "a").unwrap().into_iter().map(|(f, _, c)| (f, c)).collect();
    assert_eq!(stamps, BTreeMap::from([(1, cycle_now), (2, cycle_now + 5_000)]));
    assert!(compaction_due(&store, VAULT, cycle_now + MAX_SEGMENT_AGE_MS).unwrap());
}
