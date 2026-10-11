//! G4 red team (ultragoal G004): adversarial tests for memo storage hardening
//! and the M1–M10 migration (approved plan §4.10, §4.4; AC 9, 10, 26, 28;
//! `_sync/g4-design.md`), written against the contract rather than the
//! implementation.
//!
//! A real defect is reproduced as `#[ignore = "G4 blocker: ..."]`; every
//! other case is a normal test. Scratch files live under the system temp dir
//! (`$TMPDIR`) through `sim_tests::TempDir`.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use data_encoding::BASE64;
use serde_json::{json, Value};

use super::crypto::{derive_subkey, hex, hmac_sha256, Purpose, SecretKey};
use super::memo_bridge::{memo_ops, mirror, resolve_memo_id, MemoSink, StoreMemoSink};
use super::merge::{EntityState, Projection};
use super::migration::{
    apply_queued, conv_entity_id, finalize, finalize_crashing, flushed_qseq, import, import_crashing, memo_entity_id,
    memo_restore_id, msg_entity_id, parse_webstore, plan_import, push_webstore_chunk, push_webstore_chunk_crashing,
    tr_entity_id, verify, verify_crashing, Applied, Crash, Finalize, Manifest, MigrationDir, MigrationError,
    MigrationState, QueuedOp, Verify, WebStore, WsConv, WsImage, WsMsg, WsTr,
};
use super::ops::{Hlc, HlcClock, Kind};
use super::sim_tests::{all, device, identity, local, op, publish, sync_stream, Remote, TempDir, EPOCH, VAULT};
use super::store::{now_ms as store_now, Store};
use crate::clipboard::{
    apply_memo_projection, create_memo_in, delete_entry_in, load_from, make_preview, rebound, save_to, set_note_in,
    splice_memos, toggle_pin_in, update_memo_in, wipe_entries, write_atomic, ClipEntry, ClipboardHandle, EntryKind,
    LoadOutcome,
};

const SNAP: &str = "snap-1";
const PNG: &[u8] = b"\x89PNG red team bytes";
const ALL_CRASHES: [Crash; 6] = [
    Crash::AfterBackupFiles,
    Crash::AfterImportingSet,
    Crash::InImportTx,
    Crash::AfterImported,
    Crash::AfterVerified,
    Crash::InFinalizeTx,
];

// ---------------------------------------------------------------------------
// Fixtures

fn key() -> SecretKey {
    derive_subkey(&SecretKey::from_bytes([1; 32]), VAULT, EPOCH, Purpose::BlobName)
}

fn ce(id: &str, text: &str, pinned: bool, note: Option<&str>, kind: EntryKind) -> ClipEntry {
    ClipEntry {
        id: id.to_owned(),
        text: text.to_owned(),
        preview: make_preview(text),
        ts: 100,
        pinned,
        note: note.map(str::to_owned),
        kind,
        conflict_of: None,
    }
}

fn memo(id: &str, text: &str) -> ClipEntry {
    ce(id, text, false, None, EntryKind::Memo)
}

fn clip(id: &str, text: &str) -> ClipEntry {
    ce(id, text, false, None, EntryKind::Clip)
}

fn msg(id: &str, role: &str, content: &str, ts: u64) -> WsMsg {
    WsMsg { id: id.to_owned(), role: role.to_owned(), content: content.to_owned(), ts, ..Default::default() }
}

fn img(id: &str, data: &str) -> WsImage {
    WsImage {
        id: id.to_owned(),
        mime_type: "image/png".to_owned(),
        data: data.to_owned(),
        width: 4,
        height: 3,
        name: None,
    }
}

fn tr(id: &str, source: &str, pinned: bool) -> WsTr {
    WsTr {
        id: id.to_owned(),
        ts: 7,
        source: source.to_owned(),
        target: format!("{source}-t"),
        from: "ko".to_owned(),
        to: "en".to_owned(),
        model: "m".to_owned(),
        pinned,
    }
}

fn conv_ws(id: &str, title: &str, messages: Vec<WsMsg>) -> WsConv {
    WsConv { id: id.to_owned(), title: title.to_owned(), messages }
}

/// 3 conversations (one empty), 4 messages (the id `m1` in two
/// conversations), 3 images (valid, evicted, invalid), 2 translations.
fn source() -> WebStore {
    let mut m1 = msg("m1", "user", "hi 안녕", 1);
    m1.images = vec![img("i1", &BASE64.encode(PNG)), img("i2", ""), img("i3", "%%%not base64")];
    let mut m2 = msg("m2", "assistant", "hello", 2);
    m2.model = Some("gpt".to_owned());
    WebStore {
        conversations: vec![
            conv_ws("c1", "first", vec![m1, m2]),
            conv_ws("c2", "second", vec![msg("m1", "user", "same id, other conv", 3), msg("n1", "user", "yo", 4)]),
            conv_ws("c3", "empty", vec![]),
        ],
        history: vec![tr("t1", "a", false), tr("t2", "b", true)],
    }
}

fn clip_entries() -> Vec<ClipEntry> {
    vec![memo("a", "memo a"), clip("k", "a clip"), ce("b", "memo b", true, Some("note b"), EntryKind::Memo)]
}

fn bytes_of(ws: &WebStore) -> Vec<u8> {
    serde_json::to_vec(ws).unwrap()
}

struct Env {
    dir: MigrationDir,
    db: PathBuf,
    store: Store,
    clip_src: PathBuf,
    clip_original: Vec<u8>,
    _tmp: TempDir,
    /// The device clock (G7 owns it in production).
    clock: HlcClock,
}

fn env(tag: &str, entries: &[ClipEntry]) -> Env {
    let tmp = TempDir::new(tag);
    let clip_src = tmp.0.join("clipboard.json");
    save_to(&clip_src, entries, 500, false).unwrap();
    let dir = MigrationDir::new(tmp.0.join("sync-migration"), clip_src.clone());
    let db = tmp.0.join("sync").join("sync.db");
    let mut store = Store::open(&db).unwrap();
    store.init_identity(&identity("a")).unwrap();
    store.set_active_vault(VAULT).unwrap();
    let clip_original = fs::read(&clip_src).unwrap();
    Env { dir, db, store, clip_src, clip_original, _tmp: tmp, clock: HlcClock::default() }
}

impl Env {
    /// A process restart: the store is closed and reopened from disk.
    fn reopen(&mut self) {
        self.store = Store::open_in_memory().unwrap();
        self.store = Store::open(&self.db).unwrap();
    }

    fn state(&self) -> MigrationState {
        MigrationState::load(&self.store).unwrap()
    }

    fn snap(&self) -> String {
        serde_json::from_slice::<Manifest>(&fs::read(self.dir.manifest()).unwrap()).unwrap().snapshot_hash
    }
}

fn push_chunks(e: &mut Env, bytes: &[u8], n: usize, crash: Option<Crash>) -> Result<MigrationState, MigrationError> {
    let size = bytes.len().div_ceil(n).max(1);
    let chunks: Vec<&[u8]> = bytes.chunks(size).collect();
    let total = chunks.len() as u32;
    let mut last = MigrationState::None;
    for (i, chunk) in chunks.iter().enumerate() {
        last = push_webstore_chunk_crashing(&mut e.store, &e.dir, VAULT, i as u32, total, chunk, SNAP, crash)?;
    }
    Ok(last)
}

fn push(e: &mut Env, index: u32, total: u32, bytes: &[u8]) -> Result<MigrationState, MigrationError> {
    push_webstore_chunk(&mut e.store, &e.dir, VAULT, index, total, bytes, SNAP)
}

fn to_verified(e: &mut Env, bytes: &[u8]) {
    assert_eq!(push_chunks(e, bytes, 3, None).unwrap(), MigrationState::BackedUp);
    import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1_000).unwrap();
    assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
    assert_eq!(e.state(), MigrationState::Verified);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Push,
    Import,
    Verify,
    Finalize,
}

fn step_of(c: Crash) -> Step {
    match c {
        Crash::AfterBackupFiles => Step::Push,
        Crash::AfterImportingSet | Crash::InImportTx | Crash::AfterImported => Step::Import,
        Crash::AfterVerified => Step::Verify,
        Crash::InFinalizeTx => Step::Finalize,
    }
}

fn pick(plan: &[Crash], step: Step) -> Option<Crash> {
    plan.first().copied().filter(|c| step_of(*c) == step)
}

fn backup_bytes(e: &Env) -> Vec<Option<Vec<u8>>> {
    [e.dir.clipboard_backup(), e.dir.webstore(), e.dir.manifest()].iter().map(|p| fs::read(p).ok()).collect()
}

/// Out-of-order calls for state `st`: each must be refused or ignored and
/// change neither the state nor the op log.
fn probe_wrong_calls(e: &mut Env, st: MigrationState) {
    use MigrationState as S;
    let ops = op_count(&e.store);
    let k = key();
    let wrong = |r: Result<(), MigrationError>| matches!(r, Err(MigrationError::WrongState(s)) if s == st);
    if st != S::Native {
        let q = QueuedOp::TrDelete { tr: "t1".to_owned() };
        assert!(wrong(apply_queued(&mut e.store, VAULT, &k, &mut e.clock, 1, &q).map(|_| ())), "apply_queued in {st:?}");
    }
    if !matches!(st, S::BackedUp | S::Importing | S::Imported) {
        assert!(wrong(import(&mut e.store, &e.dir, VAULT, &k, &mut e.clock, 1_000).map(|_| ())), "import in {st:?}");
    }
    if !matches!(st, S::Imported | S::Verified) {
        assert!(wrong(verify(&mut e.store, &e.dir, VAULT).map(|_| ())), "verify in {st:?}");
    }
    if !matches!(st, S::Verified | S::Native) {
        let r = finalize(&mut e.store, &e.dir, VAULT, &k, &mut e.clock, SNAP, None, Some(&[]), 1).map(|_| ());
        assert!(wrong(r), "finalize in {st:?}");
    }
    if st == S::Native {
        // A later finalize returns the stored ack, never the new tail.
        assert_eq!(
            finalize(&mut e.store, &e.dir, VAULT, &k, &mut e.clock, "other", None, Some(&[]), 99).unwrap(),
            Finalize::Native { ack_qseq: 9 }
        );
    }
    if st != S::None {
        let before = backup_bytes(e);
        assert_eq!(push(e, 0, 1, b"{\"conversations\":[]}").unwrap(), st, "push in {st:?}");
        assert_eq!(backup_bytes(e), before, "push in {st:?} rewrote the backup");
    }
    assert_eq!(e.state(), st);
    assert_eq!(op_count(&e.store), ops, "out-of-order call wrote ops in {st:?}");
}

/// Runs the boot sequence to `native`, injecting the crashes of `plan` in
/// order (a crash is consumed by the first call of its step). Returns the
/// number of crashes hit.
fn drive(e: &mut Env, bytes: &[u8], plan: &mut Vec<Crash>, probe: bool) -> usize {
    let k = key();
    let live = clip_entries();
    let mut hits = 0;
    for _ in 0..60 {
        let st = e.state();
        if probe {
            probe_wrong_calls(e, st);
        }
        let res: Result<(), MigrationError> = match st {
            MigrationState::None => {
                let c = pick(plan, Step::Push);
                match push_chunks(e, bytes, 3, c) {
                    // A complete stale progress rejects a restart once.
                    Err(MigrationError::Chunk(_)) => push_chunks(e, bytes, 3, c).map(|_| ()),
                    other => other.map(|_| ()),
                }
            }
            MigrationState::BackedUp | MigrationState::Importing => {
                import_crashing(&mut e.store, &e.dir, VAULT, &k, &mut e.clock, 1_000, pick(plan, Step::Import)).map(|_| ())
            }
            MigrationState::Imported => match verify_crashing(&mut e.store, &e.dir, VAULT, pick(plan, Step::Verify)) {
                Ok(Verify::Passed(_)) => Ok(()),
                Ok(Verify::Failed(bad)) => panic!("verify failed: {bad:?}"),
                Err(err) => Err(err),
            },
            MigrationState::Verified => {
                let snap = e.snap();
                let c = pick(plan, Step::Finalize);
                finalize_crashing(&mut e.store, &e.dir, VAULT, &k, &mut e.clock, &snap, None, Some(&live), 9, c).map(|out| {
                    assert_eq!(out, Finalize::Native { ack_qseq: 9 });
                })
            }
            MigrationState::Native => return hits,
        };
        match res {
            Ok(()) => {}
            Err(MigrationError::Crashed(at)) => {
                assert_eq!(plan.first().copied(), Some(at));
                plan.remove(0);
                hits += 1;
                e.reopen();
            }
            Err(other) => panic!("unexpected error in {st:?}: {other}"),
        }
    }
    panic!("migration did not converge");
}

/// (live conversations, messages, live translations, live memos).
fn counts(p: &Projection) -> (usize, usize, usize, usize) {
    let (mut c, mut m, mut t, mut n) = (0, 0, 0, 0);
    for state in p.entities.values() {
        match state {
            EntityState::Conv(conv) => {
                c += 1;
                m += conv.messages.len();
            }
            EntityState::Tr(_) => t += 1,
            EntityState::Memo(_) => n += 1,
            _ => {}
        }
    }
    (c, m, t, n)
}

/// (kind, entity, type, msg_id) of every stored op.
fn op_sigs(store: &Store) -> Vec<(Kind, String, String, String)> {
    let (set, _) = store.load_set(VAULT).unwrap();
    let mut sigs: Vec<_> = set
        .iter()
        .map(|e| {
            let sub = e.body.payload.get("msg_id").and_then(Value::as_str).unwrap_or_default().to_owned();
            (e.body.kind, e.body.entity_id.clone(), e.body.op_type.clone(), sub)
        })
        .collect();
    sigs.sort();
    sigs
}

fn op_count(store: &Store) -> usize {
    op_sigs(store).len()
}

fn count_type(store: &Store, kind: Kind, entity: &str, op_type: &str) -> usize {
    op_sigs(store).iter().filter(|s| s.0 == kind && s.1 == entity && s.2 == op_type).count()
}

fn assert_no_duplicates(store: &Store) {
    let sigs: Vec<_> = op_sigs(store)
        .into_iter()
        .filter(|(_, _, t, _)| matches!(t.as_str(), "create" | "msg.add" | "tr.add"))
        .collect();
    let unique: BTreeSet<_> = sigs.iter().cloned().collect();
    assert_eq!(sigs.len(), unique.len(), "duplicate creates: {sigs:?}");
}

fn deletes(store: &Store) -> Vec<(Kind, String)> {
    op_sigs(store).into_iter().filter(|s| s.2.ends_with("delete")).map(|s| (s.0, s.1)).collect()
}

fn conv<'a>(p: &'a Projection, id: &str) -> &'a super::merge::ConvState {
    match p.entities.get(&(Kind::Conv, conv_entity_id(id))) {
        Some(EntityState::Conv(c)) => c,
        other => panic!("conversation {id}: {other:?}"),
    }
}

fn count_rows(store: &Store, sql: &str) -> i64 {
    store.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The converged result of the default fixture.
fn assert_converged(e: &Env, bytes: &[u8]) {
    assert_eq!(e.state(), MigrationState::Native);
    let p = e.store.projection(VAULT).unwrap();
    assert_eq!(counts(&p), (3, 4, 2, 2));
    assert_no_duplicates(&e.store);
    assert!(deletes(&e.store).is_empty(), "{:?}", deletes(&e.store));
    // M9: originals byte-identical, backups kept.
    assert_eq!(fs::read(&e.clip_src).unwrap(), e.clip_original);
    assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), e.clip_original);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
    assert!(e.dir.manifest().exists());
    assert_eq!(count_rows(&e.store, "SELECT count(*) FROM blobs"), 1);
    let c1 = conv(&p, "c1");
    assert_eq!(c1.messages[&msg_entity_id("c1", "m1")].attachments.len(), 3);
    assert_eq!(c1.active_leaf.as_deref(), Some(msg_entity_id("c1", "m2").as_str()));
    assert_eq!(conv(&p, "c2").messages[&msg_entity_id("c2", "m1")].content, "same id, other conv");
    assert!(conv(&p, "c3").messages.is_empty());
}

/// A queue without two updates of the same register.
fn queue() -> Vec<QueuedOp> {
    vec![
        QueuedOp::ConvCreate { conv: "c9".to_owned(), title: "q".to_owned() },
        QueuedOp::MsgAdd { conv: "c9".to_owned(), msg: msg("q1", "user", "one", 10), parent: None },
        QueuedOp::MsgAdd { conv: "c9".to_owned(), msg: msg("q2", "assistant", "two", 11), parent: Some("q1".to_owned()) },
        QueuedOp::Rename { conv: "c9".to_owned(), title: "q renamed".to_owned() },
        QueuedOp::SetActive { conv: "c9".to_owned(), leaf: "q1".to_owned() },
        QueuedOp::TrAdd { tr: tr("t9", "z", false) },
        QueuedOp::TrPin { tr: "t9".to_owned(), pinned: true },
        QueuedOp::TrDelete { tr: "t1".to_owned() },
        QueuedOp::TrClear { observed_ids: vec!["t2".to_owned(), "t1".to_owned(), "t2".to_owned(), "nope".to_owned()] },
        QueuedOp::ConvDelete { conv: "c2".to_owned() },
        QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m4", "user", "late", 12), parent: Some("m2".to_owned()) },
    ]
}

/// Applies `op` as queue item `qseq`.
fn apply_at(store: &mut Store, qseq: u64, op: &QueuedOp) -> Applied {
    apply_queued(store, VAULT, &key(), &mut HlcClock::default(), qseq, op).unwrap()
}

/// The next unused queue sequence number.
fn next_qseq(store: &Store) -> u64 {
    flushed_qseq(store).unwrap().map_or(1, |q| q + 1)
}

/// Applies `op` as a new queue item (the next qseq).
fn apply_next(store: &mut Store, op: &QueuedOp) -> Applied {
    let qseq = next_qseq(store);
    apply_at(store, qseq, op)
}

/// Flushes `q` as queue items `start..`.
fn flush_from(store: &mut Store, start: u64, q: &[QueuedOp]) -> Vec<bool> {
    q.iter().zip(start..).map(|(op, qseq)| apply_at(store, qseq, op).changed).collect()
}

/// Flushes `q` as new queue items, so only the id-based no-op rule applies.
fn flush(store: &mut Store, q: &[QueuedOp]) -> Vec<bool> {
    q.iter().map(|op| apply_next(store, op).changed).collect()
}

// ---------------------------------------------------------------------------
// AC 9 / 28: crashes across push → import → verify → finalize → flush

#[test]
fn rt01_single_crash_at_every_point_with_out_of_order_probes_reaches_native_once() {
    let bytes = bytes_of(&source());
    for c in ALL_CRASHES {
        let mut e = env("rt01", &clip_entries());
        let mut plan = vec![c];
        assert_eq!(drive(&mut e, &bytes, &mut plan, true), 1, "{c:?}");
        assert_converged(&e, &bytes);
        probe_wrong_calls(&mut e, MigrationState::Native);
    }
}

#[test]
fn rt02_double_crash_every_ordered_pair_converges_without_loss_or_duplicates() {
    let bytes = bytes_of(&source());
    for a in ALL_CRASHES {
        for b in ALL_CRASHES {
            let mut e = env("rt02", &clip_entries());
            let mut plan = vec![a, b];
            let hits = drive(&mut e, &bytes, &mut plan, false);
            assert!(hits >= 1, "{a:?},{b:?}");
            assert_converged(&e, &bytes);
        }
    }
}

#[test]
fn rt03_every_crash_point_in_one_run_then_flush_crash_and_reflush_exactly_once() {
    let bytes = bytes_of(&source());
    let mut e = env("rt03", &clip_entries());
    let mut plan = ALL_CRASHES.to_vec();
    plan.insert(3, Crash::InImportTx); // the same point twice
    plan.push(Crash::InFinalizeTx);
    assert_eq!(drive(&mut e, &bytes, &mut plan, true), 8);
    assert!(plan.is_empty());
    assert_converged(&e, &bytes);
    // No finalize attempt was counted for a crash.
    assert_eq!(e.store.meta_get("migration_finalize_attempts").unwrap(), None);

    // Queue items after the M7 ack (9) carry qseq 10.. and keep them across
    // the crash, as G8 stores them.
    let q = queue();
    for cut in [0, 1, 3, 6, q.len()] {
        let mut e2 = env("rt03-flush", &clip_entries());
        drive(&mut e2, &bytes, &mut Vec::new(), false);
        flush_from(&mut e2.store, 10, &q[..cut]);
        e2.reopen(); // crash mid-flush
        flush_from(&mut e2.store, 10, &q);
        assert_no_duplicates(&e2.store);
        let ops = op_count(&e2.store);
        assert!(flush_from(&mut e2.store, 10, &q).iter().all(|c| !c), "cut {cut}: re-flush changed something");
        assert_eq!(op_count(&e2.store), ops);
        let p = e2.store.projection(VAULT).unwrap();
        let c9 = conv(&p, "c9");
        assert_eq!((c9.title.as_str(), c9.messages.len()), ("q renamed", 2));
        assert_eq!(count_type(&e2.store, Kind::Conv, &conv_entity_id("c1"), "msg.add"), 3);
        assert_eq!(p.entities.get(&(Kind::Conv, conv_entity_id("c2"))), Some(&EntityState::Deleted));
    }
}

#[test]
fn rt04_crash_before_backed_up_keeps_none_and_a_changed_snapshot_replaces_the_backup() {
    let bytes = bytes_of(&source());
    let mut e = env("rt04", &clip_entries());
    assert!(matches!(push_chunks(&mut e, &bytes, 3, Some(Crash::AfterBackupFiles)), Err(MigrationError::Crashed(_))));
    e.reopen();
    assert_eq!(e.state(), MigrationState::None);
    // The webview now holds a different snapshot (new hash): the stale
    // progress is rejected once, then the new snapshot becomes the backup.
    let mut ws2 = source();
    ws2.conversations[0].messages.push(msg("m3", "user", "after crash", 5));
    let bytes2 = bytes_of(&ws2);
    let p = |e: &mut Env, i: u32, t: u32, b: &[u8]| push_webstore_chunk(&mut e.store, &e.dir, VAULT, i, t, b, "snap-2");
    assert!(matches!(p(&mut e, 0, 1, &bytes2), Err(MigrationError::Chunk(_))));
    assert_eq!(p(&mut e, 0, 1, &bytes2).unwrap(), MigrationState::BackedUp);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes2);
    assert_eq!(e.snap(), "snap-2");
}

#[test]
fn rt05_corrupted_backup_after_backed_up_fails_closed_without_writing_ops() {
    let bytes = bytes_of(&source());
    let mut e = env("rt05", &clip_entries());
    assert_eq!(push_chunks(&mut e, &bytes, 2, None).unwrap(), MigrationState::BackedUp);
    let mut tampered = bytes.clone();
    tampered[10] ^= 1;
    fs::write(e.dir.webstore(), &tampered).unwrap();
    assert!(matches!(import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1), Err(MigrationError::Corrupt(_))));
    assert_eq!(op_count(&e.store), 0);
    assert_eq!(fs::read(&e.clip_src).unwrap(), e.clip_original);
    // A later push cannot overwrite the backup either.
    assert_eq!(push(&mut e, 0, 1, &bytes).unwrap(), MigrationState::Importing);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), tampered);
}

/// History: this case used to back the corrupt file up verbatim and let
/// import fail closed; it now follows plan-amendments "The clipboard must be
/// readable before a push can complete" (the push fails, state stays `none`).
#[test]
fn rt06_unparseable_clipboard_blocks_the_push_and_leaves_originals_untouched() {
    // A clipboard.json that could not be moved aside (Unreadable outcome)
    // is still corrupt when the push runs.
    let mut e = env("rt06", &[]);
    fs::write(&e.clip_src, b"{not json").unwrap();
    let bytes = bytes_of(&source());
    for _ in 0..2 {
        let r = push_chunks(&mut e, &bytes, 1, None);
        assert!(matches!(r, Err(MigrationError::Corrupt(_))), "{r:?}");
        assert_eq!(e.state(), MigrationState::None);
        assert_eq!(op_count(&e.store), 0);
        assert_eq!(fs::read(&e.clip_src).unwrap(), b"{not json");
        assert!(!e.dir.clipboard_backup().exists() && !e.dir.webstore().exists() && !e.dir.manifest().exists());
    }
    // Nothing can import the corrupt bytes: there is no backup to read.
    assert!(matches!(
        import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1),
        Err(MigrationError::WrongState(MigrationState::None))
    ));
    // The chunks were kept, so once the file parses the re-sent chunk
    // completes the push and backs up the repaired bytes.
    save_to(&e.clip_src, &clip_entries(), 500, false).unwrap();
    let repaired = fs::read(&e.clip_src).unwrap();
    assert_eq!(push(&mut e, 0, 1, &bytes).unwrap(), MigrationState::BackedUp);
    assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), repaired);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
    assert_eq!(fs::read(&e.clip_src).unwrap(), repaired);
}

#[test]
fn rt07_migration_is_gated_on_the_vault_identity() {
    let mut e = env("rt07", &clip_entries());
    let bytes = bytes_of(&source());
    let r = push_webstore_chunk(&mut e.store, &e.dir, "vault-unknown", 0, 1, &bytes, SNAP);
    assert!(matches!(r, Err(MigrationError::Store(_))), "{r:?}");
    assert!(!e.dir.root.exists());
    assert_eq!(e.state(), MigrationState::None);
}

// ---------------------------------------------------------------------------
// Chunk protocol abuse (M2)

#[test]
fn rt10_duplicate_previous_chunk_is_a_noop_other_duplicates_and_gaps_reset() {
    let bytes = bytes_of(&source());
    let third = bytes.len().div_ceil(3);
    let c: Vec<&[u8]> = bytes.chunks(third).collect();
    let mut e = env("rt10", &clip_entries());
    assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None);
    assert_eq!(push(&mut e, 0, 3, c[0]).unwrap(), MigrationState::None); // identical re-send
    assert_eq!(push(&mut e, 1, 3, c[1]).unwrap(), MigrationState::None);
    assert_eq!(push(&mut e, 1, 3, c[1]).unwrap(), MigrationState::None);
    // An older chunk (i-2) is out of order: reset.
    assert!(matches!(push(&mut e, 0, 3, c[0]), Err(MigrationError::Chunk(_))));
    assert!(!e.dir.webstore().exists());
    // Gap: 0 then 2.
    push(&mut e, 0, 3, c[0]).unwrap();
    assert!(matches!(push(&mut e, 2, 3, c[2]), Err(MigrationError::Chunk(_))));
    // Previous index with other bytes.
    push(&mut e, 0, 3, c[0]).unwrap();
    assert!(matches!(push(&mut e, 0, 3, b"{\"x\":1}"), Err(MigrationError::Chunk(_))));
    // A clean restart completes with exactly the source bytes.
    for (i, chunk) in c.iter().enumerate() {
        let st = push(&mut e, i as u32, 3, chunk).unwrap();
        assert_eq!(st, if i == 2 { MigrationState::BackedUp } else { MigrationState::None });
    }
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
}

#[test]
fn rt11_huge_index_zero_total_and_total_or_hash_change_mid_stream_reset() {
    let bytes = bytes_of(&source());
    let mut e = env("rt11", &clip_entries());
    for (i, t) in [(u32::MAX - 1, u32::MAX), (u32::MAX, u32::MAX), (0, 0), (5, 3), (u32::MAX, 1)] {
        assert!(matches!(push(&mut e, i, t, b"{}"), Err(MigrationError::Chunk(_))), "{i}/{t}");
    }
    // A huge total is accepted for chunk 0, then a total change resets.
    assert_eq!(push(&mut e, 0, u32::MAX, b"{").unwrap(), MigrationState::None);
    assert!(matches!(push(&mut e, 1, 3, b"}"), Err(MigrationError::Chunk(_))));
    // Hash change mid-stream resets.
    push(&mut e, 0, 2, b"{").unwrap();
    let r = push_webstore_chunk(&mut e.store, &e.dir, VAULT, 1, 2, b"}", "other-hash");
    assert!(matches!(r, Err(MigrationError::Chunk(_))));
    assert_eq!(e.state(), MigrationState::None);
    assert!(!e.dir.webstore().exists() && !e.dir.manifest().exists() && !e.dir.clipboard_backup().exists());
    // Restart succeeds.
    assert_eq!(push(&mut e, 0, 1, &bytes).unwrap(), MigrationState::BackedUp);
}

#[test]
fn rt12_empty_chunks_are_harmless_and_an_all_empty_snapshot_is_rejected() {
    let bytes = bytes_of(&source());
    let half = bytes.len() / 2;
    let parts: [&[u8]; 5] = [b"", &bytes[..half], b"", &bytes[half..], b""];
    let mut e = env("rt12", &clip_entries());
    for (i, p) in parts.iter().enumerate() {
        push(&mut e, i as u32, 5, p).unwrap();
        if i == 2 {
            // Re-sending the empty previous chunk is a no-op.
            push(&mut e, 2, 5, b"").unwrap();
        }
    }
    assert_eq!(e.state(), MigrationState::BackedUp);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);

    let mut e = env("rt12-empty", &clip_entries());
    push(&mut e, 0, 2, b"").unwrap();
    assert!(matches!(push(&mut e, 1, 2, b""), Err(MigrationError::Chunk(_))));
    assert_eq!(e.state(), MigrationState::None);
    assert!(!e.dir.clipboard_backup().exists());
}

#[test]
fn rt13_torn_append_and_truncated_partial_and_split_utf8() {
    let bytes = bytes_of(&source());
    // Split inside the multi-byte "안" of m1's content.
    let cut = bytes.windows(3).position(|w| w == "안".as_bytes()).unwrap() + 1;
    let (a, b) = bytes.split_at(cut);
    let mut e = env("rt13", &clip_entries());
    push(&mut e, 0, 2, a).unwrap();
    // A crash after the append but before the progress write: junk tail.
    let partial = e.dir.root.join("webstore.json.partial");
    let mut raw = fs::read(&partial).unwrap();
    raw.extend_from_slice(b"GARBAGE");
    fs::write(&partial, &raw).unwrap();
    assert_eq!(push(&mut e, 1, 2, b).unwrap(), MigrationState::BackedUp);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);

    // A partial truncated behind the recorded length resets.
    let mut e = env("rt13-trunc", &clip_entries());
    push(&mut e, 0, 2, a).unwrap();
    fs::write(e.dir.root.join("webstore.json.partial"), b"").unwrap();
    assert!(matches!(push(&mut e, 1, 2, b), Err(MigrationError::Chunk(_))));
    assert_eq!(push(&mut e, 0, 2, a).unwrap(), MigrationState::None);
    assert_eq!(push(&mut e, 1, 2, b).unwrap(), MigrationState::BackedUp);
}

#[test]
fn rt14_damaged_progress_file_fails_closed_without_touching_originals() {
    let bytes = bytes_of(&source());
    let mut e = env("rt14", &clip_entries());
    push(&mut e, 0, 2, &bytes[..5]).unwrap();
    fs::write(e.dir.root.join("webstore.json.partial.progress"), b"\x00\x01").unwrap();
    assert!(matches!(push(&mut e, 1, 2, &bytes[5..]), Err(MigrationError::Corrupt(_))));
    assert_eq!(e.state(), MigrationState::None);
    assert!(!e.dir.webstore().exists());
    assert_eq!(fs::read(&e.clip_src).unwrap(), e.clip_original);
}

// ---------------------------------------------------------------------------
// Malformed webstore

/// History: the invalid UTF-8 title case used to be rejected; it now follows
/// plan-amendments "Snapshots are parsed leniently" (lossy UTF-8) and is
/// asserted to migrate with U+FFFD below.
#[test]
fn rt20_invalid_snapshots_reset_and_write_no_backup() {
    let cases: [&[u8]; 6] = [
        b"{not json",
        b"\xff\xfe\x00garbage",
        b"\"just a string\"",
        b"{\"conversations\":{}}",
        b"{\"conversations\":[{\"title\":\"no id\"}]}",
        b"null",
    ];
    for raw in cases {
        let mut e = env("rt20", &clip_entries());
        assert!(matches!(push(&mut e, 0, 1, raw), Err(MigrationError::Chunk(_))), "{raw:?}");
        assert_eq!(e.state(), MigrationState::None);
        assert!(!e.dir.webstore().exists() && !e.dir.manifest().exists() && !e.dir.clipboard_backup().exists());
        assert_eq!(fs::read(e.dir.root.join("webstore.json.partial")).unwrap(), b"");
        // A valid snapshot still completes afterwards.
        assert_eq!(push(&mut e, 0, 1, &bytes_of(&source())).unwrap(), MigrationState::BackedUp);
    }
    // Invalid UTF-8 inside a string is decoded lossily: the backup keeps the
    // original bytes and the title imports as U+FFFD.
    let raw: &[u8] = b"{\"conversations\":[{\"id\":\"c\",\"title\":\"\xc3\x28\"}]}";
    let mut e = env("rt20-lossy", &clip_entries());
    assert_eq!(push(&mut e, 0, 1, raw).unwrap(), MigrationState::BackedUp);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), raw);
    drive(&mut e, raw, &mut Vec::new(), false);
    assert_eq!(conv(&e.store.projection(VAULT).unwrap(), "c").title, "\u{FFFD}(");
}

#[test]
fn rt21_unknown_fields_missing_optionals_and_lenient_numbers_migrate_and_verify() {
    let raw = br#"{"conversations":[
        {"id":"c","extra":{"deep":[1,2]},"createdAt":1,
         "messages":[
            {"id":"m","role":"user","content":"x","ts":"not a number","images":[{"data":"","futureField":1}]},
            {"id":"m","role":"user","content":"dup id ignored","ts":2},
            {"id":"n","content":"no role","ts":-5,"model":null},
            {"id":"o","role":"assistant","content":"big","ts":1e300},
            {"id":"p","role":"assistant","content":"null ts","ts":null}
         ]},
        {"id":"c","title":"duplicate conv ignored","messages":[{"id":"zz","role":"user","content":"lost?","ts":1}]},
        {"id":"","title":"empty id","messages":[]}
      ],
      "history":[{"id":"h"},{"id":"h","source":"dup"}],
      "settings":{"x":true}}"#;
    let mut e = env("rt21", &clip_entries());
    assert_eq!(push(&mut e, 0, 1, raw).unwrap(), MigrationState::BackedUp);
    import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1).unwrap();
    assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
    let p = e.store.projection(VAULT).unwrap();
    let c = conv(&p, "c");
    assert_eq!(c.title, "");
    assert_eq!(c.messages.len(), 4);
    let m = |id: &str| &c.messages[&msg_entity_id("c", id)];
    assert_eq!((m("m").content.as_str(), m("m").ts, m("m").attachments.len()), ("x", 0, 1));
    assert_eq!((m("n").role.as_str(), m("n").ts, m("n").model.clone()), ("unknown", 0, None));
    assert_eq!(m("o").ts, i64::MAX as u64);
    assert_eq!(m("p").ts, 0);
    // First occurrence wins; the duplicate conversation's message is not
    // imported (it stays in the backup file).
    assert!(!c.messages.contains_key(&msg_entity_id("c", "zz")));
    assert!(conv(&p, "").messages.is_empty());
    assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("h"))), Some(EntityState::Tr(t)) if t.data["source"] == ""));
    assert_no_duplicates(&e.store);
}

#[test]
fn rt22_lone_surrogate_escape_in_a_title_does_not_wedge_the_migration() {
    // chat-history.ts genTitle: first.slice(0, 32) can split a surrogate
    // pair; JSON.stringify (ES2019) then emits the lone high half as an
    // escape (backslash, 'u', "d83d").
    let lone = format!("{}u{}", '\\', "d83d");
    let title = format!("{}{lone}…", "가".repeat(31));
    let raw = format!(
        r#"{{"conversations":[{{"id":"c","title":"{title}","messages":[{{"id":"m","role":"user","content":"ok","ts":1}}]}}],"history":[]}}"#
    );
    let mut e = env("rt22", &clip_entries());
    let first = push(&mut e, 0, 1, raw.as_bytes());
    assert!(first.is_ok(), "snapshot rejected: {first:?}");
    assert_eq!(first.unwrap(), MigrationState::BackedUp);
    drive(&mut e, raw.as_bytes(), &mut Vec::new(), false);
    let p = e.store.projection(VAULT).unwrap();
    assert_eq!(conv(&p, "c").messages.len(), 1);
}

/// Observation (not a blocker): serde reads a struct from a JSON array
/// positionally, so a top-level `[]` (or `[[...], [...]]`) is accepted as a
/// snapshot. The webview always sends an object; the safety property that
/// matters holds: nothing is deleted and the backup is the bytes received.
#[test]
fn rt24_top_level_array_snapshot_is_read_positionally_and_never_deletes() {
    let ws: WebStore = serde_json::from_slice(b"[]").unwrap();
    assert_eq!(ws, WebStore::default());
    let positional = br#"[[{"id":"c","title":"t","messages":[]}],[]]"#;
    let ws: WebStore = serde_json::from_slice(positional).unwrap();
    assert_eq!(ws.conversations.len(), 1);
    let mut e = env("rt24", &clip_entries());
    drive(&mut e, b"[]", &mut Vec::new(), false);
    assert!(deletes(&e.store).is_empty());
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), b"[]");
    assert_eq!(counts(&e.store.projection(VAULT).unwrap()), (0, 0, 0, 2));
}

#[test]
fn rt23_same_message_id_in_two_conversations_and_empty_conversations() {
    let p0 = device("x").projection(VAULT).unwrap();
    let plan = plan_import(&p0, &source(), &[], &BTreeSet::new(), &key());
    let adds: BTreeSet<&str> =
        plan.ops.iter().filter(|o| o.op_type == "msg.add").map(|o| o.payload["msg_id"].as_str().unwrap()).collect();
    assert_eq!(adds.len(), 4);
    assert!(adds.contains(msg_entity_id("c1", "m1").as_str()) && adds.contains(msg_entity_id("c2", "m1").as_str()));
    let c3 = conv_entity_id("c3");
    let c3_ops: Vec<&str> = plan.ops.iter().filter(|o| o.entity_id == c3).map(|o| o.op_type.as_str()).collect();
    assert_eq!(c3_ops, vec!["create"]);
    // Linear parents inside a conversation.
    let m2 = plan.ops.iter().find(|o| o.payload["msg_id"] == msg_entity_id("c1", "m2")).unwrap();
    assert_eq!(m2.payload["parent_id"], msg_entity_id("c1", "m1"));
}

// ---------------------------------------------------------------------------
// M6 delta: removal never deletes

#[test]
fn rt30_m6_snapshot_that_removes_items_emits_no_delete() {
    let bytes = bytes_of(&source());
    let mut e = env("rt30", &clip_entries());
    to_verified(&mut e, &bytes);
    let need = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, "changed", None, Some(&clip_entries()), 4).unwrap();
    assert_eq!(need, Finalize::NeedSnapshot);
    // Current: c2, c3, t1, m2 removed; c1 gets a new message and an evicted
    // image for m1 (eviction is not deletion); t2 unpinned; a new conv c4.
    let mut cur = source();
    cur.conversations.retain(|c| c.id == "c1");
    cur.conversations[0].messages.truncate(1);
    cur.conversations[0].messages[0].images[0].data.clear();
    cur.conversations[0].messages.push(msg("m5", "user", "new during migration", 9));
    cur.conversations.push(conv_ws("c4", "fresh", vec![msg("x1", "user", "x", 10)]));
    cur.history = vec![tr("t2", "b", false)];
    let cur_bytes = bytes_of(&cur);
    let out = finalize(
        &mut e.store,
        &e.dir,
        VAULT,
        &key(),
        &mut e.clock,
        "changed",
        Some((&cur_bytes, "changed")),
        Some(&clip_entries()),
        4,
    )
    .unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 4 });
    assert!(deletes(&e.store).is_empty(), "{:?}", deletes(&e.store));
    let p = e.store.projection(VAULT).unwrap();
    assert_eq!(counts(&p), (4, 6, 2, 2));
    let c1 = conv(&p, "c1");
    assert!(c1.messages.contains_key(&msg_entity_id("c1", "m2")));
    assert_eq!(c1.messages[&msg_entity_id("c1", "m5")].parent_id, Some(msg_entity_id("c1", "m1")));
    assert!(c1.messages[&msg_entity_id("c1", "m1")].attachments[0].get("blob_id").is_some());
    assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t2"))), Some(EntityState::Tr(t)) if !t.pinned));
    assert_no_duplicates(&e.store);
    // Re-running finalize (lost ack) changes nothing and returns the ack.
    let ops = op_count(&e.store);
    let again = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, "x", Some((&cur_bytes, "x")), Some(&[]), 77).unwrap();
    assert_eq!(again, Finalize::Native { ack_qseq: 4 });
    assert_eq!(op_count(&e.store), ops);
}

#[test]
fn rt31_m6_empty_or_unparsable_snapshot() {
    let bytes = bytes_of(&source());
    let mut e = env("rt31", &clip_entries());
    to_verified(&mut e, &bytes);
    for bad in [&b"{oops"[..], b"\xff\xff", b"42"] {
        let r = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, "h", Some((bad, "h")), Some(&clip_entries()), 1);
        assert!(matches!(r, Err(MigrationError::Json(_))), "{r:?}");
        assert_eq!(e.state(), MigrationState::Verified);
    }
    assert_eq!(e.store.meta_get("migration_finalize_attempts").unwrap(), None);
    let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, "h", Some((b"{}", "h")), Some(&clip_entries()), 1).unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 1 });
    assert!(deletes(&e.store).is_empty());
    assert_eq!(counts(&e.store.projection(VAULT).unwrap()), (3, 4, 2, 2));
}

// ---------------------------------------------------------------------------
// Memo delta

const REMOTE: &str = "0190a000-0000-7000-8000-0000000000ee";

#[test]
fn rt40_remote_memo_never_deleted_own_import_edit_then_delete_is_final() {
    let bytes = bytes_of(&source());
    let mut e = env("rt40", &clip_entries());
    local(&mut e.store, super::sim_tests::create(900, 1_000, REMOTE, "remote memo"));
    to_verified(&mut e, &bytes);
    // Live: own-import "a" edited, "b" deleted locally; remote memo absent.
    let live = vec![memo("a", "memo a v2"), clip("k", "a clip")];
    let snap = e.snap();
    let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, &snap, None, Some(&live), 3).unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 3 });
    let p = e.store.projection(VAULT).unwrap();
    let st = |id: &str| p.entities.get(&(Kind::Memo, id.to_owned())).cloned();
    assert!(matches!(st(REMOTE), Some(EntityState::Memo(m)) if m.text == "remote memo"));
    assert!(matches!(st(&memo_entity_id("a")), Some(EntityState::Memo(m)) if m.text == "memo a v2"));
    assert_eq!(st(&memo_entity_id("b")), Some(EntityState::Deleted));
    assert_eq!(deletes(&e.store), vec![(Kind::Memo, memo_entity_id("b"))]);

    // After native the mirror rewrites ids; the sink edits then deletes "a".
    let mirrored = mirror(&live, &p, 5);
    let a = mirrored.iter().find(|m| m.id == memo_entity_id("a")).unwrap().clone();
    let dir = e.dir.clone();
    let store = Arc::new(Mutex::new(e.store));
    let sink = StoreMemoSink { store: store.clone(), vault: VAULT.to_owned(), clock: Arc::new(Mutex::new(HlcClock::default())) };
    let mut a2 = a.clone();
    a2.text = "memo a v3".to_owned();
    sink.changed(Some(&a), Some(&a2)).unwrap();
    sink.changed(Some(&a2), None).unwrap();
    drop(sink);
    let mut s = Arc::try_unwrap(store).ok().unwrap().into_inner().unwrap();
    let p = s.projection(VAULT).unwrap();
    assert_eq!(p.entities.get(&(Kind::Memo, memo_entity_id("a"))), Some(&EntityState::Deleted));
    // A stale live list (still holding the raw "a") never resurrects it.
    let own: BTreeSet<String> =
        serde_json::from_str(&s.meta_get("migration_import_memo_ids").unwrap().unwrap()).unwrap();
    let plan = plan_import(&p, &WebStore::default(), &[memo("a", "memo a v2")], &own, &key());
    assert!(plan.ops.is_empty(), "{:?}", plan.ops);
    let ops = op_count(&s);
    assert!(matches!(finalize(&mut s, &dir, VAULT, &key(), &mut HlcClock::default(), "x", None, Some(&live), 0).unwrap(), Finalize::Native { .. }));
    assert_eq!(op_count(&s), ops);
    // The remote memo was never deleted.
    assert!(!deletes(&s).iter().any(|d| d.1 == REMOTE));
}

#[test]
fn rt41_own_import_memo_deleted_remotely_is_not_resurrected_by_finalize() {
    let bytes = bytes_of(&source());
    let mut e = env("rt41", &clip_entries());
    assert_eq!(push_chunks(&mut e, &bytes, 1, None).unwrap(), MigrationState::BackedUp);
    import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1).unwrap();
    local(&mut e.store, op(901, 2_000, Kind::Memo, &memo_entity_id("b"), "delete", json!({})));
    assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(r) if r.skipped_deleted == 1));
    let snap = e.snap();
    let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, &snap, None, Some(&clip_entries()), 1).unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 1 });
    let p = e.store.projection(VAULT).unwrap();
    assert_eq!(p.entities.get(&(Kind::Memo, memo_entity_id("b"))), Some(&EntityState::Deleted));
    assert_eq!(count_type(&e.store, Kind::Memo, &memo_entity_id("b"), "create"), 1);
}

// ---------------------------------------------------------------------------
// AC 10: clipboard.json corrupt / unreadable; atomic write

#[test]
fn rt50_every_corrupt_variant_is_preserved_byte_for_byte_then_saves_resume() {
    let mut bom = b"\xef\xbb\xbf".to_vec();
    bom.extend_from_slice(br#"{"entries":[]}"#);
    let cases: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"null".to_vec(),
        b"[]".to_vec(),
        b"{".to_vec(),
        b"\xff\xfe\x00\x01".to_vec(),
        bom,
        br#"{"entries":5}"#.to_vec(),
        br#"{"entries":[{"id":"x"}]}"#.to_vec(),
        br#"{"max_entries":3}"#.to_vec(),
        br#"{"entries":[],"max_entries":-1}"#.to_vec(),
    ];
    for raw in cases {
        let tmp = TempDir::new("rt50");
        let path = tmp.0.join("clipboard.json");
        fs::write(&path, &raw).unwrap();
        let loaded = load_from(&path);
        let LoadOutcome::Corrupt { preserved } = loaded.outcome.clone() else {
            panic!("{raw:?}: {:?}", loaded.outcome)
        };
        assert!(!loaded.persist_blocked() && loaded.entries.is_empty());
        assert!(!path.exists());
        assert_eq!(fs::read(&preserved).unwrap(), raw);
        assert!(preserved.file_name().unwrap().to_string_lossy().starts_with("clipboard.json.corrupt-"));
        assert!(save_to(&path, &clip_entries(), 10, false).unwrap());
        assert_eq!(fs::read(&preserved).unwrap(), raw);
        assert_eq!(load_from(&path).entries, clip_entries());
    }
}

#[test]
fn rt51_directory_dangling_and_symlinked_clipboard_paths() {
    let tmp = TempDir::new("rt51");
    // A directory where the file should be: unreadable, saves blocked.
    let dir_path = tmp.0.join("clipboard.json");
    fs::create_dir(&dir_path).unwrap();
    fs::write(dir_path.join("inner"), b"keep").unwrap();
    let loaded = load_from(&dir_path);
    assert_eq!(loaded.outcome, LoadOutcome::Unreadable);
    assert!(loaded.persist_blocked());
    assert!(!save_to(&dir_path, &clip_entries(), 10, loaded.persist_blocked()).unwrap());
    assert_eq!(fs::read(dir_path.join("inner")).unwrap(), b"keep");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // Symlink to a corrupt file: the link is moved aside, target intact.
        let target = tmp.0.join("real.json");
        fs::write(&target, b"{corrupt").unwrap();
        let link = tmp.0.join("link.json");
        symlink(&target, &link).unwrap();
        let LoadOutcome::Corrupt { preserved } = load_from(&link).outcome else { panic!("not corrupt") };
        assert_eq!(fs::read(&target).unwrap(), b"{corrupt");
        assert_eq!(fs::read(&preserved).unwrap(), b"{corrupt");
        // Dangling symlink: missing, saves allowed.
        let dangling = tmp.0.join("dangling.json");
        symlink(tmp.0.join("nowhere"), &dangling).unwrap();
        assert_eq!(load_from(&dangling).outcome, LoadOutcome::Missing);
        // Symlink to a valid file loads.
        let good = tmp.0.join("good.json");
        save_to(&good, &clip_entries(), 10, false).unwrap();
        let good_link = tmp.0.join("good-link.json");
        symlink(&good, &good_link).unwrap();
        assert_eq!(load_from(&good_link).entries, clip_entries());
    }
}

fn tmp_leftovers(dir: &Path, name: &str) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&format!("{name}.tmp-")))
        .collect()
}

#[test]
fn rt52_atomic_write_onto_a_directory_fails_and_leaves_the_directory() {
    let tmp = TempDir::new("rt52");
    let target = tmp.0.join("clipboard.json");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("inner"), b"keep").unwrap();
    assert!(write_atomic(&target, b"new").is_err());
    assert!(target.is_dir());
    assert_eq!(fs::read(target.join("inner")).unwrap(), b"keep");
    // Missing parent and an empty path fail without creating anything.
    assert!(write_atomic(&tmp.0.join("no-such-dir").join("f.json"), b"x").is_err());
    assert!(!tmp.0.join("no-such-dir").exists());
    assert!(write_atomic(Path::new(""), b"x").is_err());
}

#[cfg(unix)]
#[test]
fn rt53_atomic_write_onto_a_symlink_replaces_the_link_and_never_the_target() {
    use std::os::unix::fs::symlink;
    let tmp = TempDir::new("rt53");
    let real = tmp.0.join("real.json");
    fs::write(&real, b"original").unwrap();
    let link = tmp.0.join("clipboard.json");
    symlink(&real, &link).unwrap();
    write_atomic(&link, b"new bytes").unwrap();
    assert!(!fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(&link).unwrap(), b"new bytes");
    assert_eq!(fs::read(&real).unwrap(), b"original");
    assert!(tmp_leftovers(&tmp.0, "clipboard.json").is_empty());
}

#[test]
fn rt54_concurrent_atomic_writers_never_interleave() {
    let tmp = TempDir::new("rt54");
    let target = Arc::new(tmp.0.join("clipboard.json"));
    let handles: Vec<_> = (0u8..8)
        .map(|i| {
            let target = target.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    write_atomic(&target, &vec![b'a' + i; 64 * 1024]).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let raw = fs::read(&*target).unwrap();
    assert_eq!(raw.len(), 64 * 1024);
    assert!(raw.iter().all(|b| *b == raw[0]));
    assert!(tmp_leftovers(&tmp.0, "clipboard.json").is_empty());
}

// ---------------------------------------------------------------------------
// AC 26: wipe

struct FailSink;

impl MemoSink for FailSink {
    fn changed(&self, _: Option<&ClipEntry>, _: Option<&ClipEntry>) -> Result<Option<String>, String> {
        Err("down".to_owned())
    }
    fn delete_all(&self, _: &[String]) -> Result<(), String> {
        Err("down".to_owned())
    }
}

fn handle_with(entries: Vec<ClipEntry>) -> Arc<ClipboardHandle> {
    let h = ClipboardHandle::new();
    h.state.lock().unwrap().entries = entries;
    h
}

const MA: &str = "0190a000-0000-7000-8000-0000000000a1";
const MB: &str = "0190a000-0000-7000-8000-0000000000b2";

fn store_sink() -> (StoreMemoSink, Arc<Mutex<Store>>) {
    let store = Arc::new(Mutex::new(device("a")));
    let sink = StoreMemoSink { store: store.clone(), vault: VAULT.to_owned(), clock: Arc::new(Mutex::new(HlcClock::default())) };
    (sink, store)
}

#[test]
fn rt60_wipe_sink_failure_changes_nothing_and_skips_the_save() {
    let entries = vec![memo(MA, "x"), clip("k", "c")];
    let h = handle_with(entries.clone());
    let saved = Cell::new(false);
    let r = wipe_entries(&h, Some(&FailSink), |_, _| saved.set(true));
    assert!(r.is_err());
    assert!(!saved.get());
    assert_eq!(h.state.lock().unwrap().entries, entries);

    // A real store sink without the vault identity fails the same way.
    let sink = StoreMemoSink {
        store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        vault: VAULT.to_owned(),
        clock: Arc::new(Mutex::new(HlcClock::default())),
    };
    assert!(wipe_entries(&h, Some(&sink), |_, _| saved.set(true)).is_err());
    assert!(!saved.get());
    assert_eq!(h.state.lock().unwrap().entries, entries);
}

#[test]
fn rt61_wipe_crash_after_delete_commit_or_after_clear_mirrors_to_clips_only() {
    let (sink, store) = store_sink();
    let ma = memo(MA, "memo a");
    let mb = ce(MB, "memo b", true, Some("n"), EntryKind::Memo);
    sink.changed(None, Some(&ma)).unwrap();
    sink.changed(None, Some(&mb)).unwrap();
    let tmp = TempDir::new("rt61");
    let path = tmp.0.join("clipboard.json");
    // Duplicate memo id in the file: one delete per id.
    let entries = vec![ma.clone(), clip("k", "a clip"), mb.clone(), ma.clone()];
    save_to(&path, &entries, 100, false).unwrap();
    let h = handle_with(entries.clone());
    // Crash after step 2/3: the save (step 4) never runs.
    wipe_entries(&h, Some(&sink), |_, _| {}).unwrap();
    assert!(h.state.lock().unwrap().entries.is_empty());
    let s = store.lock().unwrap();
    assert_eq!(count_type(&s, Kind::Memo, MA, "delete"), 1);
    assert_eq!(count_type(&s, Kind::Memo, MB, "delete"), 1);
    // Next boot: the old file still has both memos; the mirror drops them.
    let reloaded = load_from(&path).entries;
    assert_eq!(reloaded, entries);
    let p = s.projection(VAULT).unwrap();
    let mirrored = mirror(&reloaded, &p, 1);
    assert_eq!(mirrored, vec![clip("k", "a clip")]);
    drop(s);
    // Crash before step 2 (nothing committed) keeps everything.
    let (sink2, store2) = store_sink();
    sink2.changed(None, Some(&ma)).unwrap();
    let p2 = store2.lock().unwrap().projection(VAULT).unwrap();
    assert_eq!(mirror(&reloaded, &p2, 1).iter().filter(|e| e.kind == EntryKind::Memo).count(), 1);
}

#[test]
fn rt62_wipe_without_sink_or_without_memos_emits_nothing() {
    let saved = Cell::new(0);
    let h = handle_with(vec![clip("k", "c")]);
    wipe_entries(&h, Some(&FailSink), |e, _| {
        assert!(e.is_empty());
        saved.set(saved.get() + 1)
    })
    .unwrap();
    let h = handle_with(vec![memo(MA, "x")]);
    wipe_entries(&h, None, |e, _| {
        assert!(e.is_empty());
        saved.set(saved.get() + 1)
    })
    .unwrap();
    assert_eq!(saved.get(), 2);
}

// ---------------------------------------------------------------------------
// Splice / apply / mirror helpers

#[test]
fn rt63_splice_and_apply_keep_clip_positions_and_never_duplicate_memo_ids() {
    let entries = vec![memo("m1", "a"), clip("k1", "x"), memo("m2", "b"), clip("k2", "y"), memo("m1", "dup slot")];
    let incoming = vec![memo("m3", "new"), memo("m1", "a2"), clip("evil", "clip in mirror")];
    let out = splice_memos(&entries, incoming.clone());
    let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["m3", "m1", "k1", "k2"]);
    assert_eq!(out[1].text, "a2");
    let h = handle_with(entries);
    let (applied, _) = apply_memo_projection(&h, incoming);
    assert_eq!(applied, out);
    assert_eq!(h.state.lock().unwrap().entries, out);
}

#[test]
fn rt64_mirror_is_deterministic_and_idempotent_and_memo_ops_ignore_clips() {
    let (sink, store) = store_sink();
    let ma = memo(MA, "memo a");
    sink.changed(None, Some(&ma)).unwrap();
    let mut mb = memo(MB, "memo b");
    mb.note = Some(String::new());
    sink.changed(None, Some(&mb)).unwrap();
    let p = store.lock().unwrap().projection(VAULT).unwrap();
    let entries = vec![clip("k", "c"), ma.clone()];
    let once = mirror(&entries, &p, 50);
    assert_eq!(once, mirror(&entries, &p, 50));
    assert_eq!(mirror(&once, &p, 999), once);
    assert!(once.iter().all(|e| e.note.is_none()));
    assert_eq!(once.iter().find(|e| e.id == MA).unwrap().ts, 100);
    // Clips and mismatched ids never produce ops.
    let k = clip(MA, "clip with a memo id");
    assert!(memo_ops(MA, None, None, Some(&k)).is_empty());
    assert!(memo_ops(MA, None, None, Some(&memo(MB, "other id"))).is_empty());
    sink.changed(Some(&k), None).unwrap();
    assert_eq!(count_type(&store.lock().unwrap(), Kind::Memo, MA, "delete"), 0);
}

// ---------------------------------------------------------------------------
// M8: apply_queued with every variant, replayed and shuffled

fn native_store() -> Store {
    let mut s = device("a");
    s.meta_set("migration_state", "native").unwrap();
    let seed = [
        QueuedOp::ConvCreate { conv: "c1".to_owned(), title: "first".to_owned() },
        QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m1", "user", "hi", 1), parent: None },
        QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m2", "assistant", "yo", 2), parent: Some("m1".to_owned()) },
        QueuedOp::ConvCreate { conv: "c2".to_owned(), title: "second".to_owned() },
        QueuedOp::TrAdd { tr: tr("t1", "a", false) },
        QueuedOp::TrAdd { tr: tr("t2", "b", true) },
    ];
    flush(&mut s, &seed);
    s
}

/// Order-independent view of the projection (op ids dropped).
fn view(p: &Projection) -> Vec<String> {
    let mut out = Vec::new();
    for ((kind, id), state) in &p.entities {
        out.push(match state {
            EntityState::Conv(c) => {
                let msgs: BTreeMap<&String, (Option<&String>, &String)> =
                    c.messages.iter().map(|(k, m)| (k, (m.parent_id.as_ref(), &m.content))).collect();
                format!("{kind:?} {id} {:?} {:?} {msgs:?}", c.title, c.active_leaf)
            }
            EntityState::Tr(t) => format!("{kind:?} {id} {} {}", t.data["source"], t.pinned),
            other => format!("{kind:?} {id} {other:?}"),
        });
    }
    out.sort();
    out
}

fn msg_adds_unique(store: &Store) {
    let adds: Vec<String> = op_sigs(store).into_iter().filter(|s| s.2 == "msg.add").map(|s| s.3).collect();
    let unique: BTreeSet<&String> = adds.iter().collect();
    assert_eq!(adds.len(), unique.len(), "a message was added twice: {adds:?}");
}

#[test]
fn rt70_each_variant_applied_twice_in_a_row_changes_once() {
    let q = queue();
    let mut s = native_store();
    for op in &q {
        let first = apply_next(&mut s, op);
        let ops = op_count(&s);
        assert_eq!(apply_next(&mut s, op), Applied { changed: false }, "{op:?} (first {first:?})");
        assert_eq!(op_count(&s), ops);
    }
    msg_adds_unique(&s);
}

#[test]
fn rt71_shuffled_flush_then_in_order_reflush_converges_exactly_once() {
    let q = queue();
    let mut reference = native_store();
    flush(&mut reference, &q);
    let want = view(&reference.projection(VAULT).unwrap());
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for round in 0..60 {
        let mut order: Vec<usize> = (0..q.len()).collect();
        for i in (1..order.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            order.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        let shuffled: Vec<QueuedOp> = order.iter().map(|&i| q[i].clone()).collect();
        let mut s = native_store();
        let cut = round % (q.len() + 1);
        flush(&mut s, &shuffled[..cut]);
        flush(&mut s, &shuffled);
        flush(&mut s, &q);
        assert_no_duplicates(&s);
        msg_adds_unique(&s);
        assert_eq!(view(&s.projection(VAULT).unwrap()), want, "order {order:?}");
        let ops = op_count(&s);
        assert!(flush(&mut s, &q).iter().all(|c| !c), "order {order:?}: in-order replay changed");
        assert_eq!(op_count(&s), ops);
    }
}

#[test]
fn rt72_queued_ops_never_resurrect_tombstones_and_noops_are_reported() {
    let mut s = native_store();
    flush(&mut s, &[QueuedOp::ConvDelete { conv: "c1".to_owned() }, QueuedOp::TrDelete { tr: "t1".to_owned() }]);
    let ops = op_count(&s);
    let noops = [
        QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m9", "user", "late", 9), parent: Some("m2".to_owned()) },
        QueuedOp::ConvCreate { conv: "c1".to_owned(), title: "again".to_owned() },
        QueuedOp::Rename { conv: "c1".to_owned(), title: "again".to_owned() },
        QueuedOp::SetActive { conv: "c1".to_owned(), leaf: "m1".to_owned() },
        QueuedOp::ConvDelete { conv: "c1".to_owned() },
        QueuedOp::TrAdd { tr: tr("t1", "again", true) },
        QueuedOp::TrPin { tr: "t1".to_owned(), pinned: true },
        QueuedOp::TrDelete { tr: "t1".to_owned() },
        QueuedOp::TrClear { observed_ids: vec![] },
        QueuedOp::TrClear { observed_ids: vec!["t1".to_owned(), "ghost".to_owned()] },
        QueuedOp::SetActive { conv: "c2".to_owned(), leaf: "no-such-msg".to_owned() },
        QueuedOp::TrPin { tr: "ghost".to_owned(), pinned: true },
        QueuedOp::ConvDelete { conv: "ghost".to_owned() },
    ];
    assert!(flush(&mut s, &noops).iter().all(|c| !c));
    assert_eq!(op_count(&s), ops);
    let p = s.projection(VAULT).unwrap();
    assert_eq!(p.entities.get(&(Kind::Conv, conv_entity_id("c1"))), Some(&EntityState::Deleted));
    assert_eq!(p.entities.get(&(Kind::Tr, tr_entity_id("t1"))), Some(&EntityState::Deleted));
}

#[test]
fn rt73_queued_msg_add_images_evicted_invalid_valid_and_replay() {
    let mut s = native_store();
    let mut m = msg("m3", "user", "pics", 3);
    m.images = vec![
        img("v", &BASE64.encode(PNG)),
        img("e", ""),
        img("bad", "aGk"),
        img("ws", "aGVs bG8="),
        img("v2", &BASE64.encode(PNG)),
    ];
    let q = QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: m, parent: Some("m2".to_owned()) };
    assert!(apply_next(&mut s, &q).changed);
    assert!(!apply_next(&mut s, &q).changed);
    assert_eq!(count_rows(&s, "SELECT count(*) FROM blobs"), 1);
    assert_eq!(count_rows(&s, "SELECT count(*) FROM vault_blob_map WHERE remote_state = 'pending'"), 1);
    let p = s.projection(VAULT).unwrap();
    let atts = &conv(&p, "c1").messages[&msg_entity_id("c1", "m3")].attachments;
    let blob_id = hex(&hmac_sha256(&key(), PNG));
    assert_eq!(atts.len(), 5);
    assert_eq!(atts[0]["blob_id"], blob_id);
    assert_eq!(atts[4]["blob_id"], blob_id);
    for (i, att) in atts.iter().enumerate().take(4).skip(1) {
        assert_eq!(att["evicted"], true, "{i}");
        assert!(att.get("blob_id").is_none());
    }
    assert!(deletes(&s).is_empty());
}

#[test]
fn rt74_queue_json_shapes() {
    let q: QueuedOp =
        serde_json::from_str(r#"{"op":"msg_add","conv":"c","msg":{"id":"m","role":"user","content":"x","ts":1},"extra":1}"#)
            .unwrap();
    assert!(matches!(q, QueuedOp::MsgAdd { parent: None, .. }));
    assert!(serde_json::from_str::<QueuedOp>(r#"{"op":"conv_rename","conv":"c","title":"t"}"#).is_err());
    assert!(serde_json::from_str::<QueuedOp>(r#"{"op":"tr_pin","tr":"t"}"#).is_err());
    let q: QueuedOp = serde_json::from_str(r#"{"op":"tr_clear","observedIds":["a","a"]}"#).unwrap();
    assert_eq!(q, QueuedOp::TrClear { observed_ids: vec!["a".to_owned(), "a".to_owned()] });
}

#[test]
fn rt75_full_reflush_of_superseded_register_ops_changes_nothing() {
    let q = vec![
        QueuedOp::Rename { conv: "c1".to_owned(), title: "x".to_owned() },
        QueuedOp::Rename { conv: "c1".to_owned(), title: "y".to_owned() },
        QueuedOp::TrPin { tr: "t1".to_owned(), pinned: true },
        QueuedOp::TrPin { tr: "t1".to_owned(), pinned: false },
        QueuedOp::SetActive { conv: "c1".to_owned(), leaf: "m1".to_owned() },
        QueuedOp::SetActive { conv: "c1".to_owned(), leaf: "m2".to_owned() },
    ];
    let mut s = native_store();
    let start = next_qseq(&s);
    assert!(flush_from(&mut s, start, &q).iter().all(|c| *c));
    let ops = op_count(&s);
    // Crash before the queue was cleared: the next boot re-flushes it.
    let replay = flush_from(&mut s, start, &q);
    assert_eq!(replay, vec![false; q.len()], "re-flush re-applied ops");
    assert_eq!(op_count(&s), ops);
    assert_eq!(count_type(&s, Kind::Conv, &conv_entity_id("c1"), "rename"), 2);
}

#[test]
fn rt76_reflush_after_a_newer_change_does_not_clobber_it() {
    let mut s = native_store();
    let (q_rename, q_pin) = (next_qseq(&s), next_qseq(&s) + 1);
    let rename = QueuedOp::Rename { conv: "c1".to_owned(), title: "y".to_owned() };
    let pin = QueuedOp::TrPin { tr: "t1".to_owned(), pinned: true };
    assert!(apply_at(&mut s, q_rename, &rename).changed);
    assert!(apply_at(&mut s, q_pin, &pin).changed);
    // Crash before the items were removed from the queue. A newer change
    // lands before the re-flush (the same causal shape as a synced remote op).
    local(&mut s, op(950, 9_000, Kind::Conv, &conv_entity_id("c1"), "rename", json!({"title": "z"})));
    local(&mut s, op(951, 9_001, Kind::Tr, &tr_entity_id("t1"), "tr.pin", json!({"pinned": false})));
    assert_eq!(apply_at(&mut s, q_rename, &rename), Applied { changed: false });
    assert_eq!(apply_at(&mut s, q_pin, &pin), Applied { changed: false });
    let p = s.projection(VAULT).unwrap();
    assert_eq!(conv(&p, "c1").title, "z");
    assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t1"))), Some(EntityState::Tr(t)) if !t.pinned));
}

// ---------------------------------------------------------------------------
// Evicted images / invalid base64 through import

#[test]
fn rt80_evicted_and_invalid_images_import_as_metadata_with_zero_deletes() {
    let p0 = device("x").projection(VAULT).unwrap();
    let mut ws = source();
    // Unpadded, whitespace, and a data: URL are all invalid for the strict
    // decoder and become evicted metadata (the backup keeps the bytes).
    ws.conversations[1].messages[0].images = vec![
        img("u", "aGk"),
        img("w", "aGVs\nbG8="),
        img("d", &format!("data:image/png;base64,{}", BASE64.encode(PNG))),
        img("same", &BASE64.encode(PNG)),
    ];
    let plan = plan_import(&p0, &ws, &[], &BTreeSet::new(), &key());
    assert_eq!((plan.report.images, plan.report.evicted_images, plan.report.invalid_images), (2, 1, 4));
    assert_eq!(plan.blobs.len(), 1, "identical bytes share one blob row");
    assert!(plan.ops.iter().all(|o| !o.op_type.contains("delete")));

    let mut e = env("rt80", &clip_entries());
    let bytes = bytes_of(&ws);
    drive(&mut e, &bytes, &mut Vec::new(), false);
    assert!(deletes(&e.store).is_empty());
    assert_eq!(count_rows(&e.store, "SELECT count(*) FROM blobs"), 1);
    let p = e.store.projection(VAULT).unwrap();
    let atts = &conv(&p, "c2").messages[&msg_entity_id("c2", "m1")].attachments;
    assert_eq!(atts.len(), 4);
    assert!(atts[..3].iter().all(|a| a["evicted"] == true && a.get("blob_id").is_none()));
    assert_eq!(atts[3]["blob_id"], hex(&hmac_sha256(&key(), PNG)));
}

// ---------------------------------------------------------------------------
// Delta (pass-1 fixes, plan-amendments "G4 memo and migration decisions"):
// store-first commands, id resolution, tombstone recreate, finalize(None),
// lenient parse, qseq watermark, finish_push parse check, shared clock

/// A real store sink whose calls with an index in `fail` return `Err`
/// without reaching the store.
struct FlakySink {
    inner: StoreMemoSink,
    calls: Mutex<usize>,
    fail: BTreeSet<usize>,
}

impl FlakySink {
    fn new(inner: StoreMemoSink, fail: &[usize]) -> Self {
        FlakySink { inner, calls: Mutex::new(0), fail: fail.iter().copied().collect() }
    }

    fn fails_now(&self) -> bool {
        let mut n = self.calls.lock().unwrap();
        let hit = self.fail.contains(&*n);
        *n += 1;
        hit
    }
}

impl MemoSink for FlakySink {
    fn changed(&self, before: Option<&ClipEntry>, after: Option<&ClipEntry>) -> Result<Option<String>, String> {
        if self.fails_now() {
            return Err("flaky".to_owned());
        }
        self.inner.changed(before, after)
    }

    fn delete_all(&self, ids: &[String]) -> Result<(), String> {
        if self.fails_now() {
            return Err("flaky".to_owned());
        }
        self.inner.delete_all(ids)
    }
}

type MemoCmd = Box<dyn Fn(&ClipboardHandle, Option<&dyn MemoSink>, &mut dyn FnMut(&[ClipEntry], usize)) -> Result<(), String>>;

fn cmd(
    f: impl Fn(&ClipboardHandle, Option<&dyn MemoSink>, &mut dyn FnMut(&[ClipEntry], usize)) -> Result<(), String> + 'static,
) -> MemoCmd {
    Box::new(f)
}

/// Every memo command (and wipe) against the `sink_world` entries.
fn memo_cmds() -> Vec<(&'static str, MemoCmd)> {
    vec![
        ("create", cmd(|h, s, save| create_memo_in(h, s, "new memo", 500, save).map(|_| ()))),
        ("edit", cmd(|h, s, save| update_memo_in(h, s, MA, "memo a edited", 600, save).map(|_| ()))),
        ("pin", cmd(|h, s, save| toggle_pin_in(h, s, MA, save).map(|_| ()))),
        ("note", cmd(|h, s, save| set_note_in(h, s, MA, Some("fresh note".to_owned()), save).map(|_| ()))),
        ("unnote", cmd(|h, s, save| set_note_in(h, s, MB, None, save).map(|_| ()))),
        ("delete", cmd(|h, s, save| delete_entry_in(h, s, MB, save))),
        ("wipe", cmd(|h, s, save| wipe_entries(h, s, save))),
    ]
}

/// A store holding memos MA and MB and the clipboard that mirrors it.
fn sink_world() -> (StoreMemoSink, Arc<Mutex<Store>>, Vec<ClipEntry>) {
    let (sink, store) = store_sink();
    let ma = memo(MA, "memo a");
    let mb = ce(MB, "memo b", true, Some("note b"), EntryKind::Memo);
    sink.changed(None, Some(&ma)).unwrap();
    sink.changed(None, Some(&mb)).unwrap();
    (sink, store, vec![ma, clip("k", "a clip"), mb])
}

/// (id, text, pinned, note) of every memo, order and ts ignored.
fn memo_view(entries: &[ClipEntry]) -> BTreeSet<(String, String, bool, String)> {
    entries
        .iter()
        .filter(|e| e.kind == EntryKind::Memo)
        .map(|e| (e.id.clone(), e.text.clone(), e.pinned, e.note.clone().unwrap_or_default()))
        .collect()
}

fn memo_count(entries: &[ClipEntry]) -> usize {
    entries.iter().filter(|e| e.kind == EntryKind::Memo).count()
}

fn live_memo(p: &Projection, id: &str) -> Option<super::merge::MemoState> {
    match p.entities.get(&(Kind::Memo, id.to_owned())) {
        Some(EntityState::Memo(m)) => Some(m.clone()),
        _ => None,
    }
}

fn shared_projection(store: &Arc<Mutex<Store>>) -> Projection {
    store.lock().unwrap().projection(VAULT).unwrap()
}

fn shared_sink(store: &Arc<Mutex<Store>>, clock: &Arc<Mutex<HlcClock>>) -> StoreMemoSink {
    StoreMemoSink { store: store.clone(), vault: VAULT.to_owned(), clock: clock.clone() }
}

fn new_clock() -> Arc<Mutex<HlcClock>> {
    Arc::new(Mutex::new(HlcClock::default()))
}

/// Moves the env store behind the mutex a sink shares.
fn take_store(e: &mut Env) -> Arc<Mutex<Store>> {
    Arc::new(Mutex::new(std::mem::replace(&mut e.store, Store::open_in_memory().unwrap())))
}

/// `%` stands for a backslash in the JSON fixtures below.
fn esc(s: &str) -> String {
    s.replace('%', "\\")
}

#[test]
fn rt90_store_first_every_memo_command_fails_clean_retries_once_and_survives_a_crash_before_save() {
    for (name, run) in memo_cmds() {
        let (sink, store, entries) = sink_world();
        let ops = op_count(&store.lock().unwrap());
        let h = handle_with(entries.clone());
        let flaky = FlakySink::new(sink, &[0]);
        let mut saves: Vec<Vec<ClipEntry>> = Vec::new();
        // 1. The sink fails: no mutation, no save, no op; the next mirror
        //    therefore has nothing to overwrite.
        assert!(run(&h, Some(&flaky), &mut |e, _| saves.push(e.to_vec())).is_err(), "{name}");
        assert!(saves.is_empty(), "{name}: saved after a sink failure");
        assert_eq!(h.state.lock().unwrap().entries, entries, "{name}: mutated after a sink failure");
        assert_eq!(op_count(&store.lock().unwrap()), ops, "{name}: ops after a sink failure");
        assert_eq!(mirror(&entries, &shared_projection(&store), 0), entries, "{name}");
        // 2. The retry commits first, then mutates and saves exactly once.
        run(&h, Some(&flaky), &mut |e, _| saves.push(e.to_vec())).unwrap();
        let now = h.state.lock().unwrap().entries.clone();
        assert_ne!(now, entries, "{name}: retry changed nothing");
        assert_eq!(saves, vec![now.clone()], "{name}: one save of the new entries");
        let p = shared_projection(&store);
        assert_eq!(mirror(&now, &p, 0), now, "{name}: clipboard and store disagree");
        // 3. A crash after the commit and before the save: the old file plus
        //    the mirror shows every memo the user saw.
        assert_eq!(memo_view(&mirror(&entries, &p, 0)), memo_view(&now), "{name}: crash before save lost a change");
    }
}

#[test]
fn rt91_concurrent_memo_commands_leave_clipboard_and_store_in_agreement() {
    let (sink, store, entries) = sink_world();
    let sink: Arc<dyn MemoSink> = Arc::new(sink);
    let h = handle_with(entries);
    let threads: Vec<_> = (0..6usize)
        .map(|t| {
            let (h, sink) = (h.clone(), sink.clone());
            std::thread::spawn(move || {
                for j in 0..8usize {
                    let s = Some(&*sink);
                    let ts = 1_000 + (t * 8 + j) as i64;
                    let r = match (t + j) % 5 {
                        0 => update_memo_in(&h, s, MA, &format!("a {t}-{j}"), ts, |_, _| {}).map(|_| ()),
                        1 => toggle_pin_in(&h, s, MB, |_, _| {}).map(|_| ()),
                        2 => set_note_in(&h, s, MA, Some(format!("n {t}-{j}")), |_, _| {}).map(|_| ()),
                        3 => create_memo_in(&h, s, &format!("new {t}-{j}"), ts, |_, _| {}).map(|_| ()),
                        _ => update_memo_in(&h, s, MB, &format!("b {t}-{j}"), ts, |_, _| {}).map(|_| ()),
                    };
                    r.unwrap();
                }
            })
        })
        .collect();
    for th in threads {
        th.join().unwrap();
    }
    let now = h.state.lock().unwrap().entries.clone();
    let p = shared_projection(&store);
    assert_eq!(mirror(&now, &p, 0), now, "the last command of every memo is the store head");
    assert_eq!(counts(&p).3, memo_count(&now));
    // The wipe afterwards deletes every observed memo.
    wipe_entries(&h, Some(&*sink), |_, _| {}).unwrap();
    assert_eq!(counts(&shared_projection(&store)).3, 0);
}

/// Device `a` (the migrated env store) holds MA, which devices a and b edit
/// concurrently; a's projection then shows MA plus a conflict copy.
fn conflicted_env(tag: &str) -> (Env, Arc<Mutex<Store>>, Arc<Mutex<HlcClock>>, String) {
    let bytes = bytes_of(&source());
    let mut e = env(tag, &clip_entries());
    drive(&mut e, &bytes, &mut Vec::new(), false);
    let store_a = take_store(&mut e);
    let clock_a = new_clock();
    let sink_a = shared_sink(&store_a, &clock_a);
    let store_b = Arc::new(Mutex::new(device("b")));
    let sink_b = shared_sink(&store_b, &new_clock());
    let shared = memo(MA, "shared");
    sink_a.changed(None, Some(&shared)).unwrap();
    let mut remote = Remote::new(false);
    while publish(&mut store_a.lock().unwrap(), &mut remote).is_some() {}
    sync_stream(&mut store_b.lock().unwrap(), &remote, "a", &all);
    sink_b.changed(Some(&shared), Some(&memo(MA, "from b"))).unwrap();
    sink_a.changed(Some(&shared), Some(&memo(MA, "from a"))).unwrap();
    while publish(&mut store_b.lock().unwrap(), &mut remote).is_some() {}
    sync_stream(&mut store_a.lock().unwrap(), &remote, "b", &all);
    let p = shared_projection(&store_a);
    let copy = p
        .entities
        .iter()
        .find_map(|((k, id), st)| match st {
            EntityState::Memo(m) if *k == Kind::Memo && m.conflict_of.as_deref() == Some(MA) => Some(id.clone()),
            _ => None,
        })
        .expect("no conflict copy");
    (e, store_a, clock_a, copy)
}

#[test]
fn rt92_mixed_clipboard_old_new_and_conflict_copy_ids_each_reach_one_entity() {
    let (_e, store, clock, copy) = conflicted_env("rt92");
    let sink = shared_sink(&store, &clock);
    let (va, vb) = (memo_entity_id("a"), memo_entity_id("b"));
    let p = shared_projection(&store);
    // The mirror was applied, except that memo "a" still carries its
    // pre-migration id (the crash window the resolution covers).
    let mut mixed = mirror(&clip_entries(), &p, 7);
    mixed.iter_mut().filter(|m| m.id == va).for_each(|m| m.id = "a".to_owned());
    assert_eq!(memo_count(&mixed), 4, "{mixed:?}");
    assert!(mixed.iter().any(|m| m.id == copy && m.conflict_of.as_deref() == Some(MA)));
    let h = handle_with(mixed.clone());
    let s: Option<&dyn MemoSink> = Some(&sink);
    let edited = update_memo_in(&h, s, "a", "a via its old id", 10, |_, _| {}).unwrap();
    assert_eq!(edited.id, va, "the entry takes the migrated entity id");
    toggle_pin_in(&h, s, &vb, |_, _| {}).unwrap();
    set_note_in(&h, s, &copy, Some("on the copy".to_owned()), |_, _| {}).unwrap();
    update_memo_in(&h, s, MA, "winner edited", 11, |_, _| {}).unwrap();
    let fresh = create_memo_in(&h, s, "brand new", 12, |_, _| {}).unwrap().id;
    update_memo_in(&h, s, &fresh, "brand new v2", 13, |_, _| {}).unwrap();
    let now = h.state.lock().unwrap().entries.clone();
    let p = shared_projection(&store);
    let ids: Vec<&str> = now.iter().filter(|m| m.kind == EntryKind::Memo).map(|m| m.id.as_str()).collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "duplicate memo ids: {ids:?}");
    assert_eq!(ids.len(), counts(&p).3, "clipboard memos vs live entities");
    assert_eq!(mirror(&now, &p, 0), now);
    assert_eq!(live_memo(&p, &va).unwrap().text, "a via its old id");
    assert!(!live_memo(&p, &vb).unwrap().pinned, "b was pinned; the toggle through its migrated id unpins it");
    let c = live_memo(&p, &copy).unwrap();
    assert_eq!((c.note.as_str(), c.conflict_of.as_deref()), ("on the copy", Some(MA)));
    assert_eq!(live_memo(&p, MA).unwrap().text, "winner edited");
    assert_eq!(live_memo(&p, &fresh).unwrap().text, "brand new v2");
    {
        let st = store.lock().unwrap();
        assert_eq!(count_type(&st, Kind::Memo, "a", "create") + count_type(&st, Kind::Memo, "a", "edit"), 0);
        assert_eq!(count_type(&st, Kind::Memo, &va, "create"), 1);
    }

    // Delete through the old id (the entry reverted to it again).
    let mut reverted = now.clone();
    reverted.iter_mut().filter(|m| m.id == va).for_each(|m| m.id = "a".to_owned());
    let h = handle_with(reverted);
    delete_entry_in(&h, s, "a", |_, _| {}).unwrap();
    let p = shared_projection(&store);
    assert_eq!(p.entities.get(&(Kind::Memo, va.clone())), Some(&EntityState::Deleted));
    let now = h.state.lock().unwrap().entries.clone();
    assert_eq!(mirror(&now, &p, 0), now);

    // Wipe of a clipboard that mixes old, migrated, native and copy ids.
    let mut mixed = now.clone();
    mixed.iter_mut().filter(|m| m.id == vb).for_each(|m| m.id = "b".to_owned());
    let h = handle_with(mixed);
    wipe_entries(&h, s, |_, _| {}).unwrap();
    let p = shared_projection(&store);
    assert_eq!(counts(&p).3, 0, "a wiped memo stays live: {:?}", p.entities);
    assert_eq!(memo_count(&mirror(&[], &p, 0)), 0);
}

#[test]
fn rt93_edit_after_remote_delete_recreates_once_and_again_after_a_second_delete() {
    let (sink, store, entries) = sink_world();
    let h = handle_with(entries);
    let s: Option<&dyn MemoSink> = Some(&sink);
    let remote_delete =
        |id: &str, n: u64| local(&mut store.lock().unwrap(), op(n, 9_000 + n, Kind::Memo, id, "delete", json!({})));
    let creates = |id: &str| count_type(&store.lock().unwrap(), Kind::Memo, id, "create");

    remote_delete(MA, 960);
    let f1 = update_memo_in(&h, s, MA, "a v2", 10, |_, _| {}).unwrap().id;
    assert_ne!(f1, MA);
    assert_eq!(uuid::Uuid::parse_str(&f1).unwrap().get_version_num(), 7);
    // Repeated edits of the recreated memo edit it; they never recreate.
    toggle_pin_in(&h, s, &f1, |_, _| {}).unwrap();
    set_note_in(&h, s, &f1, Some("kept".to_owned()), |_, _| {}).unwrap();
    assert_eq!(update_memo_in(&h, s, &f1, "a v3", 11, |_, _| {}).unwrap().id, f1);
    assert_eq!(creates(&f1), 1);
    let p = shared_projection(&store);
    let m = live_memo(&p, &f1).unwrap();
    assert_eq!((m.text.as_str(), m.pinned, m.note.as_str()), ("a v3", true, "kept"));
    let now = h.state.lock().unwrap().entries.clone();
    assert_eq!(mirror(&now, &p, 0), now);

    // A second remote delete, then an edit: a second fresh id, one memo.
    remote_delete(&f1, 961);
    let f2 = update_memo_in(&h, s, &f1, "a v4", 12, |_, _| {}).unwrap().id;
    assert!(f2 != f1 && f2 != MA);
    let p = shared_projection(&store);
    assert_eq!(p.entities.get(&(Kind::Memo, f1.clone())), Some(&EntityState::Deleted));
    let m = live_memo(&p, &f2).unwrap();
    assert_eq!((m.text.as_str(), m.pinned, m.note.as_str()), ("a v4", true, "kept"));
    let now = h.state.lock().unwrap().entries.clone();
    assert_eq!(memo_view(&now).len(), 2, "{now:?}");
    assert_eq!(mirror(&now, &p, 0), now);

    // A pin toggle on a tombstoned memo recreates it as well.
    remote_delete(&f2, 962);
    let f3 = toggle_pin_in(&h, s, &f2, |_, _| {}).unwrap().unwrap().id;
    assert!(f3 != f2 && f3 != f1);
    let m = live_memo(&shared_projection(&store), &f3).unwrap();
    assert_eq!((m.text.as_str(), m.pinned, m.note.as_str()), ("a v4", false, "kept"));

    // Deleting a memo whose entity is already a tombstone writes nothing.
    remote_delete(&f3, 963);
    let ops = op_count(&store.lock().unwrap());
    delete_entry_in(&h, s, &f3, |_, _| {}).unwrap();
    assert_eq!(op_count(&store.lock().unwrap()), ops);
    let now = h.state.lock().unwrap().entries.clone();
    let p = shared_projection(&store);
    assert_eq!(mirror(&now, &p, 0), now);
    assert_eq!(memo_view(&now).into_iter().map(|m| m.0).collect::<Vec<_>>(), vec![MB.to_owned()]);
}

#[test]
fn rt94_finalize_with_an_untrusted_load_never_turns_a_shrunken_clipboard_or_snapshot_into_deletes() {
    let bytes = bytes_of(&source());
    let mut cur = source();
    cur.conversations.retain(|c| c.id == "c1");
    cur.conversations[0].messages[0].images[0].data.clear();
    cur.history.clear();
    let cur_bytes = bytes_of(&cur);
    for current in [None, Some(cur_bytes.as_slice())] {
        let mut e = env("rt94", &clip_entries());
        to_verified(&mut e, &bytes);
        let ops = op_count(&e.store);
        let snap = e.snap();
        let hash = if current.is_some() { "rt94-current" } else { snap.as_str() };
        let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, hash, current.map(|b| (b, hash)), None, 2)
            .unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 2 }, "{current:?}");
        assert!(deletes(&e.store).is_empty(), "{:?}", deletes(&e.store));
        assert_eq!(op_count(&e.store), ops, "a shrunken source only removes, so nothing is written");
        let p = e.store.projection(VAULT).unwrap();
        assert_eq!(counts(&p), (3, 4, 2, 2));
        assert_eq!(live_memo(&p, &memo_entity_id("b")).unwrap().note, "note b");
        let att = &conv(&p, "c1").messages[&msg_entity_id("c1", "m1")].attachments;
        assert!(att[0].get("blob_id").is_some(), "an evicted image in the current snapshot kept its blob");
        assert_eq!(e.store.meta_get("migration_final_snapshot_hash").unwrap().as_deref(), Some(hash));
    }

    // Trusted (Some): the same shrunken clipboard is a real user deletion.
    let mut e = env("rt94-trusted", &clip_entries());
    to_verified(&mut e, &bytes);
    let snap = e.snap();
    let live = [clip("k", "a clip")];
    let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, &snap, None, Some(&live), 2).unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 2 });
    let mut want = vec![(Kind::Memo, memo_entity_id("a")), (Kind::Memo, memo_entity_id("b"))];
    want.sort();
    assert_eq!(deletes(&e.store), want);

    // Untrusted while an imported memo was deleted remotely: verify of the
    // backup memos tolerates the tombstone and nothing is restored.
    let mut e = env("rt94-tomb", &clip_entries());
    assert_eq!(push_chunks(&mut e, &bytes, 1, None).unwrap(), MigrationState::BackedUp);
    import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1).unwrap();
    local(&mut e.store, op(964, 2_000, Kind::Memo, &memo_entity_id("b"), "delete", json!({})));
    assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
    let snap = e.snap();
    let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, &snap, None, None, 2).unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 2 });
    let p = e.store.projection(VAULT).unwrap();
    assert_eq!(p.entities.get(&(Kind::Memo, memo_entity_id("b"))), Some(&EntityState::Deleted));
    assert_eq!(count_type(&e.store, Kind::Memo, &memo_entity_id("b"), "create"), 1);
    assert!(live_memo(&p, &memo_restore_id("b")).is_none());
}

/// (escaped JSON string body, decoded text), `%` = backslash in both.
const ESCAPES: [(&str, &str); 19] = [
    ("%ud83d%ude00", "\u{1F600}"),
    ("%uD83D%uDE00", "\u{1F600}"),
    ("a%ud83d", "a\u{FFFD}"),
    ("%ude00b", "\u{FFFD}b"),
    ("%%ud800", "%ud800"),
    ("%%%ud800", "%\u{FFFD}"),
    ("%%%%ud800", "%%ud800"),
    ("%%%%%ud800", "%%\u{FFFD}"),
    ("%ud800%ud800%udc00", "\u{FFFD}\u{10000}"),
    ("%udc00%ud800", "\u{FFFD}\u{FFFD}"),
    ("%ud83d%u0041", "\u{FFFD}A"),
    ("%ud83d%%ude00", "\u{FFFD}%ude00"),
    ("%\"%ud800%\"", "\"\u{FFFD}\""),
    ("%ud83d%n", "\u{FFFD}\n"),
    ("%u00e9%ud800", "\u{e9}\u{FFFD}"),
    ("%uD800%uDBFF%uDFFF", "\u{FFFD}\u{10FFFF}"),
    ("%uDBFF", "\u{FFFD}"),
    ("%uE000%uD7FF", "\u{E000}\u{D7FF}"),
    ("%/%ud800%b", "/\u{FFFD}\u{8}"),
];

#[test]
fn rt95_lenient_parse_keeps_pairs_replaces_lone_halves_and_never_touches_literal_backslash_u() {
    for (fx, want) in ESCAPES {
        let raw = esc(&format!(r#"{{"conversations":[{{"id":"c","title":"{fx}","messages":[]}}]}}"#));
        let ws = parse_webstore(raw.as_bytes()).unwrap_or_else(|err| panic!("{fx}: {err}"));
        assert_eq!(ws.conversations[0].title, esc(want), "{fx}");
    }
    // The same text reaches plan and verify: every case, also inside ids,
    // migrates to native (drive panics on a verify failure).
    let mut convs: Vec<String> = ESCAPES
        .iter()
        .enumerate()
        .map(|(i, (fx, _))| {
            format!(r#"{{"id":"c{i}","title":"{fx}","messages":[{{"id":"m","role":"user","content":"{fx}","ts":1}}]}}"#)
        })
        .collect();
    convs.push(r#"{"id":"%ud800x","title":"lone id","messages":[{"id":"%udc00","role":"user","content":"x","ts":1}]}"#.to_owned());
    let raw = esc(&format!(
        r#"{{"conversations":[{}],"history":[{{"id":"%ud83d","source":"%ud83d%ude00%ud83d"}}]}}"#,
        convs.join(",")
    ));
    let mut e = env("rt95", &clip_entries());
    drive(&mut e, raw.as_bytes(), &mut Vec::new(), false);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), raw.as_bytes(), "the backup keeps the original bytes");
    let p = e.store.projection(VAULT).unwrap();
    for (i, (fx, want)) in ESCAPES.iter().enumerate() {
        let c = conv(&p, &format!("c{i}"));
        assert_eq!(c.title, esc(want), "{fx}");
        assert_eq!(c.messages[&msg_entity_id(&format!("c{i}"), "m")].content, esc(want), "{fx}");
    }
    let lone = conv(&p, "\u{FFFD}x");
    assert!(lone.messages.contains_key(&msg_entity_id("\u{FFFD}x", "\u{FFFD}")));
    assert!(matches!(
        p.entities.get(&(Kind::Tr, tr_entity_id("\u{FFFD}"))),
        Some(EntityState::Tr(t)) if t.data["source"] == "\u{1F600}\u{FFFD}"
    ));
    assert!(deletes(&e.store).is_empty());
}

#[test]
fn rt96_lenient_parse_of_invalid_utf8_and_escapes_next_to_it() {
    // A CESU-8 surrogate, a truncated 4-byte sequence and a stray byte, then
    // a lone escape: all become U+FFFD and the snapshot migrates.
    let mut raw = br#"{"conversations":[{"id":"c","title":"a"#.to_vec();
    raw.extend_from_slice(b"\xed\xa0\x80b\xf0\x9f\x98c\xff");
    raw.extend_from_slice(esc(r#"%ud800","messages":[]}]}"#).as_bytes());
    let title = parse_webstore(&raw).unwrap().conversations[0].title.clone();
    assert_eq!(title.replace('\u{FFFD}', ""), "abc", "{title:?}");
    assert!(title.starts_with('a') && title.ends_with("\u{FFFD}\u{FFFD}"), "{title:?}");
    let mut e = env("rt96", &clip_entries());
    drive(&mut e, &raw, &mut Vec::new(), false);
    assert_eq!(conv(&e.store.projection(VAULT).unwrap(), "c").title, title);
    assert_eq!(fs::read(e.dir.webstore()).unwrap(), raw);

    // A backslash followed by an invalid byte is no escape at all: the push
    // fails closed (reset, no backup) and never panics.
    let mut bad = br#"{"conversations":[{"id":"c","title":"x\"#.to_vec();
    bad.extend_from_slice(b"\xff\",\"messages\":[]}]}");
    assert!(parse_webstore(&bad).is_err());
    let mut e = env("rt96-bad", &clip_entries());
    assert!(matches!(push(&mut e, 0, 1, &bad), Err(MigrationError::Chunk(_))));
    assert_eq!(e.state(), MigrationState::None);
    assert!(!e.dir.webstore().exists() && !e.dir.clipboard_backup().exists());
    // Escapes cut short are rejected the same way.
    assert!(parse_webstore(esc(r#"{"conversations":[{"id":"c","title":"%ud8"#).as_bytes()).is_err());
    assert!(parse_webstore(esc(r#"{"conversations":[{"id":"c","title":"%ud83d%u"}]}"#).as_bytes()).is_err());
}

#[test]
fn rt97_qseq_watermark_replay_out_of_order_gap_and_register_ops_at_or_below_ack() {
    let bytes = bytes_of(&source());
    let mut e = env("rt97", &clip_entries());
    drive(&mut e, &bytes, &mut Vec::new(), false); // ack 9
    let s = &mut e.store;
    let ops0 = op_count(s);
    let rename = |t: &str| QueuedOp::Rename { conv: "c1".to_owned(), title: t.to_owned() };
    let pin = |p: bool| QueuedOp::TrPin { tr: "t2".to_owned(), pinned: p };
    let active = || QueuedOp::SetActive { conv: "c1".to_owned(), leaf: "m1".to_owned() };
    // In qseq order: register ops at or below the ack only advance the
    // watermark; add and delete ops follow the id rule.
    let at_or_below = [
        (1, QueuedOp::TrDelete { tr: "t1".to_owned() }, true),
        (2, rename("queued rename"), false),
        (3, pin(false), false),
        (4, active(), false),
        (5, QueuedOp::MsgAdd { conv: "c1".to_owned(), msg: msg("m1", "user", "hi 안녕", 1), parent: None }, false),
        (9, QueuedOp::ConvCreate { conv: "c3".to_owned(), title: "x".to_owned() }, false),
    ];
    for (q, op, changed) in &at_or_below {
        assert_eq!(apply_at(s, *q, op).changed, *changed, "qseq {q}");
        assert_eq!(flushed_qseq(s).unwrap(), Some(*q));
    }
    assert_eq!(op_count(s), ops0 + 1);
    let p = s.projection(VAULT).unwrap();
    let c1 = conv(&p, "c1");
    assert_eq!((c1.title.as_str(), c1.active_leaf.clone()), ("first", Some(msg_entity_id("c1", "m2"))));
    assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t2"))), Some(EntityState::Tr(t)) if t.pinned));
    // Above the ack the same register values apply.
    let above = [(10, rename("queued rename")), (11, pin(false)), (12, active())];
    for (q, op) in &above {
        assert!(apply_at(s, *q, op).changed, "qseq {q}");
    }
    assert_eq!(op_count(s), ops0 + 4);
    // A full replay changes nothing, in either direction.
    let every: Vec<(u64, QueuedOp)> =
        at_or_below.iter().map(|(q, o, _)| (*q, o.clone())).chain(above.iter().cloned()).collect();
    for (q, op) in every.iter().chain(every.iter().rev()) {
        assert!(!apply_at(s, *q, op).changed, "replay qseq {q}");
    }
    assert_eq!(op_count(s), ops0 + 4);
    assert_eq!(flushed_qseq(s).unwrap(), Some(12));
    let p = s.projection(VAULT).unwrap();
    let c1 = conv(&p, "c1");
    assert_eq!((c1.title.as_str(), c1.active_leaf.clone()), ("queued rename", Some(msg_entity_id("c1", "m1"))));
    assert!(matches!(p.entities.get(&(Kind::Tr, tr_entity_id("t2"))), Some(EntityState::Tr(t)) if !t.pinned));

    // Out-of-order delivery and a gap: an unseen item at or below the
    // watermark is skipped (G8 must flush in ascending qseq order).
    let add = |id: &str| QueuedOp::TrAdd { tr: tr(id, id, false) };
    assert!(!apply_at(s, 11, &add("t7")).changed);
    assert!(apply_at(s, 14, &add("t8")).changed);
    assert!(!apply_at(s, 13, &add("t9")).changed);
    assert_eq!(flushed_qseq(s).unwrap(), Some(14));
    let p = s.projection(VAULT).unwrap();
    assert!(p.entities.contains_key(&(Kind::Tr, tr_entity_id("t8"))));
    for gone in ["t7", "t9"] {
        assert!(!p.entities.contains_key(&(Kind::Tr, tr_entity_id(gone))), "{gone}");
    }
}

#[test]
fn rt98_crash_between_any_two_queue_items_flushes_each_exactly_once() {
    let bytes = bytes_of(&source());
    // 5..=9 at or below the ack (9), then the queue fixture, then the same
    // registers updated twice.
    let mut items: Vec<(u64, QueuedOp)> = vec![
        (5, QueuedOp::Rename { conv: "c1".to_owned(), title: "r0".to_owned() }),
        (6, QueuedOp::TrPin { tr: "t1".to_owned(), pinned: true }),
        (7, QueuedOp::SetActive { conv: "c1".to_owned(), leaf: "m1".to_owned() }),
        (8, QueuedOp::TrDelete { tr: "t2".to_owned() }),
        (9, QueuedOp::ConvCreate { conv: "c4".to_owned(), title: "four".to_owned() }),
    ];
    items.extend(queue().into_iter().zip(10u64..).map(|(op, q)| (q, op)));
    items.extend([
        (30, QueuedOp::Rename { conv: "c9".to_owned(), title: "again".to_owned() }),
        (31, QueuedOp::TrPin { tr: "t9".to_owned(), pinned: false }),
        (32, QueuedOp::SetActive { conv: "c9".to_owned(), leaf: "q2".to_owned() }),
        (33, QueuedOp::Rename { conv: "c9".to_owned(), title: "q renamed".to_owned() }),
        (34, QueuedOp::TrPin { tr: "t9".to_owned(), pinned: true }),
    ]);
    let run = |s: &mut Store, items: &[(u64, QueuedOp)]| {
        for (q, op) in items {
            apply_at(s, *q, op);
        }
    };
    // Memo heads carry fresh op ids per run; the queue never touches memos.
    let flush_view = |s: &Store| -> Vec<String> {
        view(&s.projection(VAULT).unwrap()).into_iter().filter(|v| !v.starts_with("Memo ")).collect()
    };
    let mut reference = env("rt98-ref", &clip_entries());
    drive(&mut reference, &bytes, &mut Vec::new(), false);
    run(&mut reference.store, &items);
    let want = flush_view(&reference.store);
    let want_ops = op_sigs(&reference.store);
    for cut in 0..=items.len() {
        let mut e = env("rt98", &clip_entries());
        drive(&mut e, &bytes, &mut Vec::new(), false);
        run(&mut e.store, &items[..cut]);
        e.reopen(); // crash between item cut-1 and item cut
        run(&mut e.store, &items);
        assert_eq!(op_sigs(&e.store), want_ops, "cut {cut}: ops differ from the uninterrupted flush");
        assert_eq!(flush_view(&e.store), want, "cut {cut}");
        assert_eq!(flushed_qseq(&e.store).unwrap(), Some(34));
    }
}

#[test]
fn rt99_finish_push_accepts_exactly_what_import_reads_and_recovers_once_repaired() {
    let bytes = bytes_of(&source());
    let accepted: [&[u8]; 3] = [
        br#"{"entries":[],"extra":{"x":1}}"#,
        br#"{"entries":[],"max_entries":null}"#,
        br#"{"entries":[{"id":"x","text":"t","preview":"t","ts":1,"pinned":false,"kind":"memo","future":true}]}"#,
    ];
    for raw in accepted {
        let mut e = env("rt99-ok", &[]);
        fs::write(&e.clip_src, raw).unwrap();
        let probe = e._tmp.0.join("probe.json");
        fs::write(&probe, raw).unwrap();
        assert_eq!(load_from(&probe).outcome, LoadOutcome::Loaded);
        drive(&mut e, &bytes, &mut Vec::new(), false); // import reads the backup
        assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), raw);
    }
    let rejected: Vec<Vec<u8>> = vec![
        br#"{"max_entries":3}"#.to_vec(),
        br#"{"entries":[{"id":"x"}]}"#.to_vec(),
        br#"{"entries":[],"max_entries":-1}"#.to_vec(),
        b"\xef\xbb\xbf{\"entries\":[]}".to_vec(),
        esc(r#"{"entries":[{"id":"x","text":"%ud800","preview":"","ts":1,"pinned":false}]}"#).into_bytes(),
        br#"{"entries":[{"id":"x","text":"t","preview":"t","ts":1,"pinned":false,"kind":"note"}]}"#.to_vec(),
    ];
    for raw in rejected.iter().map(Some).chain([None]) {
        let mut e = env("rt99-bad", &[]);
        match raw {
            Some(raw) => {
                fs::write(&e.clip_src, raw).unwrap();
                let probe = e._tmp.0.join("probe.json");
                fs::write(&probe, raw).unwrap();
                assert!(!matches!(load_from(&probe).outcome, LoadOutcome::Loaded), "{raw:?}");
            }
            None => {
                fs::remove_file(&e.clip_src).unwrap();
                fs::create_dir(&e.clip_src).unwrap();
            }
        }
        // G8 retries whole 3-chunk cycles; each fails, nothing is backed up.
        for _ in 0..3 {
            assert!(push_chunks(&mut e, &bytes, 3, None).is_err(), "{raw:?}");
            assert_eq!(e.state(), MigrationState::None);
            assert!(!e.dir.clipboard_backup().exists() && !e.dir.webstore().exists() && !e.dir.manifest().exists());
            assert_eq!(op_count(&e.store), 0);
        }
        match raw {
            Some(raw) => assert_eq!(&fs::read(&e.clip_src).unwrap(), raw, "the original was touched"),
            None => fs::remove_dir(&e.clip_src).unwrap(),
        }
        save_to(&e.clip_src, &clip_entries(), 500, false).unwrap();
        let repaired = fs::read(&e.clip_src).unwrap();
        let mut state = MigrationState::None;
        for _ in 0..2 {
            if let Ok(st) = push_chunks(&mut e, &bytes, 3, None) {
                state = st;
                break;
            }
        }
        assert_eq!(state, MigrationState::BackedUp, "{raw:?}: no recovery after the repair");
        assert_eq!(fs::read(e.dir.clipboard_backup()).unwrap(), repaired);
        assert_eq!(fs::read(e.dir.webstore()).unwrap(), bytes);
    }

    // A missing clipboard has no backup and migrates with no memo.
    let mut e = env("rt99-missing", &[]);
    fs::remove_file(&e.clip_src).unwrap();
    drive(&mut e, &bytes, &mut Vec::new(), false);
    assert!(!e.dir.clipboard_backup().exists());
    let manifest: Manifest = serde_json::from_slice(&fs::read(e.dir.manifest()).unwrap()).unwrap();
    assert_eq!(manifest.clipboard_sha256, None);
}

#[test]
fn rt100_one_device_clock_orders_migration_flush_and_sink_ops_monotonically() {
    let bytes = bytes_of(&source());
    let mut e = env("rt100", &clip_entries());
    let clock = new_clock();
    // A remote stamp advanced the device clock 10 h ahead of the wall clock.
    let ahead = Hlc { wall_ms: store_now() + 10 * 3_600_000, ctr: 7 };
    clock.lock().unwrap().observe(ahead, store_now());
    assert_eq!(push_chunks(&mut e, &bytes, 2, None).unwrap(), MigrationState::BackedUp);
    import(&mut e.store, &e.dir, VAULT, &key(), &mut clock.lock().unwrap(), 1_000).unwrap();
    assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
    let snap = e.snap();
    let live = vec![memo("a", "memo a v2"), clip("k", "a clip")];
    let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut clock.lock().unwrap(), &snap, None, Some(&live), 3)
        .unwrap();
    assert_eq!(out, Finalize::Native { ack_qseq: 3 });
    let rename = QueuedOp::Rename { conv: "c1".to_owned(), title: "later".to_owned() };
    assert!(apply_queued(&mut e.store, VAULT, &key(), &mut clock.lock().unwrap(), 4, &rename).unwrap().changed);
    let pin = QueuedOp::TrPin { tr: "t1".to_owned(), pinned: true };
    assert!(apply_queued(&mut e.store, VAULT, &key(), &mut clock.lock().unwrap(), 5, &pin).unwrap().changed);
    let store = take_store(&mut e);
    let sink = shared_sink(&store, &clock);
    sink.changed(None, Some(&memo(MA, "after native"))).unwrap();
    let va = memo_entity_id("a");
    let a = live_memo(&shared_projection(&store), &va).unwrap();
    sink.changed(Some(&memo(&va, &a.text)), Some(&memo(&va, "memo a v3"))).unwrap();

    let s = store.lock().unwrap();
    let (set, _) = s.load_set(VAULT).unwrap();
    let mut own: Vec<(u64, Hlc)> = set
        .iter()
        .filter_map(|op| op.dots.iter().find(|d| d.device == "a").map(|d| (d.seq, op.body.hlc)))
        .collect();
    own.sort();
    assert!(own.len() > 10, "{own:?}");
    assert!(own.iter().all(|(_, h)| *h > ahead), "an own op is stamped behind the device clock");
    assert!(own.windows(2).all(|w| w[0].1 < w[1].1), "own HLCs are not strictly increasing: {own:?}");
}

/// `resolve_memo_id` maps a pre-migration id whose migrated v5 entity is a
/// tombstone to the live `memo_restore_id(id)` (AD-A3). In either G7 order
/// (mirror then sink, or sink before the mirror) every user memo maps to
/// exactly one entity, edits and the wipe reach the restored one.
/// History: this test used to assert that the old id resolved to the
/// tombstone and that a sink-before-mirror edit made a fresh copy (count 3);
/// that duplicate was superseded by AD-A3.
#[test]
fn rt101_memo_restored_by_finalize_needs_the_mirror_before_the_sink() {
    let bytes = bytes_of(&source());
    let va = memo_entity_id("a");
    let mut live = clip_entries();
    live[0].text = "memo a edited during migration".to_owned();
    let restored_world = |tag: &str| {
        let mut e = env(tag, &clip_entries());
        assert_eq!(push_chunks(&mut e, &bytes, 1, None).unwrap(), MigrationState::BackedUp);
        import(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, 1).unwrap();
        local(&mut e.store, op(970, 3_000, Kind::Memo, &va, "delete", json!({})));
        assert!(matches!(verify(&mut e.store, &e.dir, VAULT).unwrap(), Verify::Passed(_)));
        let snap = e.snap();
        let out = finalize(&mut e.store, &e.dir, VAULT, &key(), &mut e.clock, &snap, None, Some(&live), 1).unwrap();
        assert_eq!(out, Finalize::Native { ack_qseq: 1 });
        let store = take_store(&mut e);
        (e, store)
    };
    let restored = memo_restore_id("a");
    let (_e, store) = restored_world("rt101");
    let p = shared_projection(&store);
    assert_eq!(live_memo(&p, &restored).unwrap().text, "memo a edited during migration");
    assert_eq!(resolve_memo_id(&p, "a"), restored, "the old id resolves to the live restore, not the tombstone");

    // G7 order: mirror, then sink. One memo per user memo; edits and the
    // wipe reach the restored entity.
    let mirrored = mirror(&live, &p, 5);
    assert_eq!(memo_count(&mirrored), 2);
    assert!(mirrored.iter().any(|m| m.id == restored));
    let sink = shared_sink(&store, &new_clock());
    let s: Option<&dyn MemoSink> = Some(&sink);
    let h = handle_with(mirrored);
    assert_eq!(update_memo_in(&h, s, &restored, "edited after native", 9, |_, _| {}).unwrap().id, restored);
    let now = h.state.lock().unwrap().entries.clone();
    let p = shared_projection(&store);
    assert_eq!(mirror(&now, &p, 0), now);
    assert_eq!(counts(&p).3, 2);
    wipe_entries(&h, s, |_, _| {}).unwrap();
    assert_eq!(counts(&shared_projection(&store)).3, 0);

    // Sink before the mirror (G7 order violated): the edit still lands on
    // the restored memo, with no fresh UUIDv7 and no second copy.
    let (_e2, store2) = restored_world("rt101-early");
    let sink2 = shared_sink(&store2, &new_clock());
    let h = handle_with(live.clone());
    let edited = update_memo_in(&h, Some(&sink2), "a", "edited before the mirror", 9, |_, _| {}).unwrap().id;
    assert_eq!(edited, restored, "no fresh id");
    let p = shared_projection(&store2);
    assert_eq!(live_memo(&p, &restored).unwrap().text, "edited before the mirror");
    assert_eq!(counts(&p).3, 2, "restored + b");
    let now = h.state.lock().unwrap().entries.clone();
    let copies = mirror(&now, &p, 0).iter().filter(|e| e.kind == EntryKind::Memo && e.text == "edited before the mirror").count();
    assert_eq!(copies, 1, "exactly one copy of the text");
}

/// Memo commands and the wipe save under the clipboard lock
/// (plan-amendments "store-first" step 3, AD-A4), so no command can land
/// between a wipe's clear and its save; a memo made after the wipe is the
/// last write and survives in the file, the store and the mirror.
/// History: the wipe used to save `[]` after releasing the lock (expected
/// `locked == false` for "wipe", and a create racing into that gap was
/// overwritten in the file); AD-A4 moved the save under the lock.
#[test]
fn rt102_memo_command_and_wipe_saves_hold_the_clipboard_lock() {
    for (name, run) in memo_cmds() {
        let (sink, _store, entries) = sink_world();
        let h = handle_with(entries);
        let mut locked = None;
        run(&h, Some(&sink), &mut |_, _| locked = Some(h.state.try_lock().is_err())).unwrap();
        assert_eq!(locked, Some(true), "{name}: save under the clipboard lock");
    }

    let (sink, store, entries) = sink_world();
    let h = handle_with(entries);
    let mut file: Vec<Vec<ClipEntry>> = Vec::new();
    let mut locked = None;
    wipe_entries(&h, Some(&sink), |wiped, _| {
        // Never call into the handle here: the clipboard lock is held.
        locked = Some(h.state.try_lock().is_err());
        file.push(wiped.to_vec());
    })
    .unwrap();
    assert_eq!(locked, Some(true), "the wipe saves under the clipboard lock");
    assert_eq!(file, vec![Vec::<ClipEntry>::new()], "the wipe saves no memo and no clip");
    assert_eq!(counts(&shared_projection(&store)).3, 0);

    let m = create_memo_in(&h, Some(&sink), "made after the wipe", 1, |e, _| file.push(e.to_vec())).unwrap();
    assert_eq!(file.len(), 2);
    assert_eq!(memo_view(file.last().unwrap()).into_iter().map(|m| m.0).collect::<Vec<_>>(), vec![m.id.clone()]);
    let p = shared_projection(&store);
    let restored = mirror(file.last().unwrap(), &p, 0);
    assert_eq!(memo_view(&restored).into_iter().map(|m| m.0).collect::<Vec<_>>(), vec![m.id]);
}

// ---------------------------------------------------------------------------
// Final (AD-B1 rebind emit and unknown-id Err, AD-A3 restore resolution,
// AD-A4 wipe save under the lock)

/// A clipboard after a rebind chain: "a" was remapped to its migrated id by
/// a pin toggle, then recreated under a fresh UUIDv7 by an edit through that
/// id after a remote delete; "b" was remapped by a note. Every rebind is
/// flagged for `clipboard:cleared`. Returns (sink, store, handle, va, vb,
/// fresh).
fn rebind_chain() -> (StoreMemoSink, Arc<Mutex<Store>>, Arc<ClipboardHandle>, String, String, String) {
    let (sink, store) = store_sink();
    let (va, vb) = (memo_entity_id("a"), memo_entity_id("b"));
    sink.changed(None, Some(&memo(&va, "memo a"))).unwrap();
    sink.changed(None, Some(&memo(&vb, "memo b"))).unwrap();
    let h = handle_with(vec![memo("a", "memo a"), clip("k", "a clip"), memo("b", "memo b")]);
    let s: Option<&dyn MemoSink> = Some(&sink);

    let pinned = toggle_pin_in(&h, s, "a", |_, _| {}).unwrap();
    assert_eq!(pinned.as_ref().map(|e| e.id.as_str()), Some(va.as_str()), "remap to the migrated id");
    assert!(rebound("a", pinned.as_ref()), "a remap must emit clipboard:cleared");
    let noted = set_note_in(&h, s, "b", Some("note b".to_owned()), |_, _| {}).unwrap();
    assert_eq!(noted.as_ref().map(|e| e.id.as_str()), Some(vb.as_str()));
    assert!(rebound("b", noted.as_ref()));

    local(&mut store.lock().unwrap(), op(980, 9_980, Kind::Memo, &va, "delete", json!({})));
    let edited = update_memo_in(&h, s, &va, "memo a v2", 5, |_, _| {}).unwrap();
    let fresh = edited.id.clone();
    assert!(fresh != va && fresh != "a", "{fresh}");
    assert_eq!(uuid::Uuid::parse_str(&fresh).unwrap().get_version_num(), 7);
    assert!(rebound(&va, Some(&edited)), "a recreate must emit clipboard:cleared");
    (sink, store, h, va, vb, fresh)
}

#[test]
fn rt103_rebind_chain_remap_then_tombstone_recreate_lands_on_one_entity_and_flags_each_rebind() {
    let (sink, store, h, va, vb, fresh) = rebind_chain();
    let s: Option<&dyn MemoSink> = Some(&sink);
    // Commands through the current ids never rebind again; clips never do.
    let e = update_memo_in(&h, s, &fresh, "memo a v3", 6, |_, _| {}).unwrap();
    assert_eq!(e.id, fresh);
    assert!(!rebound(&fresh, Some(&e)));
    let b = toggle_pin_in(&h, s, &vb, |_, _| {}).unwrap();
    assert!(!rebound(&vb, b.as_ref()));
    let k = toggle_pin_in(&h, s, "k", |_, _| {}).unwrap();
    assert!(!rebound("k", k.as_ref()));

    let p = shared_projection(&store);
    let now = h.state.lock().unwrap().entries.clone();
    let ids: BTreeSet<String> = memo_view(&now).into_iter().map(|m| m.0).collect();
    assert_eq!(ids, BTreeSet::from([fresh.clone(), vb.clone()]), "{now:?}");
    assert_eq!(p.entities.get(&(Kind::Memo, va.clone())), Some(&EntityState::Deleted));
    let a = live_memo(&p, &fresh).unwrap();
    assert_eq!((a.text.as_str(), a.pinned, a.note.as_str()), ("memo a v3", true, ""), "the recreate kept the pin");
    let b = live_memo(&p, &vb).unwrap();
    assert_eq!((b.text.as_str(), b.pinned, b.note.as_str()), ("memo b", true, "note b"));
    assert_eq!(counts(&p).3, 2);
    assert_eq!(mirror(&now, &p, 0), now);
    let st = store.lock().unwrap();
    assert!(op_sigs(&st).iter().all(|s| s.1 != "a" && s.1 != "b"), "an op addresses a pre-migration id");
    assert_eq!(count_type(&st, Kind::Memo, &fresh, "create"), 1);
    assert_eq!(count_type(&st, Kind::Memo, &va, "create"), 1);
    assert_eq!(count_type(&st, Kind::Memo, &vb, "create"), 1);
}

type StaleCmd = Box<dyn Fn(&ClipboardHandle, Option<&dyn MemoSink>, &str, &mut dyn FnMut(&[ClipEntry], usize)) -> Result<(), String>>;

fn stale(
    f: impl Fn(&ClipboardHandle, Option<&dyn MemoSink>, &str, &mut dyn FnMut(&[ClipEntry], usize)) -> Result<(), String>
        + 'static,
) -> StaleCmd {
    Box::new(f)
}

/// Every command that takes an entry id.
fn id_cmds() -> Vec<(&'static str, StaleCmd)> {
    vec![
        ("edit", stale(|h, s, id, save| update_memo_in(h, s, id, "typed into a stale row", 9, save).map(|_| ()))),
        ("pin", stale(|h, s, id, save| toggle_pin_in(h, s, id, save).map(|_| ()))),
        ("note", stale(|h, s, id, save| set_note_in(h, s, id, Some("stale note".to_owned()), save).map(|_| ()))),
        ("unnote", stale(|h, s, id, save| set_note_in(h, s, id, None, save).map(|_| ()))),
        ("delete", stale(|h, s, id, save| delete_entry_in(h, s, id, save))),
    ]
}

/// AD-B1: after a rebind the webview may still hold an old id. With a sink
/// every command on it fails loudly (no sink call, no op, no save, no
/// change); without a sink it stays the pre-sync no-op (edit always failed).
/// The refetched id then works.
#[test]
fn rt104_every_command_on_a_stale_id_after_a_rebind_errs_with_a_sink_and_is_a_noop_without() {
    for (name, run) in id_cmds() {
        for with_sink in [true, false] {
            for which in 0..4 {
                let (sink, store, h, va, vb, fresh) = rebind_chain();
                let (id, current) = match which {
                    0 => ("a".to_owned(), Some(fresh)),
                    1 => (va, Some(fresh)),
                    2 => ("b".to_owned(), Some(vb)),
                    _ => ("memo-never-seen".to_owned(), None),
                };
                let counted = FlakySink::new(sink, &[]);
                let s: Option<&dyn MemoSink> = with_sink.then_some(&counted as &dyn MemoSink);
                let before = h.state.lock().unwrap().entries.clone();
                let ops = op_count(&store.lock().unwrap());
                let mut saves = 0;
                let r = run(&h, s, &id, &mut |_, _| saves += 1);
                let tag = format!("{name} on {id} (sink: {with_sink})");
                assert_eq!(h.state.lock().unwrap().entries, before, "{tag}: entries changed");
                assert_eq!(op_count(&store.lock().unwrap()), ops, "{tag}: store changed");
                assert_eq!(*counted.calls.lock().unwrap(), 0, "{tag}: the sink was reached");
                if with_sink || name == "edit" {
                    assert!(r.is_err(), "{tag}: a silent Ok drops the user's change");
                    assert_eq!(saves, 0, "{tag}: saved after an error");
                } else {
                    assert_eq!(r, Ok(()), "{tag}");
                }

                // The webview refetched (clipboard:cleared): the current id works.
                if let (true, Some(current)) = (with_sink, current) {
                    run(&h, s, &current, &mut |_, _| {}).unwrap_or_else(|e| panic!("{tag} -> {current}: {e}"));
                    assert_eq!(*counted.calls.lock().unwrap(), 1, "{tag}");
                    let now = h.state.lock().unwrap().entries.clone();
                    let p = shared_projection(&store);
                    assert!(now.iter().all(|e| e.id != id), "{tag}: the stale id came back");
                    assert_eq!(mirror(&now, &p, 0), now, "{tag}: clipboard and store disagree");
                }
            }
        }
    }
}

/// AD-A3: the restore id wins only over a migrated tombstone.
#[test]
fn rt105_resolve_memo_id_prefers_a_live_restore_only_over_a_migrated_tombstone() {
    let (sink, store) = store_sink();
    let (va, ra) = (memo_entity_id("a"), memo_restore_id("a"));
    let s: Option<&dyn MemoSink> = Some(&sink);
    let p = || shared_projection(&store);
    assert_eq!(resolve_memo_id(&p(), "a"), "a", "no entity: the id itself");
    // A restore with no migrated entity is never consulted (finalize only
    // restores over a migrated tombstone, so this state is not reachable).
    sink.changed(None, Some(&memo(&ra, "restored a"))).unwrap();
    assert_eq!(resolve_memo_id(&p(), "a"), "a");
    // Live migrated and live restore: the migrated entity.
    sink.changed(None, Some(&memo(&va, "memo a"))).unwrap();
    assert_eq!(resolve_memo_id(&p(), "a"), va);
    // Migrated tombstone and live restore: the restore.
    local(&mut store.lock().unwrap(), op(981, 9_981, Kind::Memo, &va, "delete", json!({})));
    assert_eq!(resolve_memo_id(&p(), "a"), ra);
    // Ids that name an entity resolve to themselves, tombstone or not.
    assert_eq!(resolve_memo_id(&p(), &ra), ra);
    assert_eq!(resolve_memo_id(&p(), &va), va);

    // Every command through "a" reaches the restore: rebind, no fresh id.
    let h = handle_with(vec![memo("a", "restored a")]);
    let pinned = toggle_pin_in(&h, s, "a", |_, _| {}).unwrap();
    assert_eq!(pinned.as_ref().map(|e| e.id.as_str()), Some(ra.as_str()));
    assert!(rebound("a", pinned.as_ref()));
    let h = handle_with(vec![ce("a", "restored a", true, None, EntryKind::Memo)]);
    let noted = set_note_in(&h, s, "a", Some("n".to_owned()), |_, _| {}).unwrap();
    assert_eq!(noted.map(|e| e.id), Some(ra.clone()));
    let h = handle_with(vec![ce("a", "restored a", true, Some("n"), EntryKind::Memo)]);
    assert_eq!(update_memo_in(&h, s, "a", "restored a v2", 3, |_, _| {}).unwrap().id, ra);
    let m = live_memo(&p(), &ra).unwrap();
    assert_eq!((m.text.as_str(), m.pinned, m.note.as_str()), ("restored a v2", true, "n"));
    {
        let st = store.lock().unwrap();
        assert_eq!(count_type(&st, Kind::Memo, &ra, "create"), 1, "no recreate of a live restore");
        assert!(op_sigs(&st).iter().all(|s| s.1 == ra || s.1 == va), "only ra and va: {:?}", op_sigs(&st));
    }

    // Delete through "a" deletes the restore; with both tombstoned "a"
    // resolves to the migrated tombstone and an edit recreates once.
    let h = handle_with(vec![ce("a", "restored a v2", true, Some("n"), EntryKind::Memo)]);
    delete_entry_in(&h, s, "a", |_, _| {}).unwrap();
    assert_eq!(p().entities.get(&(Kind::Memo, ra.clone())), Some(&EntityState::Deleted));
    assert_eq!(resolve_memo_id(&p(), "a"), va);
    let h = handle_with(vec![ce("a", "restored a v2", true, Some("n"), EntryKind::Memo)]);
    let fresh = update_memo_in(&h, s, "a", "back again", 4, |_, _| {}).unwrap().id;
    assert!(fresh != ra && fresh != va && fresh != "a", "{fresh}");
    assert_eq!(counts(&p()).3, 1);
    let m = live_memo(&p(), &fresh).unwrap();
    assert_eq!((m.text.as_str(), m.pinned, m.note.as_str()), ("back again", true, "n"));

    // Observation: an id that itself names a tombstone never consults its
    // own restore id. finalize can only plan such a restore for a live memo
    // whose clipboard id is a native entity id, which no pre-native
    // clipboard holds (the sink and the mirror start at native).
    let native = "memo-native-tombstone";
    sink.changed(None, Some(&memo(native, "native"))).unwrap();
    local(&mut store.lock().unwrap(), op(982, 9_982, Kind::Memo, native, "delete", json!({})));
    sink.changed(None, Some(&memo(&memo_restore_id(native), "native restored"))).unwrap();
    assert_eq!(resolve_memo_id(&p(), native), native);
}

/// A wipe after a rebind chain deletes the current entities; a failing sink
/// leaves everything (entries, store, file) untouched and the lock free;
/// the save runs once, under the lock, with no entry (AD-A4).
#[test]
fn rt106_wipe_after_a_rebind_chain_deletes_the_current_ids_fails_clean_and_saves_under_the_lock() {
    let (sink, store, h, va, vb, fresh) = rebind_chain();
    let flaky = FlakySink::new(sink, &[0]);
    let before = h.state.lock().unwrap().entries.clone();
    let ops = op_count(&store.lock().unwrap());
    let mut saves: Vec<Vec<ClipEntry>> = Vec::new();
    assert!(wipe_entries(&h, Some(&flaky), |e, _| saves.push(e.to_vec())).is_err());
    assert_eq!(h.state.lock().unwrap().entries, before);
    assert_eq!(op_count(&store.lock().unwrap()), ops);
    assert!(saves.is_empty(), "saved after a failed commit");
    assert!(h.state.try_lock().is_ok(), "the clipboard lock was left held");

    let mut locked = None;
    wipe_entries(&h, Some(&flaky), |e, _| {
        locked = Some(h.state.try_lock().is_err());
        saves.push(e.to_vec());
    })
    .unwrap();
    assert_eq!(locked, Some(true), "the wipe saves under the clipboard lock");
    assert_eq!(saves, vec![Vec::<ClipEntry>::new()]);
    assert!(h.state.lock().unwrap().entries.is_empty());
    let p = shared_projection(&store);
    assert_eq!(counts(&p).3, 0, "{:?}", p.entities);
    for id in [&fresh, &vb, &va] {
        assert_eq!(p.entities.get(&(Kind::Memo, id.clone())), Some(&EntityState::Deleted), "{id}");
    }
    {
        let st = store.lock().unwrap();
        assert_eq!(count_type(&st, Kind::Memo, &fresh, "delete"), 1);
        assert_eq!(count_type(&st, Kind::Memo, &vb, "delete"), 1);
        assert!(op_sigs(&st).iter().all(|s| s.1 != "a" && s.1 != "b"));
    }
    assert_eq!(memo_count(&mirror(&[], &p, 0)), 0);

    // Without a sink (sync off) the wipe still saves [] once under the lock
    // and writes no op.
    let (_sink, store, h, ..) = rebind_chain();
    let ops = op_count(&store.lock().unwrap());
    let mut locked = None;
    let mut saves = 0;
    wipe_entries(&h, None, |e, _| {
        locked = Some(h.state.try_lock().is_err());
        assert!(e.is_empty());
        saves += 1;
    })
    .unwrap();
    assert_eq!((locked, saves), (Some(true), 1));
    assert_eq!(op_count(&store.lock().unwrap()), ops);
}

/// AD-A4 under contention: wipes race memo creates, edits and pins on four
/// threads. Every save runs under the clipboard lock, so the last save is
/// the final memory state and the store's live memos match it exactly.
#[test]
fn rt107_concurrent_wipes_and_memo_commands_leave_the_last_save_equal_to_memory_and_store() {
    let (sink, store, entries) = sink_world();
    let sink: Arc<dyn MemoSink> = Arc::new(sink);
    let h = handle_with(entries);
    let file: Arc<Mutex<Vec<Vec<ClipEntry>>>> = Arc::new(Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let (h, sink, file) = (h.clone(), sink.clone(), file.clone());
            std::thread::spawn(move || {
                for i in 0..24usize {
                    let s: Option<&dyn MemoSink> = Some(sink.as_ref());
                    let save = |e: &[ClipEntry], _: usize| file.lock().unwrap().push(e.to_vec());
                    let first_memo = || {
                        h.state.lock().unwrap().entries.iter().find(|e| e.kind == EntryKind::Memo).map(|e| e.id.clone())
                    };
                    match (t + i) % 4 {
                        0 => wipe_entries(&h, s, save).unwrap(),
                        1 => {
                            create_memo_in(&h, s, &format!("t{t} i{i}"), i as i64, save).unwrap();
                        }
                        2 => {
                            // A concurrent wipe may remove the memo first: Err.
                            if let Some(id) = first_memo() {
                                let _ = toggle_pin_in(&h, s, &id, save);
                            }
                        }
                        _ => {
                            if let Some(id) = first_memo() {
                                let _ = update_memo_in(&h, s, &id, &format!("edit t{t} i{i}"), i as i64, save);
                            }
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let now = h.state.lock().unwrap().entries.clone();
    let file = file.lock().unwrap();
    assert_eq!(file.last(), Some(&now), "the last save is not the final memory state");
    let p = shared_projection(&store);
    assert_eq!(counts(&p).3, memo_count(&now));
    assert_eq!(memo_view(&mirror(&now, &p, 0)), memo_view(&now), "store and clipboard disagree");
}
