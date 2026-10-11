//! G2 red-team tests for device identity, clone detection, rotation planning,
//! attempted-upload settling, and keychain isolation. Written from the approved
//! plan (§4.1, §4.2.1, §4.3, §4.7, AC 16/16b/16c/17/25, Final Amendment W-1,
//! leader amendment W-2) rather than from the implementation: expected values
//! are derived here independently, and the fakes model the provider behaviours
//! the plan names.
//!
//! Nothing here touches the real keychain or this machine's hardware ids:
//! identities live in `MemoryStore`, fingerprints are fixed strings.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::identity::{
    advance_generation, detect, genesis_chain_head, load_identity, rotate, save_identity, DeviceIdentity,
    IdentityCheck, LocalIdentity, Observation, RemoteRecord, RemoteSegment, SeqRange, Trigger,
};
use super::publish::{
    extra_self_copies, settle_attempted, Outcome, Pause, PutOutcome, RemoteObject, SegmentRemote, Settled,
    TransportError, Verdict, RECHECK_DELAYS,
};
use super::rotation::{
    plan_rotation, Frontier, OpHashes, OwnOp, RotationError, RotationInput, RotationPlan, UploadState,
};
use super::secrets::{read_secret, write_secret, CloudProvider, MemoryStore, SecretError, SyncSecret};

const VAULT: &str = "0b5e2a8c-3d6f-4f1a-9c2e-7a4b1d9e8f60";
const FP_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FP_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

// ---------------------------------------------------------------------------
// §4.2.1 detection: T1–T8
// ---------------------------------------------------------------------------

fn keychain(device: &str, generation: u64) -> DeviceIdentity {
    DeviceIdentity {
        v: 1,
        vault_id: VAULT.to_owned(),
        device_id: device.to_owned(),
        install_fp: Some(FP_A.to_owned()),
        generation,
    }
}

fn database(device: &str, generation: u64, sealed_seq: u64) -> LocalIdentity {
    LocalIdentity {
        device_id: device.to_owned(),
        install_fp: Some(FP_A.to_owned()),
        generation,
        sealed_seq,
        chain_head: [0x11; 32],
    }
}

fn observe<'a>(keychain: &'a DeviceIdentity, local: &'a LocalIdentity) -> Observation<'a> {
    Observation {
        keychain: Some(keychain),
        local: Some(local),
        current_install_fp: Some(FP_A),
        ..Observation::default()
    }
}

fn healthy() -> IdentityCheck {
    IdentityCheck::Healthy { adopt_generation: None }
}

/// T3: the keychain may be equal to or exactly one seal ahead of SQLite
/// (keychain-first write); behind, or two or more ahead, is a rollback.
#[test]
fn g2_t3_generation_gap_boundaries() {
    let cases: [(u64, u64, IdentityCheck); 9] = [
        (4, 4, healthy()),
        (5, 4, IdentityCheck::Healthy { adopt_generation: Some(5) }),
        (6, 4, IdentityCheck::Rotate(vec![Trigger::T3PartialRollback])),
        (3, 4, IdentityCheck::Rotate(vec![Trigger::T3PartialRollback])),
        (0, 0, healthy()),
        (1, 0, IdentityCheck::Healthy { adopt_generation: Some(1) }),
        (0, u64::MAX, IdentityCheck::Rotate(vec![Trigger::T3PartialRollback])),
        (u64::MAX, u64::MAX - 1, IdentityCheck::Healthy { adopt_generation: Some(u64::MAX) }),
        (u64::MAX, 0, IdentityCheck::Rotate(vec![Trigger::T3PartialRollback])),
    ];
    for (kg, lg, expected) in cases {
        let (k, l) = (keychain("dev-a", kg), database("dev-a", lg, 9));
        assert_eq!(detect(&observe(&k, &l)), expected, "keychain {kg} vs db {lg}");
    }
    // A one-ahead keychain is only adopted when nothing else fired.
    let (k, l) = (keychain("dev-a", 5), database("dev-a", 4, 9));
    let ahead = RemoteRecord { last_seq: 10, chain_head: [0x11; 32] };
    assert_eq!(
        detect(&Observation { remote_record: Some(ahead), ..observe(&k, &l) }),
        IdentityCheck::Rotate(vec![Trigger::T4FullRollback])
    );
}

/// T4: remote `last_seq` above `sealed_seq`, or equal with another chain head.
#[test]
fn g2_t4_remote_high_water_boundaries() {
    let (k, l) = (keychain("dev-a", 4), database("dev-a", 4, 9));
    let t4 = IdentityCheck::Rotate(vec![Trigger::T4FullRollback]);
    let cases = [
        (RemoteRecord { last_seq: 9, chain_head: [0x11; 32] }, healthy()),
        (RemoteRecord { last_seq: 9, chain_head: [0x12; 32] }, t4.clone()),
        (RemoteRecord { last_seq: 10, chain_head: [0x11; 32] }, t4.clone()),
        (RemoteRecord { last_seq: u64::MAX, chain_head: [0x11; 32] }, t4.clone()),
        // The rec is rewritten after the seal, so a lagging rec is normal.
        (RemoteRecord { last_seq: 8, chain_head: [0x99; 32] }, healthy()),
        (RemoteRecord { last_seq: 0, chain_head: [0; 32] }, healthy()),
    ];
    for (remote, expected) in cases {
        assert_eq!(detect(&Observation { remote_record: Some(remote), ..observe(&k, &l) }), expected, "{remote:?}");
    }
    let (k, top) = (keychain("dev-a", 4), database("dev-a", 4, u64::MAX));
    let same = RemoteRecord { last_seq: u64::MAX, chain_head: [0x11; 32] };
    assert_eq!(detect(&Observation { remote_record: Some(same), ..observe(&k, &top) }), healthy());
}

fn seg(first: u64, last: u64, digest: u8) -> RemoteSegment {
    RemoteSegment { range: SeqRange { first, last }, digest: [digest; 32] }
}

/// T5: any listed range we never sealed, any overlap, or one range listed
/// with two different contents. A byte-identical copy of a sealed range (a
/// Drive resend that raced a slow listing) is ours, not a clone.
#[test]
fn g2_t5_remote_range_shapes() {
    let (k, l) = (keychain("dev-a", 4), database("dev-a", 4, 9));
    let sealed = [SeqRange { first: 1, last: 5 }, SeqRange { first: 6, last: 9 }];
    let t5 = IdentityCheck::Rotate(vec![Trigger::T5SameIdentityClone]);
    let cases: Vec<(Vec<RemoteSegment>, IdentityCheck)> = vec![
        (vec![], healthy()),
        (vec![seg(1, 5, 1)], healthy()),
        (vec![seg(1, 5, 1), seg(6, 9, 2)], healthy()),
        (vec![seg(6, 9, 2), seg(1, 5, 1)], healthy()),
        // Identical-digest duplicates (pass-1 expected T5; amended).
        (vec![seg(6, 9, 2), seg(6, 9, 2)], healthy()),
        (vec![seg(1, 5, 1), seg(6, 9, 2), seg(1, 5, 1)], healthy()),
        (vec![seg(1, 5, 1), seg(1, 5, 1), seg(1, 5, 1), seg(6, 9, 2), seg(6, 9, 2)], healthy()),
        // Different digests under one range: a clone wrote it.
        (vec![seg(6, 9, 2), seg(6, 9, 3)], t5.clone()),
        (vec![seg(1, 5, 1), seg(6, 9, 2), seg(1, 5, 7)], t5.clone()),
        (vec![seg(1, 5, 1), seg(1, 5, 1), seg(1, 5, 2)], t5.clone()),
        (vec![seg(1, 5, 2), seg(1, 5, 1), seg(1, 5, 2)], t5.clone()),
        // Ranges we never sealed fire even as identical copies.
        (vec![seg(10, 12, 1)], t5.clone()),
        (vec![seg(10, 12, 1), seg(10, 12, 1)], t5.clone()),
        (vec![seg(4, 7, 1)], t5.clone()),
        (vec![seg(2, 3, 1)], t5.clone()),
        (vec![seg(1, 9, 1)], t5.clone()),
        (vec![seg(u64::MAX, u64::MAX, 1)], t5.clone()),
    ];
    for (remote, expected) in cases {
        let observation = Observation { local_segments: &sealed, remote_segments: &remote, ..observe(&k, &l) };
        assert_eq!(detect(&observation), expected, "{remote:?}");
    }
}

/// T2 with missing fingerprints: an unreadable current fp disables T2 only;
/// a stored fp that disagrees with a readable current one always fires, and
/// so does a stored copy without any fp (amended: it cannot vouch).
#[test]
fn g2_t2_missing_and_none_fingerprints() {
    let k = keychain("dev-a", 4);
    let l = database("dev-a", 4, 9);
    let t2 = IdentityCheck::Rotate(vec![Trigger::T2InstallChanged]);

    assert_eq!(detect(&Observation { current_install_fp: None, ..observe(&k, &l) }), healthy());
    assert_eq!(detect(&Observation { current_install_fp: Some(FP_B), ..observe(&k, &l) }), t2);

    let k_none = DeviceIdentity { install_fp: None, ..k.clone() };
    let l_none = LocalIdentity { install_fp: None, ..l.clone() };
    for current in [FP_A, FP_B] {
        for (kc, lc) in [(&k_none, &l), (&k, &l_none), (&k_none, &l_none)] {
            let observation = Observation { current_install_fp: Some(current), ..observe(kc, lc) };
            assert_eq!(detect(&observation), t2, "current {current}, stored {:?}/{:?}", kc.install_fp, lc.install_fp);
        }
    }
    // An unreadable current fp never fires T2, whatever is stored.
    for (kc, lc) in [(&k_none, &l), (&k, &l_none), (&k_none, &l_none)] {
        assert_eq!(detect(&Observation { current_install_fp: None, ..observe(kc, lc) }), healthy());
    }
    // Without T2 the other triggers still guard the copy.
    let rolled_back = database("dev-a", 4, 3);
    let remote = RemoteRecord { last_seq: 9, chain_head: [0x11; 32] };
    assert_eq!(
        detect(&Observation { current_install_fp: None, remote_record: Some(remote), ..observe(&k, &rolled_back) }),
        IdentityCheck::Rotate(vec![Trigger::T4FullRollback])
    );
}

/// Every fault at once reports every trigger, in order; a missing copy on one
/// side still rotates together with the event triggers.
#[test]
fn g2_t1_t8_combined_faults() {
    let k = DeviceIdentity { install_fp: Some(FP_A.to_owned()), ..keychain("dev-b", 7) };
    let l = database("dev-a", 4, 9);
    let sealed = [SeqRange { first: 1, last: 9 }];
    let remote_segments = [seg(1, 9, 1), seg(1, 9, 2)];
    let everything = Observation {
        keychain: Some(&k),
        local: Some(&l),
        current_install_fp: Some(FP_B),
        remote_record: Some(RemoteRecord { last_seq: 12, chain_head: [0; 32] }),
        local_segments: &sealed,
        remote_segments: &remote_segments,
        put_create_conflict: true,
        checkpoint_restored: true,
        vault_switched: true,
    };
    assert_eq!(
        detect(&everything),
        IdentityCheck::Rotate(vec![
            Trigger::T1DeviceMismatch,
            Trigger::T2InstallChanged,
            Trigger::T3PartialRollback,
            Trigger::T4FullRollback,
            Trigger::T5SameIdentityClone,
            Trigger::T6NameTaken,
            Trigger::T7CheckpointRestore,
            Trigger::T8VaultSwitch,
        ])
    );
    let lost_keychain = Observation { keychain: None, ..everything };
    match detect(&lost_keychain) {
        IdentityCheck::Rotate(triggers) => {
            assert_eq!(triggers.first(), Some(&Trigger::T1DeviceMismatch));
            for event in [Trigger::T6NameTaken, Trigger::T7CheckpointRestore, Trigger::T8VaultSwitch] {
                assert!(triggers.contains(&event), "{event:?}");
            }
        }
        other => panic!("a missing keychain copy must rotate, got {other:?}"),
    }
    let lost_db = Observation { keychain: Some(&k), local: None, ..Observation::default() };
    assert_eq!(detect(&lost_db), IdentityCheck::Rotate(vec![Trigger::T1DeviceMismatch]));
    // Each event trigger alone, on an otherwise healthy identity.
    let (k, l) = (keychain("dev-a", 4), database("dev-a", 4, 9));
    for (observation, trigger) in [
        (Observation { put_create_conflict: true, ..observe(&k, &l) }, Trigger::T6NameTaken),
        (Observation { checkpoint_restored: true, ..observe(&k, &l) }, Trigger::T7CheckpointRestore),
        (Observation { vault_switched: true, ..observe(&k, &l) }, Trigger::T8VaultSwitch),
    ] {
        assert_eq!(detect(&observation), IdentityCheck::Rotate(vec![trigger]));
    }
}

// ---------------------------------------------------------------------------
// AC 17: keychain-first writes converge after a crash at any step
// ---------------------------------------------------------------------------

#[test]
fn g2_ac17_seal_crash_points_converge() {
    let store = MemoryStore::default();
    let identity = keychain("dev-a", 4);
    save_identity(&store, &identity).unwrap();
    let mut db = database("dev-a", 4, 9);

    // Crash before the keychain write: nothing moved anywhere.
    store.fail_next_sets(1);
    assert!(advance_generation(&store, &identity).is_err());
    let stored = load_identity(&store, VAULT).unwrap().unwrap();
    assert_eq!(stored, identity);
    assert_eq!(detect(&observe(&stored, &db)), healthy());

    // Crash after the keychain write, before the SQLite commit: healed.
    let next = advance_generation(&store, &stored).unwrap();
    let stored = load_identity(&store, VAULT).unwrap().unwrap();
    assert_eq!(stored, next);
    assert_eq!(detect(&observe(&stored, &db)), IdentityCheck::Healthy { adopt_generation: Some(5) });
    db.generation = 5;
    assert_eq!(detect(&observe(&stored, &db)), healthy());

    // Sealing again without adopting a crash leaves a gap of two: T3.
    let once = advance_generation(&store, &stored).unwrap();
    let twice = advance_generation(&store, &once).unwrap();
    assert_eq!(detect(&observe(&twice, &db)), IdentityCheck::Rotate(vec![Trigger::T3PartialRollback]));
}

#[test]
fn g2_ac17_rotation_crash_loop_converges_to_one_new_id() {
    let store = MemoryStore::default();
    let original = keychain("dev-a", 4);
    save_identity(&store, &original).unwrap();
    let db = database("dev-a", 4, 9);
    let mut ids = BTreeSet::from([original.device_id.clone()]);

    // R4 keychain writes fail repeatedly: the keychain never moves and the
    // cause (T2) keeps the rotation pending.
    for failures in 1..=3 {
        store.fail_next_sets(failures);
        for _ in 0..failures {
            assert!(matches!(rotate(&store, VAULT, Some(FP_B.to_owned())), Err(SecretError::Backend(_))));
            assert_eq!(load_identity(&store, VAULT).unwrap().as_ref(), Some(&original));
        }
        let stored = load_identity(&store, VAULT).unwrap().unwrap();
        let observation = Observation { current_install_fp: Some(FP_B), ..observe(&stored, &db) };
        assert_eq!(detect(&observation), IdentityCheck::Rotate(vec![Trigger::T2InstallChanged]));
    }

    // Keychain written, SQLite crash, three times in a row: T1 every time,
    // and every abandoned id is new.
    for _ in 0..3 {
        let abandoned = rotate(&store, VAULT, Some(FP_B.to_owned())).unwrap();
        assert!(ids.insert(abandoned.device_id.clone()), "a rotation reused {}", abandoned.device_id);
        let stored = load_identity(&store, VAULT).unwrap().unwrap();
        let observation = Observation { current_install_fp: Some(FP_B), ..observe(&stored, &db) };
        match detect(&observation) {
            IdentityCheck::Rotate(triggers) => assert!(triggers.contains(&Trigger::T1DeviceMismatch)),
            other => panic!("an uncommitted rotation must rotate again, got {other:?}"),
        }
    }

    // Damaged entries of any kind read as absent → T1, never healthy and
    // never a hard error (pass-1 watch W-G2-3: non-base64 used to be Corrupt).
    let account = format!("sync:device:{VAULT}");
    let b64 = |bytes: &[u8]| data_encoding::BASE64.encode(bytes);
    let other_vault = serde_json::to_vec(&DeviceIdentity {
        vault_id: "1b5e2a8c-3d6f-4f1a-9c2e-7a4b1d9e8f60".to_owned(),
        ..keychain("dev-a", 4)
    })
    .unwrap();
    let damaged = [
        b64(b"{\"v\":1"),
        "%%%".to_owned(),
        "not base64 at all!".to_owned(),
        String::new(),
        b64(&[0xFF, 0xFE, 0x00]),
        b64(&other_vault),
        b64(br#"{"v":2,"vault_id":"0b5e2a8c-3d6f-4f1a-9c2e-7a4b1d9e8f60","device_id":"dev-a","install_fp":null,"generation":0}"#),
        b64(br#"{"v":1,"vault_id":"0b5e2a8c-3d6f-4f1a-9c2e-7a4b1d9e8f60","device_id":"dev-a","install_fp":null,"generation":0,"extra":1}"#),
    ];
    for raw in damaged {
        store.put_raw(&account, &raw);
        assert_eq!(load_identity(&store, VAULT), Ok(None), "{raw:?}");
        assert_eq!(
            detect(&Observation { keychain: None, local: Some(&db), ..Observation::default() }),
            IdentityCheck::Rotate(vec![Trigger::T1DeviceMismatch]),
            "{raw:?}"
        );
    }

    let settled = rotate(&store, VAULT, Some(FP_B.to_owned())).unwrap();
    assert!(ids.insert(settled.device_id.clone()));
    let committed = LocalIdentity {
        device_id: settled.device_id.clone(),
        install_fp: settled.install_fp.clone(),
        generation: settled.generation,
        sealed_seq: 0,
        chain_head: genesis_chain_head(VAULT, &settled.device_id),
    };
    let stored = load_identity(&store, VAULT).unwrap().unwrap();
    assert_eq!(stored, settled);
    assert_eq!(stored.generation, 0, "R4 resets the generation");
    let observation = Observation { current_install_fp: Some(FP_B), ..observe(&stored, &committed) };
    assert_eq!(detect(&observation), healthy());
}

// ---------------------------------------------------------------------------
// §4.2.1 R5–R7 + W-2: rotation planning
// ---------------------------------------------------------------------------

const A: &str = "dev-a";
const A2: &str = "dev-a2";

/// §4.3 shape: content covers the op id, entity, type, payload with refs
/// (`base_op_id`/`parent_id`) and HLC; body adds origin, ctx and obs.
fn hashes(op: &OwnOp) -> OpHashes {
    let content: [u8; 32] = Sha256::digest(format!(
        "{}|{}|{}|{}|{:?}|{:?}|{}|{}",
        op.op_id, op.entity_kind, op.entity_id, op.op_type, op.payload, op.refs, op.hlc_wall, op.hlc_ctr
    ))
    .into();
    let body: [u8; 32] =
        Sha256::digest(format!("{content:?}|{}|{:?}|{:?}", op.origin_device, op.ctx, op.obs)).into();
    OpHashes { content, body }
}

fn sealed(mut op: OwnOp) -> OwnOp {
    let h = hashes(&op);
    op.content_hash = h.content;
    op.body_hash = h.body;
    op
}

fn own(id: &str, seq: u64, state: UploadState, entity: &str, seen: Option<u64>) -> OwnOp {
    sealed(OwnOp {
        op_id: id.to_owned(),
        origin_device: A.to_owned(),
        seq,
        entity_kind: "memo".to_owned(),
        entity_id: entity.to_owned(),
        op_type: "edit".to_owned(),
        payload: format!("payload of {id}").into_bytes(),
        hlc_wall: 1_700_000_000_000u64.wrapping_add(seq),
        hlc_ctr: (seq % 7) as u32,
        ctx: seen.map(|s| Frontier::from([(A.to_owned(), s)])).unwrap_or_default(),
        upload_state: state,
        ..OwnOp::default()
    })
}

fn with_links(mut op: OwnOp, refs: &[&str], obs: &[&str]) -> OwnOp {
    op.refs = refs.iter().map(|r| (*r).to_owned()).collect();
    op.obs = obs.iter().map(|o| (*o).to_owned()).collect();
    sealed(op)
}

fn settled_as(mut op: OwnOp, state: UploadState) -> OwnOp {
    op.upload_state = state;
    op
}

type Remote = BTreeMap<u64, Vec<[u8; 32]>>;
type Compacted = BTreeMap<u64, [u8; 32]>;

fn remote_of(ops: &[&OwnOp]) -> Remote {
    ops.iter().map(|op| (op.seq, vec![op.body_hash])).collect()
}

fn no_compaction() -> Compacted {
    Compacted::new()
}

/// Runs the planner with a counting id source; returns the plan and how many
/// fresh op ids were drawn.
fn run(ops: &[OwnOp], remote: &Remote, local_compacted: &Compacted) -> (Result<RotationPlan, RotationError>, usize) {
    let drawn = Cell::new(0usize);
    let input = RotationInput { old_device: A, new_device: A2, ops, remote, local_compacted };
    let plan = plan_rotation(
        &input,
        || {
            drawn.set(drawn.get() + 1);
            format!("fresh-{}", drawn.get())
        },
        hashes,
    );
    (plan, drawn.get())
}

fn moved<'a>(plan: &'a RotationPlan, id: &str) -> &'a OwnOp {
    plan.rewritten.iter().map(|r| &r.op).find(|op| op.op_id == id).unwrap_or_else(|| panic!("{id} was not moved"))
}

fn rewritten_ids(plan: &RotationPlan) -> Vec<&str> {
    plan.rewritten.iter().map(|r| r.op.op_id.as_str()).collect()
}

/// A merged op as a reader holds it (§4.4): one or more dots (I6 aliases).
struct Logical {
    op_id: String,
    dots: Vec<(String, u64)>,
    ctx: Frontier,
    obs: BTreeSet<String>,
    memo: String,
    delete: bool,
}

impl Logical {
    fn of(op: &OwnOp, delete: bool) -> Self {
        Self {
            op_id: op.op_id.clone(),
            dots: vec![(op.origin_device.clone(), op.seq)],
            ctx: op.ctx.clone(),
            obs: op.obs.clone(),
            memo: op.entity_id.clone(),
            delete,
        }
    }
}

/// §4.4: Y saw X ⇔ some dot (d, s) of X has `ctx_Y[d] ≥ s`, or X ∈ obs_Y.
fn saw(y: &Logical, x: &Logical) -> bool {
    x.dots.iter().any(|(d, s)| y.ctx.get(d).is_some_and(|seen| seen >= s)) || y.obs.contains(&x.op_id)
}

/// §4.4 edit vs delete: deleted only when every content op is seen by a delete.
fn memo_deleted(ops: &[Logical], memo: &str) -> bool {
    let deletes: Vec<&Logical> = ops.iter().filter(|op| op.memo == memo && op.delete).collect();
    !deletes.is_empty()
        && ops
            .iter()
            .filter(|op| op.memo == memo && !op.delete)
            .all(|content| deletes.iter().any(|d| saw(d, content)))
}

/// AC 16b (N-1) in three-device form. A and its clone A′ share seq 1..N
/// (N = 3; a1 creates memo M). A′ writes x at (A, 4) and z = delete M at
/// (A, 5) with ctx[A] = 4; A writes y = edit M at (A, 4) and uploads it.
/// A′ rotates (T2) before uploading. Readers: A, A′ (→ A′₂), and R.
#[test]
fn g2_n1_three_device_clone_delete_cannot_cover_the_original_edit() {
    let a1 = own("a1", 1, UploadState::Published, "M", None);
    let a2 = own("a2", 2, UploadState::Published, "K", Some(1));
    let a3 = own("a3", 3, UploadState::Published, "K", Some(2));
    let x = own("x", 4, UploadState::Never, "X", Some(3));
    let z = own("z", 5, UploadState::Never, "M", Some(4));
    let y = own("y", 4, UploadState::Published, "M", Some(3)); // the original's edit

    let remote = remote_of(&[&a1, &a2, &a3, &y]);
    let clone_ops = vec![a1.clone(), a2.clone(), a3.clone(), x.clone(), z.clone()];
    let (plan, drawn) = run(&clone_ops, &remote, &no_compaction());
    let plan = plan.unwrap();
    assert_eq!(drawn, 0);

    // S = 3: seq 4 holds y's body, not x's.
    let s = plan.verified_prefix;
    assert_eq!(s, 3);
    let (x2, z2) = (moved(&plan, "x"), moved(&plan, "z"));
    assert_eq!((x2.origin_device.as_str(), x2.seq), (A2, 1));
    assert_eq!((z2.origin_device.as_str(), z2.seq), (A2, 2));
    assert_eq!(plan.rewritten.iter().map(|r| r.source_seq).collect::<Vec<_>>(), [4, 5]);
    // Inclusion check computed here: z′.ctx[A] = min(4, S) = 3 ≤ S < y.seq = 4.
    let z_seen_a = *z2.ctx.get(A).unwrap();
    assert_eq!(z_seen_a, 3);
    assert!(z_seen_a <= s && z_seen_a < y.seq);
    assert_eq!(z2.ctx.get(A2), Some(&1), "z′ still sees x′");
    assert!(z2.obs.is_empty(), "A′ never held y, so obs cannot name it");

    // R7: A′ reads the retired stream from S + 1, and no local dot (A, 4) is
    // left to collide with y.
    assert_eq!(plan.retired.verified_prefix, 3);
    assert_eq!(plan.retired.applied_seq, 3);
    let moved_ids: BTreeSet<&str> = rewritten_ids(&plan).into_iter().collect();
    let mut clone_dots = BTreeSet::new();
    for op in &clone_ops {
        if !moved_ids.contains(op.op_id.as_str()) {
            assert!(clone_dots.insert((op.origin_device.clone(), op.seq)));
        }
    }
    for r in &plan.rewritten {
        assert!(clone_dots.insert((r.op.origin_device.clone(), r.op.seq)));
    }
    assert!(!clone_dots.contains(&(A.to_owned(), 4)));

    // Every device converges on the same op set and keeps M.
    let rotated = |ops: &[&OwnOp]| -> Vec<Logical> { ops.iter().map(|op| Logical::of(op, op.op_id == "z")).collect() };
    let at_a = rotated(&[&a1, &a2, &a3, &y, x2, z2]);
    let at_clone = rotated(&[&a1, &a2, &a3, x2, z2, &y]);
    let at_reader = rotated(&[x2, z2, &a1, &y, &a3, &a2]);
    for (name, view) in [("A", &at_a), ("A′", &at_clone), ("R", &at_reader)] {
        assert!(!memo_deleted(view, "M"), "{name} must keep memo M");
        let ids: BTreeSet<&str> = view.iter().map(|op| op.op_id.as_str()).collect();
        assert_eq!(ids, BTreeSet::from(["a1", "a2", "a3", "x", "y", "z"]), "{name}");
    }
    // Counterfactual: without the cut, z at (A, 5) with ctx[A] = 4 covers y.
    let unrotated: Vec<Logical> = [&a1, &a2, &a3, &y, &x, &z].iter().map(|op| Logical::of(op, op.op_id == "z")).collect();
    assert!(memo_deleted(&unrotated, "M"), "the counterexample must be real");
}

/// W-2 N-1 variant: the clone's x went out with an unknown outcome
/// (`attempted`) while the original's y owns dot (A, 4). Rotation must wait
/// for R3; once R3 settles x, z′.ctx[A] ≤ S and M survives everywhere.
#[tokio::test]
async fn g2_w2_n1_attempted_clone_op_waits_for_r3() {
    let a1 = own("a1", 1, UploadState::Published, "M", None);
    let a2 = own("a2", 2, UploadState::Published, "K", Some(1));
    let a3 = own("a3", 3, UploadState::Published, "K", Some(2));
    let x = own("x", 4, UploadState::Attempted, "X", Some(3));
    let z = own("z", 5, UploadState::Never, "M", Some(4));
    let y = own("y", 4, UploadState::Published, "M", Some(3));
    let clone_ops = vec![a1.clone(), a2.clone(), a3.clone(), x.clone(), z.clone()];

    let remote = remote_of(&[&a1, &a2, &a3, &y]);
    let (plan, drawn) = run(&clone_ops, &remote, &no_compaction());
    assert_eq!(plan, Err(RotationError::UnsettledAttempts(vec!["x".to_owned()])));
    assert_eq!(drawn, 0, "nothing is drawn for a refused rotation");

    // R3 on a create-only store: y's segment already holds x's key.
    let store = FakeStore::new(true, 0);
    store.landed(THEIRS, 0);
    let (settled, _) = cycle(&store).await;
    assert_eq!(settled.outcome, Outcome::Rejected);
    assert_eq!(store.puts(), 0);
    let mut ops = clone_ops.clone();
    ops[3] = settled_as(x.clone(), UploadState::Rejected);
    let (plan, drawn) = run(&ops, &remote, &no_compaction());
    let plan = plan.unwrap();
    let s = plan.verified_prefix;
    assert_eq!((s, drawn), (3, 1));
    assert_eq!(rewritten_ids(&plan), ["fresh-1", "z"]);
    assert_eq!(plan.quarantined, vec![ops[3].clone()]);
    let (x2, z2) = (moved(&plan, "fresh-1"), moved(&plan, "z"));
    assert_eq!((x2.seq, z2.seq), (1, 2));
    assert_eq!(z2.ctx, Frontier::from([(A.to_owned(), 3), (A2.to_owned(), 1)]));
    assert!(z2.ctx[A] <= s && z2.ctx[A] < y.seq);
    assert_eq!(plan.retired.applied_seq, 3);
    let view: Vec<Logical> = [&a1, &a2, &a3, &y, x2, z2].iter().map(|op| Logical::of(op, op.op_id == "z")).collect();
    assert!(!memo_deleted(&view, "M"));

    // R3 on Drive: our bytes are resent next to y's, a fork; never Rejected.
    let store = FakeStore::new(false, 0);
    store.landed(THEIRS, 0);
    let (settled, _) = cycle(&store).await;
    assert_eq!(settled.outcome, Outcome::PublishedWithFork);
    assert_eq!((store.puts(), store.count(OURS)), (1, 1));
    let mut ops = clone_ops.clone();
    ops[3] = settled_as(x.clone(), UploadState::Published);
    let mut forked = remote.clone();
    forked.insert(4, vec![y.body_hash, x.body_hash]);
    let (plan, drawn) = run(&ops, &forked, &no_compaction());
    let plan = plan.unwrap();
    let s = plan.verified_prefix;
    assert_eq!((s, drawn), (3, 0));
    assert_eq!(rewritten_ids(&plan), ["z"]);
    let z2 = moved(&plan, "z");
    assert_eq!(z2.ctx, Frontier::from([(A.to_owned(), 3)]), "x stays on (A, 4); ctx[A] is cut to S");
    let view: Vec<Logical> = [&a1, &a2, &a3, &y, &ops[3], z2].iter().map(|op| Logical::of(op, op.op_id == "z")).collect();
    assert!(!memo_deleted(&view, "M"));
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `S` from §4.2.1 as amended: walk seqs from 1 while the remote holds one
/// distinct content for the seq and it equals the local (or compacted) body.
fn expected_prefix(ops: &[OwnOp], remote: &Remote, compacted: &Compacted) -> u64 {
    let by_seq: BTreeMap<u64, &OwnOp> = ops.iter().map(|op| (op.seq, op)).collect();
    let mut s = 0;
    loop {
        let next = s + 1;
        let distinct: BTreeSet<[u8; 32]> = remote.get(&next).into_iter().flatten().copied().collect();
        let local = by_seq.get(&next).map(|op| op.body_hash).or_else(|| compacted.get(&next).copied());
        match (distinct.len(), local) {
            (1, Some(hash)) if distinct.contains(&hash) => s = next,
            _ => return s,
        }
    }
}

/// I1/I2/I3, R5/R6 bookkeeping and W-2 over random streams with refs, obs,
/// foreign ctx entries, compaction (with corrupt compacted hashes), duplicate
/// and foreign remote objects. Half of the rounds settle every attempted op
/// above S first (R3), half do not. Expected values are recomputed from the
/// plan text, not the planner.
#[test]
fn g2_random_streams_hold_i1_i2_i3_r5_r6_and_w2() {
    const STATES: [UploadState; 4] =
        [UploadState::Never, UploadState::Attempted, UploadState::Published, UploadState::Rejected];
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let (mut moved_total, mut reissued_total, mut refused_total, mut planned_total) = (0usize, 0usize, 0usize, 0usize);
    let mut promoted_attempted = 0usize;
    for round in 0..3000 {
        let n = rng.below(16) + 1;
        let mut all = Vec::new();
        for seq in 1..=n {
            let state = STATES[rng.below(4) as usize];
            let entity = ["M", "N", "O"][rng.below(3) as usize];
            let mut op = own(&format!("op{seq}"), seq, state, entity, None);
            if seq > 1 && rng.below(4) != 0 {
                let seen = rng.below(seq);
                if seen > 0 {
                    op.ctx.insert(A.to_owned(), seen);
                }
            }
            if rng.below(3) == 0 {
                op.ctx.insert("dev-b".to_owned(), rng.below(50) + 1);
            }
            if seq > 1 {
                for _ in 0..rng.below(3) {
                    op.refs.push(format!("op{}", rng.below(seq - 1) + 1));
                }
                if rng.below(3) == 0 {
                    op.obs.insert(format!("op{}", rng.below(seq - 1) + 1));
                }
            }
            all.push(sealed(op));
        }
        let take = rng.below(n + 1) as usize;
        let mut remote = Remote::new();
        for op in all.iter().take(take) {
            let mut objects = vec![op.body_hash];
            match rng.below(10) {
                0 => objects[0] = [0xEE; 32],
                1 => objects.push(op.body_hash),
                2 => objects.push([0xEF; 32]),
                _ => {}
            }
            remote.insert(op.seq, objects);
        }
        if rng.below(5) == 0 {
            remote.insert(n + 1 + rng.below(3), vec![[0x55; 32]]);
        }
        let compacted = rng.below(4).min(n);
        let mut local_compacted = Compacted::new();
        for op in all.iter().take(compacted as usize) {
            let hash = if rng.below(8) == 0 { [0xC0; 32] } else { op.body_hash };
            local_compacted.insert(op.seq, hash);
        }
        let mut ops: Vec<OwnOp> = all.iter().filter(|op| op.seq > compacted).cloned().collect();

        let s = expected_prefix(&ops, &remote, &local_compacted);
        if rng.below(2) == 0 {
            // R3 settled every attempted op above S (published or rejected).
            for op in ops.iter_mut().filter(|op| op.seq > s && op.upload_state == UploadState::Attempted) {
                op.upload_state = [UploadState::Published, UploadState::Rejected][rng.below(2) as usize];
            }
        }
        let unsettled: Vec<String> = ops
            .iter()
            .filter(|op| op.seq > s && op.upload_state == UploadState::Attempted)
            .map(|op| op.op_id.clone())
            .collect();

        let (plan, drawn) = run(&ops, &remote, &local_compacted);
        if !unsettled.is_empty() {
            // W-2: refused exactly when an attempted op lies above S.
            assert_eq!(plan, Err(RotationError::UnsettledAttempts(unsettled)), "round {round}");
            assert_eq!(drawn, 0, "round {round}: no id drawn for a refused plan");
            refused_total += 1;
            continue;
        }
        let plan = plan.unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        planned_total += 1;
        assert_eq!(plan.verified_prefix, s, "round {round}");

        let batch: Vec<&OwnOp> = ops
            .iter()
            .filter(|op| op.seq > s && matches!(op.upload_state, UploadState::Never | UploadState::Rejected))
            .collect();
        let rejected: Vec<&OwnOp> =
            batch.iter().copied().filter(|op| op.upload_state == UploadState::Rejected).collect();
        assert_eq!(drawn, rejected.len(), "round {round}: one fresh id per X op");
        let reissued_keys: Vec<&str> = plan.reissued.keys().map(String::as_str).collect();
        let mut rejected_ids: Vec<&str> = rejected.iter().map(|op| op.op_id.as_str()).collect();
        rejected_ids.sort_unstable();
        assert_eq!(reissued_keys, rejected_ids, "round {round}");
        let original_ids: BTreeSet<&str> = all.iter().map(|op| op.op_id.as_str()).collect();
        let fresh: BTreeSet<&str> = plan.reissued.values().map(String::as_str).collect();
        assert_eq!(fresh.len(), rejected.len());
        assert!(fresh.is_disjoint(&original_ids), "round {round}: reissued ids must be new");
        let remap = |id: &String| plan.reissued.get(id).cloned().unwrap_or_else(|| id.clone());

        // R6: the X ops are quarantined exactly as they were, in seq order.
        let expected_quarantine: Vec<OwnOp> = rejected.iter().map(|op| (*op).clone()).collect();
        assert_eq!(plan.quarantined, expected_quarantine, "round {round}");

        // I2: P above S and everything at or below S keep their dot; the
        // unpublished ones at or below S (attempted included) are promoted.
        let promoted: Vec<&str> = ops
            .iter()
            .filter(|op| op.seq <= s && op.upload_state != UploadState::Published)
            .map(|op| op.op_id.as_str())
            .collect();
        assert_eq!(plan.promoted, promoted, "round {round}");
        promoted_attempted +=
            ops.iter().filter(|op| op.seq <= s && op.upload_state == UploadState::Attempted).count();

        assert_eq!(plan.rewritten.len(), batch.len(), "round {round}");
        for (i, (rewrite, before)) in plan.rewritten.iter().zip(&batch).enumerate() {
            let next = &rewrite.op;
            let k = i as u64 + 1;
            assert_eq!(rewrite.source_seq, before.seq, "round {round}: source dot");
            assert_eq!(next.op_id, remap(&before.op_id), "round {round}: old-seq order");
            assert_eq!((next.origin_device.as_str(), next.seq), (A2, k));
            assert_eq!(next.upload_state, UploadState::Never);
            assert_eq!((&next.entity_kind, &next.entity_id), (&before.entity_kind, &before.entity_id));
            // R5-4: payload, type and HLC are kept.
            assert_eq!((&next.op_type, &next.payload), (&before.op_type, &before.payload));
            assert_eq!((next.hlc_wall, next.hlc_ctr), (before.hlc_wall, before.hlc_ctr));

            // R5-2/3: cut ctx[D] to S, name the batch prefix on D′, and keep
            // the same-entity published ops in (S, seen] by op id.
            let mut ctx = before.ctx.clone();
            ctx.remove(A);
            let mut obs: BTreeSet<String> = before.obs.iter().map(remap).collect();
            if let Some(&seen) = before.ctx.get(A) {
                if seen.min(s) > 0 {
                    ctx.insert(A.to_owned(), seen.min(s));
                }
                let latest = batch[..i].iter().filter(|op| op.seq <= seen).count() as u64;
                if latest > 0 {
                    ctx.insert(A2.to_owned(), latest);
                }
                obs.extend(
                    ops.iter()
                        .filter(|p| p.seq > s && p.seq <= seen && p.upload_state == UploadState::Published)
                        .filter(|p| p.entity_kind == before.entity_kind && p.entity_id == before.entity_id)
                        .map(|p| p.op_id.clone()),
                );
            }
            assert_eq!(next.ctx, ctx, "round {round}: ctx of {}", before.op_id);
            assert_eq!(next.obs, obs, "round {round}: obs of {}", before.op_id);
            assert_eq!(next.refs, before.refs.iter().map(remap).collect::<Vec<_>>());
            // I3: no moved op's ctx[old] exceeds S.
            assert!(next.ctx.get(A).is_none_or(|seen| *seen <= s), "round {round}: I3");
            assert!(next.ctx.get(A2).is_none_or(|seen| *seen < k));
            for old in plan.reissued.keys() {
                assert!(!next.refs.contains(old) && !next.obs.contains(old), "round {round}: dangling {old}");
            }
            // Hashes: body always recomputed; content kept unless the op id
            // or a ref changed.
            let rehashed = hashes(next);
            assert_eq!(next.body_hash, rehashed.body);
            assert_eq!(next.content_hash, rehashed.content);
            if next.op_id == before.op_id && next.refs == before.refs {
                assert_eq!(next.content_hash, before.content_hash, "round {round}: content of {}", before.op_id);
            }
        }

        // I1: every op this device keeps holds exactly one dot, and no two
        // ops share a dot.
        let moved_ids: BTreeSet<&str> = batch.iter().map(|op| op.op_id.as_str()).collect();
        let mut dot_of: BTreeMap<String, (String, u64)> = BTreeMap::new();
        let mut dots = BTreeSet::new();
        for op in ops.iter().filter(|op| !moved_ids.contains(op.op_id.as_str())) {
            assert!(dot_of.insert(op.op_id.clone(), (A.to_owned(), op.seq)).is_none());
            assert!(dots.insert((A.to_owned(), op.seq)));
        }
        for r in &plan.rewritten {
            assert!(dot_of.insert(r.op.op_id.clone(), (A2.to_owned(), r.op.seq)).is_none(), "round {round}: I1");
            assert!(dots.insert((A2.to_owned(), r.op.seq)));
        }
        assert_eq!(dot_of.len(), ops.len(), "round {round}");

        // R7 + W-2: applied_seq is S extended only through contiguous
        // published ops; nothing above S that is not published is covered.
        let mut applied = s;
        while ops.iter().any(|op| op.seq == applied + 1 && op.upload_state == UploadState::Published) {
            applied += 1;
        }
        assert_eq!((plan.retired.verified_prefix, plan.retired.applied_seq), (s, applied), "round {round}");
        for seq in s + 1..=plan.retired.applied_seq {
            let op = ops.iter().find(|op| op.seq == seq);
            assert!(
                op.is_some_and(|op| op.upload_state == UploadState::Published),
                "round {round}: applied_seq covers non-published seq {seq}"
            );
        }
        assert_eq!(plan.retired.device_id, A);

        moved_total += batch.len();
        reissued_total += rejected.len();
    }
    assert!(moved_total > 1000 && reissued_total > 300, "the generator must exercise R5 and R6");
    assert!(refused_total > 300 && planned_total > 1000, "refused {refused_total}, planned {planned_total}");
    assert!(promoted_attempted > 50, "attempted ops at or below S must be promoted: {promoted_attempted}");
}

/// W-2 boundaries: attempted at or below S is promoted; attempted anywhere
/// above S (adjacent, after published ops, or behind an unverifiable seq)
/// refuses the whole rotation and names every such op in seq order.
#[test]
fn g2_w2_unsettled_attempts_boundary() {
    let a1 = own("a1", 1, UploadState::Attempted, "M", None);
    let p2 = own("p2", 2, UploadState::Published, "M", Some(1));
    let a3 = own("a3", 3, UploadState::Attempted, "M", Some(2));
    let r4 = own("r4", 4, UploadState::Rejected, "M", Some(3));
    let a5 = own("a5", 5, UploadState::Attempted, "M", Some(4));
    let ops = vec![a1.clone(), p2.clone(), a3.clone(), r4.clone(), a5.clone()];

    // S = 3: a1 and a3 promoted; a5 above S refuses.
    let remote = remote_of(&[&a1, &p2, &a3]);
    let (plan, drawn) = run(&ops, &remote, &no_compaction());
    assert_eq!((plan, drawn), (Err(RotationError::UnsettledAttempts(vec!["a5".to_owned()])), 0));

    // S = 1: a3 and a5 both named, in seq order.
    let (plan, _) = run(&ops, &remote_of(&[&a1]), &no_compaction());
    assert_eq!(plan, Err(RotationError::UnsettledAttempts(vec!["a3".to_owned(), "a5".to_owned()])));

    // S = 0: every attempted op is unsettled, including seq 1.
    let (plan, _) = run(&ops, &Remote::new(), &no_compaction());
    assert_eq!(
        plan,
        Err(RotationError::UnsettledAttempts(vec!["a1".to_owned(), "a3".to_owned(), "a5".to_owned()]))
    );

    // Another object at seq 2 ends S at 1 even though p2 is published.
    let mut forked = remote_of(&[&a1, &p2, &a3]);
    forked.get_mut(&2).unwrap().push([0x99; 32]);
    let (plan, _) = run(&ops, &forked, &no_compaction());
    assert_eq!(plan, Err(RotationError::UnsettledAttempts(vec!["a3".to_owned(), "a5".to_owned()])));

    // S = 5 covers every attempted op: all promoted, nothing moved.
    let all = remote_of(&[&a1, &p2, &a3, &r4, &a5]);
    let (plan, drawn) = run(&ops, &all, &no_compaction());
    let plan = plan.unwrap();
    assert_eq!((plan.verified_prefix, drawn), (5, 0));
    assert_eq!(plan.promoted, ["a1", "a3", "r4", "a5"]);
    assert!(plan.rewritten.is_empty());
    assert_eq!(plan.retired.applied_seq, 5);

    // Settled a5 (published) above S = 3: applied_seq stops at the rejected r4.
    let settled = vec![a1, p2, a3, r4, settled_as(a5, UploadState::Published)];
    let (plan, drawn) = run(&settled, &remote, &no_compaction());
    let plan = plan.unwrap();
    assert_eq!((plan.verified_prefix, drawn, plan.retired.applied_seq), (3, 1, 3));
    assert_eq!(rewritten_ids(&plan), ["fresh-1"]);
}

/// R6: X ops are reissued under fresh ids; the originals are quarantined and
/// never rewritten; refs and obs in the batch follow the new ids; P ops that
/// mention X stay byte-identical. Amended (W-2): the attempted p6 above S
/// first refuses the rotation; the expectations then hold once R3 settled it.
#[test]
fn g2_r6_reissue_quarantine_and_reference_remap() {
    let p1 = own("p1", 1, UploadState::Published, "M", None);
    let x = own("x", 2, UploadState::Rejected, "M", Some(1));
    let u = with_links(own("u", 3, UploadState::Never, "M", Some(2)), &["x"], &["x"]);
    let y = with_links(own("y", 4, UploadState::Rejected, "N", Some(3)), &["u"], &[]);
    let w = with_links(own("w", 5, UploadState::Never, "M", Some(4)), &["y", "x"], &["y"]);
    let p6 = with_links(own("p6", 6, UploadState::Attempted, "M", Some(5)), &["x"], &[]);
    let mut ops = vec![p1.clone(), x.clone(), u.clone(), y.clone(), w.clone(), p6.clone()];
    let mut remote = remote_of(&[&p1]);
    remote.insert(2, vec![[0x42; 32]]);

    let (plan, drawn) = run(&ops, &remote, &no_compaction());
    assert_eq!((plan, drawn), (Err(RotationError::UnsettledAttempts(vec!["p6".to_owned()])), 0));

    // R3 found p6 published.
    ops[5] = settled_as(p6.clone(), UploadState::Published);
    let (plan, drawn) = run(&ops, &remote, &no_compaction());
    let plan = plan.unwrap();
    assert_eq!(drawn, 2);
    assert_eq!(plan.verified_prefix, 1);
    assert_eq!(
        plan.reissued,
        BTreeMap::from([("x".to_owned(), "fresh-1".to_owned()), ("y".to_owned(), "fresh-2".to_owned())])
    );
    assert_eq!(plan.quarantined, vec![x.clone(), y.clone()]);
    let ids = rewritten_ids(&plan);
    assert_eq!(ids, ["fresh-1", "u", "fresh-2", "w"]);
    assert_eq!(plan.rewritten.iter().map(|r| r.source_seq).collect::<Vec<_>>(), [2, 3, 4, 5]);

    let x2 = moved(&plan, "fresh-1");
    assert_eq!((x2.seq, x2.entity_id.as_str()), (1, "M"));
    assert_ne!(x2.content_hash, x.content_hash);
    let u2 = moved(&plan, "u");
    assert_eq!(u2.refs, ["fresh-1"]);
    assert_eq!(u2.obs, BTreeSet::from(["fresh-1".to_owned()]));
    assert_ne!(u2.content_hash, u.content_hash, "its base op id changed");
    let y2 = moved(&plan, "fresh-2");
    assert_eq!(y2.refs, ["u"], "U keeps its op id, so refs to it do not move");
    assert_eq!(y2.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 2)]));
    let w2 = moved(&plan, "w");
    assert_eq!(w2.refs, ["fresh-2", "fresh-1"]);
    assert_eq!(w2.obs, BTreeSet::from(["fresh-2".to_owned()]));
    assert_eq!(w2.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 3)]));
    for r in &plan.rewritten {
        assert_eq!(r.op.body_hash, hashes(&r.op).body);
    }
    // p6 is published: it stays on (A, 6) as it was, even though it names x.
    assert!(!ids.contains(&"p6"));
    assert!(!plan.promoted.contains(&"p6".to_owned()));
    assert_eq!(plan.retired.applied_seq, 1);

    // R3 found another object under p6's key on a create-only store: p6 is X
    // too, reissued with its ref to x following x's new id.
    ops[5] = settled_as(p6, UploadState::Rejected);
    let (plan, drawn) = run(&ops, &remote, &no_compaction());
    let plan = plan.unwrap();
    assert_eq!(drawn, 3);
    assert_eq!(rewritten_ids(&plan), ["fresh-1", "u", "fresh-2", "w", "fresh-3"]);
    let p6b = moved(&plan, "fresh-3");
    assert_eq!(p6b.refs, ["fresh-1"]);
    assert_eq!(p6b.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 4)]));
    assert_eq!(plan.retired.applied_seq, 1);
}

/// I6 / AC 16c: an op the remote already holds under the old dot with our
/// exact body is promoted, not moved; a moved op keeps its op id and
/// content hash so a later copy of it under (D, s) is an alias, not a second op.
#[test]
fn g2_i6_promotion_and_alias() {
    let n1 = own("n1", 1, UploadState::Never, "M", None);
    let a2 = own("a2", 2, UploadState::Attempted, "M", Some(1));
    let r3 = own("r3", 3, UploadState::Rejected, "M", Some(2));
    let p4 = own("p4", 4, UploadState::Published, "M", Some(3));
    let u5 = own("u5", 5, UploadState::Never, "M", Some(4));
    let ops = vec![n1.clone(), a2.clone(), r3.clone(), p4.clone(), u5.clone()];

    let (plan, drawn) = run(&ops, &remote_of(&[&n1, &a2, &r3, &p4]), &no_compaction());
    let plan = plan.unwrap();
    assert_eq!((plan.verified_prefix, drawn), (4, 0));
    assert_eq!(plan.promoted, ["n1", "a2", "r3"]);
    assert!(plan.quarantined.is_empty() && plan.reissued.is_empty());
    let u = moved(&plan, "u5");
    assert_eq!((u.seq, &u.ctx), (1, &Frontier::from([(A.to_owned(), 4)])));
    assert_eq!(u.content_hash, u5.content_hash);
    // The original (or another clone) later publishes u5 at (A, 5): same op
    // id and content hash → one logical op with two dots.
    let alias = |left: &OwnOp, right: &OwnOp| left.op_id == right.op_id && left.content_hash == right.content_hash;
    assert!(alias(u, &u5));
    assert_ne!(u.body_hash, u5.body_hash, "the dots differ but the op is one");

    // Another object at seq 2 ends S at 1. Amended (W-2): the attempted a2
    // now refuses the rotation until R3 settles it.
    let mut forked = remote_of(&[&n1, &a2, &r3, &p4]);
    forked.insert(2, vec![[0x77; 32]]);
    let (plan, drawn) = run(&ops, &forked, &no_compaction());
    assert_eq!((plan, drawn), (Err(RotationError::UnsettledAttempts(vec!["a2".to_owned()])), 0));

    // R3 → published (Drive fork): a2 keeps its dot and extends applied_seq.
    let mut settled = ops.clone();
    settled[1] = settled_as(a2.clone(), UploadState::Published);
    let plan = run(&settled, &forked, &no_compaction()).0.unwrap();
    assert_eq!(plan.verified_prefix, 1);
    assert_eq!(plan.promoted, ["n1"]);
    assert_eq!(rewritten_ids(&plan), ["fresh-1", "u5"]);
    assert_eq!(plan.quarantined, vec![r3.clone()]);
    assert_eq!(plan.retired.applied_seq, 2);

    // R3 → rejected (create-only): a2 is reissued ahead of r3.
    settled[1] = settled_as(a2, UploadState::Rejected);
    let plan = run(&settled, &forked, &no_compaction()).0.unwrap();
    assert_eq!(rewritten_ids(&plan), ["fresh-1", "fresh-2", "u5"]);
    assert_eq!(plan.quarantined, vec![settled[1].clone(), r3]);
    let u = moved(&plan, "u5");
    assert_eq!(u.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 2)]));
    assert_eq!(u.obs, BTreeSet::from(["p4".to_owned()]), "p4 in (S, 4] is kept by op id");
    assert_eq!(plan.retired.applied_seq, 1);

    // Drive self-duplicate of our own bytes: byte-identical copies count once
    // (amended; pass 1 expected S = 0). A different second hash still ends S.
    let mut doubled = remote_of(&[&n1]);
    doubled.insert(1, vec![n1.body_hash, n1.body_hash]);
    let plan = run(std::slice::from_ref(&n1), &doubled, &no_compaction()).0.unwrap();
    assert_eq!((plan.verified_prefix, plan.promoted.as_slice()), (1, ["n1".to_owned()].as_slice()));
    doubled.insert(1, vec![n1.body_hash, [0x01; 32], n1.body_hash]);
    assert_eq!(run(std::slice::from_ref(&n1), &doubled, &no_compaction()).0.unwrap().verified_prefix, 0);
}

/// `local_compacted`: compacted seqs compare hash for hash; a mismatching or
/// missing compacted hash ends S there, and an op above it is moved with its
/// ctx cut to that shorter S.
#[test]
fn g2_local_compacted_hash_must_match() {
    let h = |s: u64| [s as u8; 32];
    let u5 = own("u5", 5, UploadState::Never, "M", Some(4));
    let mut remote: Remote = (1..=4).map(|s| (s, vec![h(s)])).collect();
    let ops = vec![u5.clone()];

    let all_match: Compacted = (1..=4).map(|s| (s, h(s))).collect();
    let plan = run(&ops, &remote, &all_match).0.unwrap();
    assert_eq!(plan.verified_prefix, 4);
    assert_eq!(moved(&plan, "u5").ctx, Frontier::from([(A.to_owned(), 4)]));

    for bad in 1..=4u64 {
        let mut compacted = all_match.clone();
        compacted.insert(bad, [0xC0; 32]);
        let plan = run(&ops, &remote, &compacted).0.unwrap();
        let s = bad - 1;
        assert_eq!(plan.verified_prefix, s, "mismatch at {bad}");
        assert_eq!(plan.retired.verified_prefix, s);
        let u = moved(&plan, "u5");
        assert!(u.ctx.get(A).is_none_or(|seen| *seen <= s), "I3 after mismatch at {bad}");

        let mut missing = all_match.clone();
        missing.remove(&bad);
        assert_eq!(run(&ops, &remote, &missing).0.unwrap().verified_prefix, s, "missing {bad}");
    }

    // A compacted hash matching one of two different remote contents is not
    // enough: two contents end S.
    remote.get_mut(&3).unwrap().push([0xAB; 32]);
    assert_eq!(run(&ops, &remote, &all_match).0.unwrap().verified_prefix, 2);
    // A held op's hash wins over a stale compacted entry for the same seq.
    let held = own("h1", 1, UploadState::Never, "M", None);
    let listed = remote_of(&[&held]);
    let stale: Compacted = BTreeMap::from([(1, [0xC0; 32])]);
    let plan = run(std::slice::from_ref(&held), &listed, &stale).0.unwrap();
    assert_eq!((plan.verified_prefix, plan.promoted.as_slice()), (1, ["h1".to_owned()].as_slice()));
}

#[test]
fn g2_planner_refuses_malformed_input() {
    let remote = Remote::new();
    let compacted = no_compaction();
    let a = own("a", 1, UploadState::Never, "M", None);
    let refuse = |ops: &[OwnOp], new_device: &str| {
        let drawn = Cell::new(0);
        let input = RotationInput { old_device: A, new_device, ops, remote: &remote, local_compacted: &compacted };
        let result = plan_rotation(
            &input,
            || {
                drawn.set(drawn.get() + 1);
                String::from("never")
            },
            hashes,
        );
        assert_eq!(drawn.get(), 0, "no fresh id is drawn for a refused plan");
        result
    };
    assert_eq!(refuse(std::slice::from_ref(&a), A), Err(RotationError::SameDevice));
    let foreign = OwnOp { origin_device: "dev-b".to_owned(), ..a.clone() };
    assert_eq!(refuse(std::slice::from_ref(&foreign), A2), Err(RotationError::ForeignOp("a".to_owned())));
    // An op already moved to D′ fed back in is foreign to D.
    let already_moved = OwnOp { origin_device: A2.to_owned(), ..a.clone() };
    assert_eq!(refuse(&[already_moved], A2), Err(RotationError::ForeignOp("a".to_owned())));
    let same_seq = OwnOp { op_id: "b".to_owned(), ..a.clone() };
    assert_eq!(refuse(&[a.clone(), same_seq], A2), Err(RotationError::DuplicateSeq(1)));
    let same_id = OwnOp { seq: 2, ..a.clone() };
    assert_eq!(refuse(&[a.clone(), same_id], A2), Err(RotationError::DuplicateOpId("a".to_owned())));
    let rejected = OwnOp { upload_state: UploadState::Rejected, origin_device: "dev-z".to_owned(), ..a.clone() };
    assert_eq!(refuse(&[rejected], A2), Err(RotationError::ForeignOp("a".to_owned())));
    // A rejected op ahead of an unsettled one: refused before any id is drawn.
    let x = OwnOp { op_id: "x".to_owned(), seq: 2, upload_state: UploadState::Rejected, ..a.clone() };
    let t = OwnOp { op_id: "t".to_owned(), seq: 3, upload_state: UploadState::Attempted, ..a.clone() };
    assert_eq!(refuse(&[a, x, t], A2), Err(RotationError::UnsettledAttempts(vec!["t".to_owned()])));
}

/// Seqs start at 1 (§4.2.1 R4/R5). A seq-0 op cannot have been verified
/// remotely, yet `seq <= S` holds for S = 0; pass 1 found it "promoted"
/// (marked published) with no remote copy (B-G2-2). It must be refused.
#[test]
fn g2_seq_zero_is_not_silently_promoted() {
    let zero = own("z0", 0, UploadState::Never, "M", None);
    let (plan, drawn) = run(std::slice::from_ref(&zero), &Remote::new(), &no_compaction());
    assert_eq!((plan, drawn), (Err(RotationError::InvalidSeq("z0".to_owned())), 0));
}

/// Seq 0 in any upload state, next to valid ops, or "listed" remotely at
/// seq 0 is refused; it is never promoted, moved, or kept.
#[test]
fn g2_seq_zero_refused_in_every_shape() {
    let one = own("a1", 1, UploadState::Published, "M", None);
    let two = own("a2", 2, UploadState::Never, "M", Some(1));
    for state in [UploadState::Never, UploadState::Attempted, UploadState::Published, UploadState::Rejected] {
        let zero = own("z0", 0, state, "M", None);
        let mut remote = remote_of(&[&one, &zero]);
        for ops in [vec![zero.clone()], vec![one.clone(), zero.clone(), two.clone()], vec![two.clone(), zero.clone()]] {
            let (plan, drawn) = run(&ops, &remote, &no_compaction());
            assert_eq!((plan, drawn), (Err(RotationError::InvalidSeq("z0".to_owned())), 0), "{state:?}");
        }
        let compacted: Compacted = BTreeMap::from([(0, zero.body_hash)]);
        remote.insert(0, vec![zero.body_hash]);
        let (plan, _) = run(&[zero.clone(), one.clone()], &remote, &compacted);
        assert_eq!(plan, Err(RotationError::InvalidSeq("z0".to_owned())), "{state:?}");
    }
}

#[test]
fn g2_large_seqs_near_u64_max_do_not_overflow() {
    let top = u64::MAX;
    let p = own("p", top - 2, UploadState::Published, "M", Some(top - 3));
    let u = own("u", top - 1, UploadState::Never, "M", Some(top - 2));
    let x = own("x", top, UploadState::Rejected, "M", Some(top - 1));
    let corrupt = own("c", 7, UploadState::Never, "N", Some(top));
    let remote: Remote = BTreeMap::from([(top - 2, vec![p.body_hash]), (top, vec![[1; 32]])]);
    let ops = vec![corrupt, p, u, x];

    let unlisted: Compacted = BTreeMap::from([(1, [1; 32]), (top - 3, [3; 32])]);
    for compacted in [no_compaction(), unlisted] {
        let (plan, drawn) = run(&ops, &remote, &compacted);
        let plan = plan.unwrap();
        assert_eq!((plan.verified_prefix, drawn), (0, 1));
        assert_eq!(rewritten_ids(&plan), ["c", "u", "fresh-1"]);
        let c = moved(&plan, "c");
        assert!(!c.ctx.contains_key(A), "ctx[A] = MAX is cut to S = 0");
        assert!(c.ctx.get(A2).is_none_or(|seen| *seen < c.seq));
        let u2 = moved(&plan, "u");
        assert_eq!(u2.obs, BTreeSet::from(["p".to_owned()]));
        assert_eq!(u2.ctx, Frontier::from([(A2.to_owned(), 1)]), "c at old seq 7 ≤ MAX-2 is named on D′");
        assert_eq!(moved(&plan, "fresh-1").ctx, Frontier::from([(A2.to_owned(), 2)]));
        assert_eq!(plan.retired.applied_seq, 0);
    }

    // Compaction past what is listed stops at the last listed seq.
    let listed: Remote = (1..=3).map(|s| (s, vec![[s as u8; 32]])).collect();
    let compacted: Compacted = (1..=5).map(|s| (s, [s as u8; 32])).collect();
    assert_eq!(run(&[], &listed, &compacted).0.unwrap().verified_prefix, 3);
}

#[test]
fn g2_empty_ops_plan_is_empty() {
    let (plan, drawn) = run(&[], &Remote::new(), &no_compaction());
    let plan = plan.unwrap();
    assert_eq!(drawn, 0);
    assert_eq!(plan.verified_prefix, 0);
    assert!(plan.rewritten.is_empty() && plan.quarantined.is_empty());
    assert!(plan.reissued.is_empty() && plan.promoted.is_empty());
    assert_eq!((plan.retired.device_id.as_str(), plan.retired.applied_seq), (A, 0));

    let listed: Remote = (1..=2).map(|s| (s, vec![[s as u8; 32]])).collect();
    assert_eq!(
        run(&[], &listed, &no_compaction()).0.unwrap().verified_prefix,
        0,
        "unverifiable without local or compaction"
    );
    let compacted: Compacted = (1..=2).map(|s| (s, [s as u8; 32])).collect();
    assert_eq!(run(&[], &listed, &compacted).0.unwrap().verified_prefix, 2);
    let wrong: Compacted = BTreeMap::from([(1, [1; 32]), (2, [9; 32])]);
    assert_eq!(run(&[], &listed, &wrong).0.unwrap().verified_prefix, 1);
}

// ---------------------------------------------------------------------------
// W-1 / AC 17 / R3: settling attempted uploads
// ---------------------------------------------------------------------------

const KEY: &str = "v1/vault/log/dev-a/000000000004-000000000006.seg";
const OURS: &[u8] = b"our sealed segment";
const THEIRS: &[u8] = b"another writer's segment";
const GARBAGE: &[u8] = b"\x00\xFFnot a segment";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    /// A successful listing with the bytes it returned.
    Get(Vec<Vec<u8>>),
    GetFailed,
    Put,
    PutFailed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap()
}

/// One key on a provider. `create_only` stores answer AlreadyExists for a
/// taken key (R2/WebDAV/OneDrive); otherwise duplicates accumulate (Drive).
/// Writes become visible to listings only after `write_lag` reads.
struct FakeStore {
    create_only: bool,
    write_lag: usize,
    visible: Mutex<Vec<(String, Vec<u8>)>>,
    pending: Mutex<Vec<(String, Vec<u8>, usize)>>,
    /// Per-`get_all` script: `false` fails that call; empty means succeed.
    gets: Mutex<VecDeque<bool>>,
    /// Puts that fail before anything is written.
    refused_puts: Mutex<usize>,
    /// Puts whose write lands but whose response is lost.
    lost_responses: Mutex<usize>,
    calls: Mutex<Vec<Call>>,
    next_ref: Mutex<usize>,
}

impl FakeStore {
    fn new(create_only: bool, write_lag: usize) -> Self {
        Self {
            create_only,
            write_lag,
            visible: Mutex::default(),
            pending: Mutex::default(),
            gets: Mutex::default(),
            refused_puts: Mutex::new(0),
            lost_responses: Mutex::new(0),
            calls: Mutex::default(),
            next_ref: Mutex::new(0),
        }
    }

    fn fresh_ref(&self) -> String {
        let mut next = lock(&self.next_ref);
        *next += 1;
        format!("file-{next}")
    }

    /// An object that landed earlier and becomes listable after `lag` reads.
    fn landed(&self, bytes: &[u8], lag: usize) {
        let provider_ref = self.fresh_ref();
        lock(&self.pending).push((provider_ref, bytes.to_vec(), lag));
    }

    fn objects(&self) -> Vec<Vec<u8>> {
        let mut all: Vec<Vec<u8>> = lock(&self.visible).iter().map(|(_, bytes)| bytes.clone()).collect();
        all.extend(lock(&self.pending).iter().map(|(_, bytes, _)| bytes.clone()));
        all
    }

    fn count(&self, bytes: &[u8]) -> usize {
        self.objects().iter().filter(|object| object.as_slice() == bytes).count()
    }

    /// Every pending write becomes listable now.
    fn flush(&self) {
        let drained: Vec<_> = lock(&self.pending).drain(..).collect();
        lock(&self.visible).extend(drained.into_iter().map(|(provider_ref, bytes, _)| (provider_ref, bytes)));
    }

    fn delete(&self, provider_ref: &str) {
        lock(&self.visible).retain(|(r, _)| r != provider_ref);
        lock(&self.pending).retain(|(r, _, _)| r != provider_ref);
    }

    fn calls(&self) -> Vec<Call> {
        lock(&self.calls).clone()
    }

    fn puts(&self) -> usize {
        self.calls().iter().filter(|call| matches!(call, Call::Put | Call::PutFailed)).count()
    }

    fn listing(&self) -> Vec<RemoteObject> {
        lock(&self.visible)
            .iter()
            .map(|(provider_ref, bytes)| RemoteObject { provider_ref: provider_ref.clone(), bytes: bytes.clone() })
            .collect()
    }
}

impl SegmentRemote for FakeStore {
    fn create_only(&self) -> bool {
        self.create_only
    }

    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, TransportError> {
        assert_eq!(key, KEY);
        if lock(&self.gets).pop_front() == Some(false) {
            lock(&self.calls).push(Call::GetFailed);
            return Err(TransportError);
        }
        {
            let mut visible = lock(&self.visible);
            lock(&self.pending).retain_mut(|(provider_ref, bytes, reads)| {
                if *reads == 0 {
                    visible.push((provider_ref.clone(), bytes.clone()));
                    false
                } else {
                    *reads -= 1;
                    true
                }
            });
        }
        let listing = self.listing();
        lock(&self.calls).push(Call::Get(listing.iter().map(|o| o.bytes.clone()).collect()));
        Ok(listing)
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, TransportError> {
        assert_eq!(key, KEY);
        assert_eq!(bytes, OURS, "a resend must carry the same bytes");
        {
            let mut refused = lock(&self.refused_puts);
            if *refused > 0 {
                *refused -= 1;
                lock(&self.calls).push(Call::PutFailed);
                return Err(TransportError);
            }
        }
        let taken = !self.objects().is_empty();
        if taken && self.create_only {
            lock(&self.calls).push(Call::Put);
            return Ok(PutOutcome::AlreadyExists);
        }
        let provider_ref = self.fresh_ref();
        lock(&self.pending).push((provider_ref, bytes.to_vec(), self.write_lag));
        {
            let mut lost = lock(&self.lost_responses);
            if *lost > 0 {
                *lost -= 1;
                lock(&self.calls).push(Call::PutFailed);
                return Err(TransportError);
            }
        }
        lock(&self.calls).push(Call::Put);
        Ok(PutOutcome::Created)
    }
}

#[derive(Default)]
struct Pauses(Mutex<Vec<Duration>>);

impl Pause for Pauses {
    async fn pause(&self, duration: Duration) {
        lock(&self.0).push(duration);
    }
}

/// The caller's W-1 judgement after decrypting: ours, another writer's
/// well-formed segment, or unreadable.
fn judge(object: &[u8]) -> Verdict {
    match object {
        OURS => Verdict::Ours,
        THEIRS => Verdict::Other,
        _ => Verdict::Invalid,
    }
}

async fn cycle(store: &FakeStore) -> (Settled, Vec<Duration>) {
    let pauses = Pauses::default();
    let settled = settle_attempted(store, &pauses, KEY, OURS, judge).await;
    (settled, pauses.0.into_inner().unwrap())
}

/// Cycles until the op leaves `attempted`; returns every result seen.
async fn cycles(store: &FakeStore, max: usize) -> Vec<Settled> {
    let mut seen = Vec::new();
    for _ in 0..max {
        let (settled, _) = cycle(store).await;
        let done = settled.outcome != Outcome::StillAttempted;
        seen.push(settled);
        if done {
            break;
        }
    }
    seen
}

fn outcomes(seen: &[Settled]) -> Vec<Outcome> {
    seen.iter().map(|s| s.outcome).collect()
}

fn settled(outcome: Outcome) -> Settled {
    Settled { outcome, extra_self_copies: Vec::new(), invalid_seen: false }
}

/// Exactly once on create-only stores across listing lag, lost responses
/// and refused puts: one object, our bytes, never rejected.
#[tokio::test]
async fn g2_w1_create_only_publishes_exactly_once() {
    for landed_lag in [None, Some(0), Some(1), Some(2), Some(3), Some(5), Some(9)] {
        for write_lag in [0, 2, 4, 7] {
            for lost in 0..=2 {
                for refused in 0..=1 {
                    let store = FakeStore::new(true, write_lag);
                    if let Some(lag) = landed_lag {
                        store.landed(OURS, lag);
                    }
                    *lock(&store.lost_responses) = lost;
                    *lock(&store.refused_puts) = refused;
                    let seen = cycles(&store, 12).await;
                    let outs = outcomes(&seen);
                    let case = format!("landed {landed_lag:?} write_lag {write_lag} lost {lost} refused {refused}");
                    assert_eq!(outs.last(), Some(&Outcome::Published), "{case}: {outs:?}");
                    assert!(!outs.contains(&Outcome::Rejected), "{case}");
                    assert!(seen.iter().all(|s| s.extra_self_copies.is_empty() && !s.invalid_seen), "{case}");
                    assert_eq!(store.count(OURS), 1, "{case}: one object");
                    assert_eq!(store.objects().len(), 1, "{case}");
                    assert!(store.puts() <= seen.len(), "{case}: at most one resend per cycle");
                }
            }
        }
    }
}

/// An unknown read never counts as "absent": no resend after a transport
/// error at any probe, and the op stays attempted.
#[tokio::test]
async fn g2_w1_no_resend_after_a_probe_transport_error() {
    for create_only in [true, false] {
        for failing_probe in 0..3 {
            let store = FakeStore::new(create_only, 0);
            let mut script: VecDeque<bool> = std::iter::repeat_n(true, failing_probe).collect();
            script.push_back(false);
            *lock(&store.gets) = script;
            let (result, pauses) = cycle(&store).await;
            assert_eq!(result, settled(Outcome::StillAttempted), "probe {failing_probe}");
            assert_eq!(store.puts(), 0, "probe {failing_probe}: resent after an unknown read");
            assert_eq!(pauses, RECHECK_DELAYS[..failing_probe].to_vec());
            assert_eq!(cycles(&store, 3).await, [settled(Outcome::Published)]);
            assert_eq!((store.puts(), store.count(OURS)), (1, 1));
        }
        // Verification read after the resend fails: unknown, and the next
        // cycle finds the object without a second put.
        let store = FakeStore::new(create_only, 0);
        *lock(&store.gets) = VecDeque::from([true, true, true, false]);
        assert_eq!(cycle(&store).await.0, settled(Outcome::StillAttempted));
        assert_eq!(cycles(&store, 3).await, [settled(Outcome::Published)]);
        assert_eq!((store.puts(), store.count(OURS)), (1, 1));
    }
}

/// Another writer on a create-only store: rejected only once it is seen, never
/// overwritten, and unknown (still attempted) while it is invisible.
#[tokio::test]
async fn g2_w1_foreign_object_on_create_only_store() {
    let store = FakeStore::new(true, 0);
    store.landed(THEIRS, 0);
    assert_eq!(cycle(&store).await, (settled(Outcome::Rejected), vec![]));
    assert_eq!(store.puts(), 0);

    let store = FakeStore::new(true, 0);
    store.landed(THEIRS, 5);
    let seen = cycles(&store, 3).await;
    assert_eq!(outcomes(&seen), [Outcome::StillAttempted, Outcome::Rejected]);
    assert_eq!(store.objects(), vec![THEIRS.to_vec()], "no overwrite, ours never stored");
    assert_eq!(store.puts(), 1);
}

/// Index of the first successful listing that showed `bytes`.
fn first_seen(calls: &[Call], bytes: &[u8]) -> Option<usize> {
    calls.iter().position(|call| matches!(call, Call::Get(listing) if listing.iter().any(|b| b.as_slice() == bytes)))
}

/// Create-only adversarial sweep: a foreign or unreadable object under our
/// key yields Rejected, with zero puts once it is visible, and the stored
/// object is never replaced. Never Published, never a fork.
#[tokio::test]
async fn g2_w1_create_only_foreign_or_invalid_rejects_without_writes() {
    let blockers: [&[&[u8]]; 3] = [&[THEIRS], &[GARBAGE], &[THEIRS, GARBAGE]];
    for blocker in blockers {
        for lag in 0..=8 {
            for write_lag in [0, 3] {
                for lost in 0..=1 {
                    let store = FakeStore::new(true, write_lag);
                    for bytes in blocker {
                        store.landed(bytes, lag);
                    }
                    *lock(&store.lost_responses) = lost;
                    let seen = cycles(&store, 8).await;
                    let outs = outcomes(&seen);
                    let case = format!("blocker {blocker:?} lag {lag} write_lag {write_lag} lost {lost}");
                    assert_eq!(outs.last(), Some(&Outcome::Rejected), "{case}: {outs:?}");
                    assert!(
                        !outs.iter().any(|o| matches!(o, Outcome::Published | Outcome::PublishedWithFork)),
                        "{case}"
                    );
                    let mut expected: Vec<Vec<u8>> = blocker.iter().map(|b| b.to_vec()).collect();
                    let mut stored = store.objects();
                    expected.sort();
                    stored.sort();
                    assert_eq!(stored, expected, "{case}: nothing overwritten or added");
                    let calls = store.calls();
                    let visible_at = first_seen(&calls, blocker[0]).unwrap_or_else(|| panic!("{case}: never listed"));
                    assert!(
                        calls[visible_at..].iter().all(|call| !matches!(call, Call::Put | Call::PutFailed)),
                        "{case}: put after the foreign object was visible: {calls:?}"
                    );
                    assert!(store.puts() <= seen.len(), "{case}");
                    let last = seen.last().unwrap();
                    assert!(last.extra_self_copies.is_empty(), "{case}");
                    assert_eq!(last.invalid_seen, blocker.contains(&GARBAGE), "{case}");
                }
            }
        }
    }
}

impl FakeStore {
    fn bytes_of(&self, provider_ref: &str) -> Option<Vec<u8>> {
        let visible = lock(&self.visible).iter().find(|(r, _)| r == provider_ref).map(|(_, b)| b.clone());
        visible.or_else(|| lock(&self.pending).iter().find(|(r, _, _)| r == provider_ref).map(|(_, b, _)| b.clone()))
    }

    /// The engine's verify pass: list everything, drop byte-identical extra
    /// copies of ours by `provider_ref`. Returns how many were removed.
    async fn remove_self_duplicates(&self) -> usize {
        self.flush();
        let listing = self.get_all(KEY).await.unwrap();
        let extras = extra_self_copies(&listing, judge);
        for provider_ref in &extras {
            assert_eq!(self.bytes_of(provider_ref).as_deref(), Some(OURS), "{provider_ref} is not a copy of ours");
            self.delete(provider_ref);
        }
        extras.len()
    }
}

/// Drive-style store (duplicates allowed): no overwrite ever, our copies are
/// byte-identical, variants with another object report a fork.
#[tokio::test]
async fn g2_w1_drive_duplicates() {
    // Ours landed but lists only after the probes: the resend adds a second,
    // identical copy, which the verify pass names and removes (pass-1 watch
    // W-G2-2, now `extra_self_copies`).
    let store = FakeStore::new(false, 0);
    store.landed(OURS, 5);
    assert_eq!(cycles(&store, 3).await, [settled(Outcome::Published)]);
    assert!(store.objects().iter().all(|object| object == OURS));
    assert_eq!((store.puts(), store.count(OURS)), (1, 2));
    assert_eq!(store.remove_self_duplicates().await, 1);
    assert_eq!(store.count(OURS), 1);
    assert_eq!(cycle(&store).await, (settled(Outcome::Published), vec![]));
    assert_eq!(store.puts(), 1);

    let store = FakeStore::new(false, 0);
    store.landed(OURS, 0);
    store.landed(THEIRS, 0);
    assert_eq!(cycle(&store).await, (settled(Outcome::PublishedWithFork), vec![]));
    assert_eq!(store.puts(), 0);

    // Nothing there yet: one resend, one object.
    let store = FakeStore::new(false, 0);
    assert_eq!(cycle(&store).await, (settled(Outcome::Published), RECHECK_DELAYS.to_vec()));
    assert_eq!((store.puts(), store.count(OURS)), (1, 1));
}

/// R3: "an op that may still arrive late never gets another name". On a
/// store that allows duplicate names, a visible foreign object proves
/// nothing about ours (no AlreadyExists/412/409 was ever returned, §4.1).
/// Pass 1 got Rejected here (B-G2-1); it must settle as a fork.
#[tokio::test]
async fn g2_w1_drive_foreign_listing_is_not_a_confirmed_rejection() {
    let store = FakeStore::new(false, 0);
    store.landed(THEIRS, 0);
    store.landed(OURS, 1);
    let first = cycle(&store).await.0;
    assert_ne!(first.outcome, Outcome::Rejected, "our upload is still in flight on a duplicate-allowing store");
    let seen = cycles(&store, 3).await;
    assert_eq!(seen.last().map(|s| s.outcome), Some(Outcome::PublishedWithFork));
    assert_eq!((store.puts(), store.count(OURS), store.count(THEIRS)), (0, 1, 1));
}

/// Drive sweep: a foreign (and optionally an unreadable) object next to ours
/// that lags by any amount, with lagging writes and lost responses. Never
/// Rejected; always settles published; every self-duplicate is reported by
/// `extra_self_copies` and removable, after which one copy of ours remains
/// and the foreign objects are untouched.
#[tokio::test]
async fn g2_w1_drive_foreign_plus_lagging_ours_never_rejected() {
    let mut forks = 0usize;
    let mut duplicates_removed = 0usize;
    for ours_lag in [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(6), Some(9)] {
        for theirs_lag in [0, 1, 3, 6] {
            for write_lag in [0, 2, 5] {
                for lost in 0..=2 {
                    for garbage in [false, true] {
                        let store = FakeStore::new(false, write_lag);
                        store.landed(THEIRS, theirs_lag);
                        if garbage {
                            store.landed(GARBAGE, theirs_lag);
                        }
                        if let Some(lag) = ours_lag {
                            store.landed(OURS, lag);
                        }
                        *lock(&store.lost_responses) = lost;
                        let case = format!(
                            "ours {ours_lag:?} theirs {theirs_lag} write_lag {write_lag} lost {lost} garbage {garbage}"
                        );
                        let seen = cycles(&store, 12).await;
                        let outs = outcomes(&seen);
                        assert!(!outs.contains(&Outcome::Rejected), "{case}: {outs:?}");
                        let last = *outs.last().unwrap();
                        assert!(matches!(last, Outcome::Published | Outcome::PublishedWithFork), "{case}: {outs:?}");
                        assert!(store.puts() <= seen.len(), "{case}: at most one resend per cycle");
                        for s in &seen {
                            for r in &s.extra_self_copies {
                                assert_eq!(store.bytes_of(r).as_deref(), Some(OURS), "{case}: reported {r}");
                            }
                        }
                        assert_eq!((store.count(THEIRS), store.count(GARBAGE)), (1, usize::from(garbage)), "{case}");
                        assert!(store.count(OURS) >= 1, "{case}");

                        let copies = store.count(OURS);
                        assert_eq!(store.remove_self_duplicates().await, copies - 1, "{case}");
                        duplicates_removed += copies - 1;
                        assert_eq!(store.count(OURS), 1, "{case}");
                        assert_eq!((store.count(THEIRS), store.count(GARBAGE)), (1, usize::from(garbage)), "{case}");
                        let puts = store.puts();
                        let after = cycle(&store).await.0;
                        assert_eq!(
                            after,
                            Settled { outcome: Outcome::PublishedWithFork, extra_self_copies: vec![], invalid_seen: garbage },
                            "{case}"
                        );
                        assert_eq!(store.puts(), puts, "{case}: no resend once ours is listed");
                        forks += usize::from(last == Outcome::PublishedWithFork);
                    }
                }
            }
        }
    }
    assert!(forks > 100 && duplicates_removed > 50, "forks {forks}, duplicates {duplicates_removed}");
}

/// Drive with only unreadable objects under our key: ours is resent and
/// published with `invalid_seen` for quarantine; an unreadable object is never
/// a fork (§4.4 needs two valid variants) and never a rejection.
#[tokio::test]
async fn g2_w1_drive_invalid_only_is_quarantined_not_a_fork() {
    for copies in 1..=3 {
        let store = FakeStore::new(false, 0);
        for _ in 0..copies {
            store.landed(GARBAGE, 0);
        }
        let (result, pauses) = cycle(&store).await;
        assert_eq!(
            result,
            Settled { outcome: Outcome::Published, extra_self_copies: vec![], invalid_seen: true },
            "{copies} garbage"
        );
        assert_eq!(pauses, RECHECK_DELAYS.to_vec());
        assert_eq!((store.puts(), store.count(OURS), store.count(GARBAGE)), (1, 1, copies));
    }
}

/// Drive with several copies of ours plus a foreign and an unreadable object:
/// a fork, no write, the self-duplicates named for removal and nothing else.
#[tokio::test]
async fn g2_w1_drive_ours_duplicates_and_foreign() {
    let store = FakeStore::new(false, 0);
    for bytes in [OURS, THEIRS, OURS, GARBAGE, OURS] {
        store.landed(bytes, 0);
    }
    let (result, pauses) = cycle(&store).await;
    assert_eq!((result.outcome, result.invalid_seen, pauses), (Outcome::PublishedWithFork, true, vec![]));
    assert_eq!(result.extra_self_copies.len(), 2);
    assert_eq!(store.puts(), 0);
    let listing = store.listing();
    assert_eq!(extra_self_copies(&listing, judge), result.extra_self_copies);
    for provider_ref in &result.extra_self_copies {
        assert_eq!(store.bytes_of(provider_ref).as_deref(), Some(OURS));
        store.delete(provider_ref);
    }
    assert_eq!((store.count(OURS), store.count(THEIRS), store.count(GARBAGE)), (1, 1, 1));
    let (again, _) = cycle(&store).await;
    assert_eq!(again, Settled { outcome: Outcome::PublishedWithFork, extra_self_copies: vec![], invalid_seen: true });
    assert_eq!(store.puts(), 0);
}

// ---------------------------------------------------------------------------
// AC 25 / §4.7: sync secrets never reachable through `named_entry`
// ---------------------------------------------------------------------------

fn every_secret(id: &str) -> Vec<SyncSecret<'_>> {
    let providers = [CloudProvider::GoogleDrive, CloudProvider::OneDrive, CloudProvider::WebDav, CloudProvider::R2];
    let mut secrets = vec![SyncSecret::Device { vault_id: id }, SyncSecret::VaultKey { vault_id: id }];
    secrets.extend(providers.map(|provider| SyncSecret::Credential { provider, binding_fp: id }));
    // Exhaustive matches: a new variant or provider fails to compile, and the
    // list must cover every one of them.
    let kinds: BTreeSet<&str> = secrets
        .iter()
        .map(|secret| match secret {
            SyncSecret::Device { .. } => "device",
            SyncSecret::VaultKey { .. } => "vault",
            SyncSecret::Credential { provider, .. } => match provider {
                CloudProvider::GoogleDrive => "gdrive",
                CloudProvider::OneDrive => "onedrive",
                CloudProvider::WebDav => "webdav",
                CloudProvider::R2 => "r2",
            },
        })
        .collect();
    assert_eq!(kinds.len(), 6);
    secrets
}

#[test]
fn g2_ac25_every_sync_account_is_refused_by_named_entry() {
    let long = "f".repeat(64);
    let ids = [
        VAULT,
        long.as_str(),
        "a",
        "deepl_api_key",
        "nvidia_api_key",
        "oauth_anthropic",
        "openrouter_api_key",
        "0",
    ];
    let mut seen = BTreeSet::new();
    for id in ids {
        for secret in every_secret(id) {
            let account = secret.account().unwrap_or_else(|| panic!("{secret:?} must name an entry")).as_str().to_owned();
            assert!(account.starts_with("sync:"), "{account}");
            assert!(seen.insert(account.clone()), "{account} named twice");
            for variant in [
                account.clone(),
                account.to_uppercase(),
                format!(" {account}"),
                format!("{account}\n"),
                format!("{account}\0"),
            ] {
                assert!(crate::named_entry(&variant).is_err(), "{variant:?} must be refused");
            }
        }
    }
    assert_eq!(seen.len(), ids.len() * 6);

    // Ids that would leave the namespace name no entry and write nothing.
    let store = MemoryStore::default();
    for bad in ["", ":oauth_anthropic", "x:deepl_api_key", "../a", "a b", "é", "-a", &"f".repeat(65)] {
        for secret in every_secret(bad) {
            assert_eq!(secret.account(), None, "{secret:?}");
            assert_eq!(write_secret(&store, secret, b"k"), Err(SecretError::InvalidName));
            assert!(matches!(read_secret(&store, secret), Err(SecretError::InvalidName)));
        }
    }
    assert!(store.accounts().is_empty());
}