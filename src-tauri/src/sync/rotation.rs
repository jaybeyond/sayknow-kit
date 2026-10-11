//! Rotation planning (§4.2.1 R2, R5–R7, with the W-2 amendment).
//!
//! When this device rotates from the old id `D` to a new id `D′`, its own ops
//! under `D` are sorted against what the remote log verifiably holds:
//!
//! - **P** (published) stay exactly as they are: a published name never
//!   changes (I2).
//! - **U** (never sent) keep their op id and move to a new dot on `D′`.
//! - **X** (rejected by the provider) are reissued under a new op id.
//!
//! W-2: rotation waits until R3 has settled every `attempted` op, so an op
//! whose upload outcome is still unknown is never treated as seen. Uploads are
//! already stopped while a trigger is pending, so waiting only delays sync.
//!
//! The causal context of every moved op is cut back to the verified shared
//! prefix `S`, so a clone can never claim to have seen what the original
//! wrote after the split (N-1). The planner is pure: the store applies the
//! plan in one SQLite transaction after the keychain already names `D′` (R4).

use std::collections::{BTreeMap, BTreeSet};

/// Per-device causal frontier: the highest seq applied without gaps.
pub(crate) type Frontier = BTreeMap<String, u64>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UploadState {
    #[default]
    Never,
    Attempted,
    Published,
    Rejected,
}

/// One op this device created, with every input of its §4.3 hashes so a
/// rewritten op can be hashed and stored from the plan alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct OwnOp {
    pub(crate) op_id: String,
    pub(crate) origin_device: String,
    pub(crate) seq: u64,
    pub(crate) entity_kind: String,
    pub(crate) entity_id: String,
    pub(crate) op_type: String,
    /// The op's data without any op-id references.
    pub(crate) payload: Vec<u8>,
    /// Op ids this op builds on (a memo edit's base, a message's parent op).
    /// Kept apart from `payload` so a reissue can repoint them; they are part
    /// of the content hash like the payload.
    pub(crate) refs: Vec<String>,
    pub(crate) hlc_wall: u64,
    pub(crate) hlc_ctr: u32,
    pub(crate) ctx: Frontier,
    pub(crate) obs: BTreeSet<String>,
    pub(crate) upload_state: UploadState,
    pub(crate) content_hash: [u8; 32],
    pub(crate) body_hash: [u8; 32],
}

/// Hashes of a rewritten op, computed by the store's canonical encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpHashes {
    pub(crate) content: [u8; 32],
    pub(crate) body: [u8; 32],
}

pub(crate) struct RotationInput<'a> {
    pub(crate) old_device: &'a str,
    pub(crate) new_device: &'a str,
    /// Every op this device created under `old_device` that it still holds.
    pub(crate) ops: &'a [OwnOp],
    /// Verified body hashes per seq listed under `log/<old_device>/` (and its
    /// snapshots), after R3. A seq stored twice lists both hashes.
    pub(crate) remote: &'a BTreeMap<u64, Vec<[u8; 32]>>,
    /// Body hashes of own ops that were verified published and then compacted
    /// away locally, so the prefix can still be compared hash for hash.
    pub(crate) local_compacted: &'a BTreeMap<u64, [u8; 32]>,
}

/// A U or X op under its new dot `(D′, op.seq)`; `source_seq` is the old dot
/// `(D, source_seq)` the store deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rewrite {
    pub(crate) source_seq: u64,
    pub(crate) op: OwnOp,
}

/// The old id's stream, read from `verified_prefix + 1` like another device's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetiredStream {
    pub(crate) device_id: String,
    pub(crate) verified_prefix: u64,
    /// `S` extended through the published ops that follow it without a gap.
    pub(crate) applied_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RotationPlan {
    /// `S`: the longest prefix whose every seq has exactly one remote content
    /// matching this device's body hash.
    pub(crate) verified_prefix: u64,
    /// U and X ops in old-seq order, renumbered `1..` on `D′`.
    pub(crate) rewritten: Vec<Rewrite>,
    /// Old op id → new op id for each reissued X op.
    pub(crate) reissued: BTreeMap<String, String>,
    /// The X ops as they were; kept locally, never uploaded or merged.
    pub(crate) quarantined: Vec<OwnOp>,
    /// Ops not marked published locally that the remote verifiably holds
    /// with the same body (a clone already sent them); mark them published.
    pub(crate) promoted: Vec<String>,
    pub(crate) retired: RetiredStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RotationError {
    SameDevice,
    ForeignOp(String),
    /// Seqs start at 1.
    InvalidSeq(String),
    DuplicateSeq(u64),
    DuplicateOpId(String),
    /// These ops still have an unknown upload outcome; settle them (R3) first.
    UnsettledAttempts(Vec<String>),
}

pub(crate) fn plan_rotation(
    input: &RotationInput<'_>,
    mut new_op_id: impl FnMut() -> String,
    hashes: impl Fn(&OwnOp) -> OpHashes,
) -> Result<RotationPlan, RotationError> {
    let (old, new) = (input.old_device, input.new_device);
    if old == new {
        return Err(RotationError::SameDevice);
    }
    let mut by_seq: BTreeMap<u64, &OwnOp> = BTreeMap::new();
    let mut op_ids = BTreeSet::new();
    for op in input.ops {
        if op.origin_device != old {
            return Err(RotationError::ForeignOp(op.op_id.clone()));
        }
        if op.seq == 0 {
            return Err(RotationError::InvalidSeq(op.op_id.clone()));
        }
        if by_seq.insert(op.seq, op).is_some() {
            return Err(RotationError::DuplicateSeq(op.seq));
        }
        if !op_ids.insert(op.op_id.as_str()) {
            return Err(RotationError::DuplicateOpId(op.op_id.clone()));
        }
    }

    let s = verified_prefix(input, &by_seq);
    let unsettled: Vec<String> = by_seq
        .range(s + 1..)
        .filter(|(_, op)| op.upload_state == UploadState::Attempted)
        .map(|(_, op)| op.op_id.clone())
        .collect();
    if !unsettled.is_empty() {
        return Err(RotationError::UnsettledAttempts(unsettled));
    }

    let mut promoted = Vec::new();
    let mut kept: BTreeMap<u64, &OwnOp> = BTreeMap::new();
    let mut batch: Vec<&OwnOp> = Vec::new();
    for (&seq, &op) in &by_seq {
        if seq <= s {
            if op.upload_state != UploadState::Published {
                promoted.push(op.op_id.clone());
            }
            continue;
        }
        match op.upload_state {
            UploadState::Published => {
                kept.insert(seq, op);
            }
            UploadState::Never | UploadState::Rejected => batch.push(op),
            UploadState::Attempted => unreachable!("refused above"),
        }
    }

    let reissued: BTreeMap<String, String> = batch
        .iter()
        .filter(|op| op.upload_state == UploadState::Rejected)
        .map(|op| (op.op_id.clone(), new_op_id()))
        .collect();
    let remap = |id: &String| reissued.get(id).cloned().unwrap_or_else(|| id.clone());

    let mut new_seq_of: BTreeMap<u64, u64> = BTreeMap::new();
    let mut rewritten = Vec::with_capacity(batch.len());
    let mut quarantined = Vec::new();
    for (k, op) in (1u64..).zip(&batch) {
        let mut next = (*op).clone();
        next.ctx.remove(old);
        if let Some(&seen) = op.ctx.get(old) {
            if seen.min(s) > 0 {
                next.ctx.insert(old.to_owned(), seen.min(s));
            }
            if seen > s {
                // Self-references past S point into this batch (now on D′) or
                // at kept published ops, which stay on D and are named by id.
                if let Some((_, &latest)) = new_seq_of.range(s + 1..=seen).next_back() {
                    next.ctx.insert(new.to_owned(), latest);
                }
                next.obs.extend(
                    kept.range(s + 1..=seen)
                        .map(|(_, p)| *p)
                        .filter(|p| p.entity_kind == op.entity_kind && p.entity_id == op.entity_id)
                        .map(|p| p.op_id.clone()),
                );
            }
        }
        next.obs = next.obs.iter().map(remap).collect();
        next.refs = next.refs.iter().map(remap).collect();
        if let Some(fresh) = reissued.get(&op.op_id) {
            next.op_id = fresh.clone();
            quarantined.push((*op).clone());
        }
        next.origin_device = new.to_owned();
        next.seq = k;
        next.upload_state = UploadState::Never;
        let rehashed = hashes(&next);
        next.content_hash = rehashed.content;
        next.body_hash = rehashed.body;
        new_seq_of.insert(op.seq, k);
        rewritten.push(Rewrite { source_seq: op.seq, op: next });
    }

    let mut applied_seq = s;
    while kept.contains_key(&(applied_seq + 1)) {
        applied_seq += 1;
    }
    Ok(RotationPlan {
        verified_prefix: s,
        rewritten,
        reissued,
        quarantined,
        promoted,
        retired: RetiredStream { device_id: old.to_owned(), verified_prefix: s, applied_seq },
    })
}

/// `S`: walk seqs from 1 while the remote holds exactly one content for the
/// seq (byte-identical copies count once) and it matches our body hash.
fn verified_prefix(input: &RotationInput<'_>, by_seq: &BTreeMap<u64, &OwnOp>) -> u64 {
    let mut s = 0;
    loop {
        let next = s + 1;
        let contents: BTreeSet<&[u8; 32]> = input.remote.get(&next).into_iter().flatten().collect();
        let [remote_hash] = contents.into_iter().collect::<Vec<_>>()[..] else {
            return s;
        };
        let local_hash = by_seq.get(&next).map(|op| &op.body_hash).or_else(|| input.local_compacted.get(&next));
        if local_hash != Some(remote_hash) {
            return s;
        }
        s = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const A: &str = "dev-a";
    const A2: &str = "dev-a2";

    /// Mirrors §4.3: content covers op id, entity, type, payload with refs,
    /// and HLC; body adds the origin device, ctx, and obs. The dot is outside.
    fn hashes(op: &OwnOp) -> OpHashes {
        let content: [u8; 32] = Sha256::digest(format!(
            "{}|{}|{}|{}|{:?}|{:?}|{}|{}",
            op.op_id, op.entity_kind, op.entity_id, op.op_type, op.payload, op.refs, op.hlc_wall, op.hlc_ctr
        ))
        .into();
        let body: [u8; 32] =
            Sha256::digest(format!("{:?}|{}|{:?}|{:?}", content, op.origin_device, op.ctx, op.obs)).into();
        OpHashes { content, body }
    }

    fn sealed(mut op: OwnOp) -> OwnOp {
        let h = hashes(&op);
        op.content_hash = h.content;
        op.body_hash = h.body;
        op
    }

    fn op(id: &str, seq: u64, state: UploadState, entity: &str, seen: Option<u64>) -> OwnOp {
        sealed(OwnOp {
            op_id: id.to_owned(),
            origin_device: A.to_owned(),
            seq,
            entity_kind: "memo".to_owned(),
            entity_id: entity.to_owned(),
            op_type: "edit".to_owned(),
            payload: format!("text of {id}").into_bytes(),
            hlc_wall: 1_700_000_000_000 + seq,
            ctx: seen.map(|s| Frontier::from([(A.to_owned(), s)])).unwrap_or_default(),
            upload_state: state,
            ..OwnOp::default()
        })
    }

    fn remote_of(ops: &[&OwnOp]) -> BTreeMap<u64, Vec<[u8; 32]>> {
        ops.iter().map(|op| (op.seq, vec![op.body_hash])).collect()
    }

    fn plan_with(
        ops: &[OwnOp],
        remote: &BTreeMap<u64, Vec<[u8; 32]>>,
        local_compacted: &BTreeMap<u64, [u8; 32]>,
    ) -> Result<RotationPlan, RotationError> {
        let mut counter = 0;
        let input = RotationInput { old_device: A, new_device: A2, ops, remote, local_compacted };
        plan_rotation(
            &input,
            || {
                counter += 1;
                format!("reissued-{counter}")
            },
            hashes,
        )
    }

    fn plan(ops: &[OwnOp], remote: &BTreeMap<u64, Vec<[u8; 32]>>) -> RotationPlan {
        plan_with(ops, remote, &BTreeMap::new()).unwrap()
    }

    fn find<'a>(plan: &'a RotationPlan, id: &str) -> &'a Rewrite {
        plan.rewritten.iter().find(|rewrite| rewrite.op.op_id == id).unwrap()
    }

    /// AC 16b: the clone's delete must not claim to have seen the original's
    /// edit made at the same seq after the split.
    #[test]
    fn clone_delete_cannot_cover_the_originals_unseen_edit() {
        let shared: Vec<OwnOp> =
            (1..=3).map(|s| op(&format!("a{s}"), s, UploadState::Published, "other", s.checked_sub(1))).collect();
        let x = op("x", 4, UploadState::Never, "X", Some(3));
        let z = op("z", 5, UploadState::Never, "M", Some(4));
        let mut remote = remote_of(&shared.iter().collect::<Vec<_>>());
        remote.insert(4, vec![[0x79; 32]]); // the original's edit y of memo M at (A, 4)
        let ops: Vec<OwnOp> = shared.iter().cloned().chain([x.clone(), z.clone()]).collect();

        let plan = plan(&ops, &remote);
        assert_eq!(plan.verified_prefix, 3);
        let (x2, z2) = (find(&plan, "x"), find(&plan, "z"));
        assert_eq!((x2.source_seq, x2.op.origin_device.as_str(), x2.op.seq), (4, A2, 1));
        assert_eq!((z2.source_seq, z2.op.origin_device.as_str(), z2.op.seq), (5, A2, 2));
        assert_eq!(x2.op.ctx, Frontier::from([(A.to_owned(), 3)]));
        assert_eq!(z2.op.ctx, Frontier::from([(A.to_owned(), 3), (A2.to_owned(), 1)]));
        assert!(z2.op.ctx[A] < 4, "z must not cover y at (A, 4)");
        assert_eq!(x2.op.content_hash, x.content_hash);
        assert_eq!(z2.op.content_hash, z.content_hash);
        assert_ne!(x2.op.body_hash, x.body_hash);
        assert_ne!(z2.op.body_hash, z.body_hash);
        assert_eq!(z2.op.body_hash, hashes(&z2.op).body);
        assert_eq!((z2.op.payload.as_slice(), z2.op.hlc_wall), (z.payload.as_slice(), z.hlc_wall));
        assert_eq!(plan.retired, RetiredStream { device_id: A.to_owned(), verified_prefix: 3, applied_seq: 3 });
        assert!(plan.quarantined.is_empty() && plan.reissued.is_empty() && plan.promoted.is_empty());
    }

    /// I2: published ops keep their name; moved ops keep their same-entity
    /// view of them by op id, and only published ops extend `applied_seq`.
    #[test]
    fn published_ops_are_never_restamped() {
        let ops = vec![
            op("p1", 1, UploadState::Published, "M", None),
            op("p2", 2, UploadState::Published, "M", Some(1)),
            op("u", 3, UploadState::Never, "M", Some(2)),
            op("v", 4, UploadState::Never, "N", Some(3)),
            op("p5", 5, UploadState::Published, "N", Some(4)),
            op("w", 6, UploadState::Never, "N", Some(5)),
        ];
        let remote = remote_of(&[&ops[0]]); // list lag: seqs 2 and 5 not yet visible
        let plan = plan(&ops, &remote);

        assert_eq!(plan.verified_prefix, 1);
        let moved: Vec<&str> = plan.rewritten.iter().map(|r| r.op.op_id.as_str()).collect();
        assert_eq!(moved, ["u", "v", "w"]);
        let (u, v, w) = (&find(&plan, "u").op, &find(&plan, "v").op, &find(&plan, "w").op);
        assert_eq!(u.ctx, Frontier::from([(A.to_owned(), 1)]));
        assert_eq!(u.obs, BTreeSet::from(["p2".to_owned()]));
        assert_eq!(v.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 1)]));
        assert!(v.obs.is_empty(), "p2 is another entity");
        assert_eq!(w.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 2)]));
        assert_eq!(w.obs, BTreeSet::from(["p5".to_owned()]));
        assert_eq!(plan.retired.applied_seq, 2);
    }

    /// W-2: an op whose upload outcome is unknown blocks the rotation, so no
    /// moved op can claim to have seen it and it can never be stranded on D.
    #[test]
    fn rotation_waits_for_every_attempted_upload_to_settle() {
        let ops = vec![
            op("p1", 1, UploadState::Published, "M", None),
            op("att", 2, UploadState::Attempted, "M", Some(1)),
            op("u", 3, UploadState::Never, "M", Some(2)),
        ];
        let remote = remote_of(&[&ops[0]]);
        assert_eq!(
            plan_with(&ops, &remote, &BTreeMap::new()),
            Err(RotationError::UnsettledAttempts(vec!["att".to_owned()]))
        );
        // Once R3 verified it, the attempted op is inside S and simply promoted.
        let remote = remote_of(&[&ops[0], &ops[1]]);
        let plan = plan(&ops, &remote);
        assert_eq!((plan.verified_prefix, plan.promoted.clone()), (2, vec!["att".to_owned()]));
        assert_eq!(find(&plan, "u").op.ctx, Frontier::from([(A.to_owned(), 2)]));
    }

    #[test]
    fn rejected_ops_are_reissued_and_references_follow_them() {
        let p1 = op("p1", 1, UploadState::Published, "M", None);
        let x = op("x", 2, UploadState::Rejected, "M", Some(1));
        let mut u = op("u", 3, UploadState::Never, "M", Some(2));
        u.refs = vec!["x".to_owned(), "p1".to_owned()];
        u.obs = BTreeSet::from(["x".to_owned()]);
        let u = sealed(u);
        let mut remote = remote_of(&[&p1]);
        remote.insert(2, vec![[0x42; 32]]); // another writer took (A, 2)
        let plan = plan(&[p1, x.clone(), u.clone()], &remote);

        assert_eq!(plan.reissued, BTreeMap::from([("x".to_owned(), "reissued-1".to_owned())]));
        assert_eq!(plan.quarantined, vec![x.clone()]);
        let x2 = find(&plan, "reissued-1");
        assert_eq!((x2.source_seq, x2.op.seq, x2.op.upload_state), (2, 1, UploadState::Never));
        assert_eq!((x2.op.entity_id.as_str(), x2.op.payload.as_slice()), ("M", x.payload.as_slice()));
        assert_ne!(x2.op.content_hash, x.content_hash);
        let u2 = &find(&plan, "u").op;
        assert_eq!(u2.seq, 2);
        assert_eq!(u2.refs, ["reissued-1", "p1"]);
        assert_eq!(u2.obs, BTreeSet::from(["reissued-1".to_owned()]));
        assert_eq!(u2.ctx, Frontier::from([(A.to_owned(), 1), (A2.to_owned(), 1)]));
        assert_eq!(u2.content_hash, hashes(u2).content);
        assert!(plan.rewritten.iter().all(|r| r.op.op_id != "x"));
    }

    /// I6: an op the clone already sent under the old dot is one logical op;
    /// it is marked published instead of being restamped a second time.
    #[test]
    fn ops_the_remote_already_holds_are_promoted_not_moved() {
        let never = op("n1", 1, UploadState::Never, "M", None);
        let rejected = op("r2", 2, UploadState::Rejected, "M", Some(1));
        let attempted = op("a3", 3, UploadState::Attempted, "M", Some(2));
        let remote = remote_of(&[&never, &rejected, &attempted]);
        let plan = plan(&[never, rejected, attempted], &remote);
        assert_eq!(plan.verified_prefix, 3);
        assert_eq!(plan.promoted, ["n1", "r2", "a3"]);
        assert!(plan.rewritten.is_empty() && plan.quarantined.is_empty());
    }

    #[test]
    fn compacted_ops_still_compare_hash_for_hash() {
        let u = op("u", 3, UploadState::Never, "M", Some(2));
        let remote = BTreeMap::from([(1, vec![[1; 32]]), (2, vec![[2; 32]])]);
        let compacted = BTreeMap::from([(1, [1; 32]), (2, [2; 32])]);
        let plan_c = plan_with(std::slice::from_ref(&u), &remote, &compacted).unwrap();
        assert_eq!(plan_c.verified_prefix, 2);
        assert_eq!(find(&plan_c, "u").op.ctx, Frontier::from([(A.to_owned(), 2)]));

        let diverged = BTreeMap::from([(1, [1; 32]), (2, [9; 32])]);
        let plan_d = plan_with(std::slice::from_ref(&u), &remote, &diverged).unwrap();
        assert_eq!(plan_d.verified_prefix, 1);
        let unknown = plan(&[u], &remote);
        assert_eq!(unknown.verified_prefix, 0);
        assert!(find(&unknown, "u").op.ctx.is_empty());
    }

    #[test]
    fn identical_copies_count_once_but_two_contents_end_the_prefix() {
        let p1 = op("p1", 1, UploadState::Published, "M", None);
        let copies = BTreeMap::from([(1, vec![p1.body_hash, p1.body_hash])]);
        assert_eq!(plan(std::slice::from_ref(&p1), &copies).verified_prefix, 1);

        let fork = BTreeMap::from([(1, vec![p1.body_hash, [9; 32]])]);
        let plan = plan(&[p1], &fork);
        assert_eq!(plan.verified_prefix, 0);
        assert!(plan.rewritten.is_empty());
        assert_eq!(plan.retired.applied_seq, 1);
    }

    #[test]
    fn malformed_inputs_are_refused() {
        let remote = BTreeMap::new();
        let none = BTreeMap::new();
        let a = op("a", 1, UploadState::Never, "M", None);
        let run = |ops: &[OwnOp], new_device: &str| {
            let input = RotationInput { old_device: A, new_device, ops, remote: &remote, local_compacted: &none };
            plan_rotation(&input, String::new, hashes)
        };
        assert_eq!(run(std::slice::from_ref(&a), A), Err(RotationError::SameDevice));
        let foreign = OwnOp { origin_device: "dev-b".to_owned(), ..a.clone() };
        assert_eq!(run(&[foreign], A2), Err(RotationError::ForeignOp("a".to_owned())));
        let zero = OwnOp { seq: 0, ..a.clone() };
        assert_eq!(run(&[zero], A2), Err(RotationError::InvalidSeq("a".to_owned())));
        let same_seq = OwnOp { op_id: "b".to_owned(), ..a.clone() };
        assert_eq!(run(&[a.clone(), same_seq], A2), Err(RotationError::DuplicateSeq(1)));
        let same_id = OwnOp { seq: 2, ..a.clone() };
        assert_eq!(run(&[a, same_id], A2), Err(RotationError::DuplicateOpId("a".to_owned())));
    }

    /// I1–I3 over random streams: one dot per op, published ops untouched,
    /// no moved op claims anything of the old id beyond `S`, and an unsettled
    /// upload past `S` always blocks the rotation.
    #[test]
    fn invariants_hold_over_random_streams() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let (mut planned, mut refused) = (0, 0);
        for _ in 0..1000 {
            let n = next() % 12 + 1;
            let ops: Vec<OwnOp> = (1..=n)
                .map(|seq| {
                    let state = match next() % 4 {
                        0 => UploadState::Never,
                        1 => UploadState::Attempted,
                        2 => UploadState::Published,
                        _ => UploadState::Rejected,
                    };
                    let seen = (seq > 1).then(|| next() % seq).filter(|s| *s > 0);
                    op(&format!("op{seq}"), seq, state, ["M", "N"][(next() % 2) as usize], seen)
                })
                .collect();
            let mut remote: BTreeMap<u64, Vec<[u8; 32]>> = ops
                .iter()
                .take((next() % (n + 1)) as usize)
                .map(|op| (op.seq, vec![op.body_hash]))
                .collect();
            if next() % 4 == 0 {
                let seq = next() % n + 1;
                remote.entry(seq).or_default().push([0xAB; 32]);
            }
            let plan = match plan_with(&ops, &remote, &BTreeMap::new()) {
                Ok(plan) => plan,
                Err(RotationError::UnsettledAttempts(ids)) => {
                    assert!(!ids.is_empty());
                    refused += 1;
                    continue;
                }
                Err(other) => panic!("{other:?}"),
            };
            planned += 1;
            let s = plan.verified_prefix;
            assert!(ops.iter().all(|op| op.seq <= s || op.upload_state != UploadState::Attempted));

            let new_seqs: Vec<u64> = plan.rewritten.iter().map(|r| r.op.seq).collect();
            assert_eq!(new_seqs, (1..=plan.rewritten.len() as u64).collect::<Vec<_>>());
            let mut ids = BTreeSet::new();
            for Rewrite { source_seq, op: moved } in &plan.rewritten {
                assert!(*source_seq > s);
                assert_eq!(moved.origin_device, A2);
                assert!(ids.insert(moved.op_id.clone()), "two dots for {}", moved.op_id);
                assert!(moved.ctx.get(A).is_none_or(|seen| *seen <= s));
                assert!(moved.ctx.get(A2).is_none_or(|seen| *seen < moved.seq));
                assert_eq!((moved.content_hash, moved.body_hash), {
                    let h = hashes(moved);
                    (h.content, h.body)
                });
            }
            let mut expected_promoted = Vec::new();
            for original in &ops {
                if original.seq <= s {
                    if original.upload_state != UploadState::Published {
                        expected_promoted.push(original.op_id.clone());
                    }
                    assert!(!ids.contains(&original.op_id));
                } else if original.upload_state == UploadState::Published {
                    assert!(!ids.contains(&original.op_id), "{} must keep its dot", original.op_id);
                }
            }
            assert_eq!(plan.promoted, expected_promoted);
            let rejected = ops.iter().filter(|op| op.seq > s && op.upload_state == UploadState::Rejected).count();
            assert_eq!((plan.quarantined.len(), plan.reissued.len()), (rejected, rejected));
            let applied = plan.retired.applied_seq;
            assert!(applied >= s);
            assert!(ops
                .iter()
                .filter(|op| op.seq > s && op.seq <= applied)
                .all(|op| op.upload_state == UploadState::Published));
        }
        assert!(planned > 100 && refused > 100, "both paths exercised: {planned}/{refused}");
    }
}
