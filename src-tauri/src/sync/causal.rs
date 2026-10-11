//! Causality over ops (§4.4 N-2, I6): effective context under forks, the
//! "saw" relation, and the in-memory op set with alias dots.
//!
//! Contract deviations (G3 design, causal.rs):
//! - [`saw_ctx`] is the ctx/obs-level form of [`saw`], so an [`OpSet`] entry
//!   can pass its alias-unioned ctx/obs.
//! - `OpSet::insert` returns `Result<Inserted, OpError>` (content hashing can
//!   fail on a float). `Inserted { outcome: Insert, fork_started }` carries
//!   the "fork started" signal next to New/Alias/Conflict.
//! - Same op id with a different `origin_device` but the same content hash
//!   (a restamp) keeps the smaller `origin_device`, so the stored body does
//!   not depend on arrival order.

use std::collections::{BTreeMap, BTreeSet};

use super::ops::{content_hash, Dot, OpBody, OpError};

/// Per device, the first seq of a detected fork (§4.2.3 (a)).
pub(crate) type Forks = BTreeMap<String /* device */, u64 /* fork_start */>;

/// `eff_ctx[d] = min(ctx[d], F[d] - 1)`; a device without a fork is unbounded.
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) fn eff_ctx(ctx: &BTreeMap<String, u64>, forks: &Forks) -> BTreeMap<String, u64> {
    ctx.iter().map(|(d, s)| (d.clone(), eff_seq(*s, d, forks))).collect()
}

fn eff_seq(seq: u64, device: &str, forks: &Forks) -> u64 {
    match forks.get(device) {
        Some(start) => seq.min(start.saturating_sub(1)),
        None => seq,
    }
}

/// Y saw X iff some dot of X (aliases included) is inside Y's effective ctx,
/// or X's op id is in Y's `obs`. Fork-range dots are only seen through `obs`.
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) fn saw(y_body: &OpBody, y_forks: &Forks, x_op_id: &str, x_dots: &[Dot]) -> bool {
    saw_ctx(&y_body.ctx, &y_body.obs, y_forks, x_op_id, x_dots)
}

pub(crate) fn saw_ctx<'a>(
    y_ctx: &BTreeMap<String, u64>,
    y_obs: &[String],
    y_forks: &Forks,
    x_op_id: &str,
    x_dots: impl IntoIterator<Item = &'a Dot>,
) -> bool {
    x_dots.into_iter().any(|dot| {
        y_ctx.get(&dot.device).is_some_and(|s| eff_seq(*s, &dot.device, y_forks) >= dot.seq)
    }) || y_obs.iter().any(|o| o == x_op_id)
}

/// One logical op with every dot it was published at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpEntry {
    /// ctx and obs are the union over all aliases.
    pub(crate) body: OpBody,
    pub(crate) content_hash: [u8; 32],
    pub(crate) dots: BTreeSet<Dot>,
}

impl OpEntry {
    pub(crate) fn dot_list(&self) -> Vec<Dot> {
        self.dots.iter().cloned().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Insert {
    New,
    /// Same op id and content at another (or the same) dot (I6).
    Alias,
    /// Same op id, different content. Nothing was stored; quarantine it.
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inserted {
    pub(crate) outcome: Insert,
    /// The dot now holds two op ids and `forks` gained a row or moved down.
    pub(crate) fork_started: bool,
}

/// Deterministic op set: the stored state depends only on which
/// (body, dot) pairs were inserted, not their order, except for `Conflict`
/// inputs, which are never stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OpSet {
    ops: BTreeMap<String, OpEntry>,
    by_dot: BTreeMap<Dot, BTreeSet<String>>,
    forks: Forks,
}

impl OpSet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, body: OpBody, dot: Dot) -> Result<Inserted, OpError> {
        let hash = content_hash(&body)?;
        if let Some(existing) = self.ops.get(&body.op_id) {
            if existing.content_hash != hash {
                return Ok(Inserted { outcome: Insert::Conflict, fork_started: false });
            }
        }

        let holders = self.by_dot.entry(dot.clone()).or_default();
        holders.insert(body.op_id.clone());
        let mut fork_started = false;
        if holders.len() > 1 {
            let start = self.forks.entry(dot.device.clone()).or_insert(u64::MAX);
            if dot.seq < *start {
                *start = dot.seq;
                fork_started = true;
            }
        }

        let outcome = match self.ops.get_mut(&body.op_id) {
            Some(entry) => {
                entry.dots.insert(dot);
                for (d, s) in body.ctx {
                    let slot = entry.body.ctx.entry(d).or_insert(0);
                    *slot = (*slot).max(s);
                }
                if body.obs.iter().any(|o| entry.body.obs.binary_search(o).is_err()) {
                    let merged: BTreeSet<String> = entry.body.obs.drain(..).chain(body.obs).collect();
                    entry.body.obs = merged.into_iter().collect();
                }
                if body.origin_device < entry.body.origin_device {
                    entry.body.origin_device = body.origin_device;
                }
                Insert::Alias
            }
            None => {
                let op_id = body.op_id.clone();
                let mut body = body;
                let obs: BTreeSet<String> = std::mem::take(&mut body.obs).into_iter().collect();
                body.obs = obs.into_iter().collect();
                self.ops.insert(op_id, OpEntry { body, content_hash: hash, dots: BTreeSet::from([dot]) });
                Insert::New
            }
        };
        Ok(Inserted { outcome, fork_started })
    }

    pub(crate) fn get(&self, op_id: &str) -> Option<&OpEntry> {
        self.ops.get(op_id)
    }

    /// Entries in op id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &OpEntry> {
        self.ops.values()
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn len(&self) -> usize {
        self.ops.len()
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Forks discovered from same-dot/different-op pairs in this set.
    pub(crate) fn forks(&self) -> &Forks {
        &self.forks
    }

    /// Op ids published at `dot`.
    #[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
    pub(crate) fn at_dot(&self, dot: &Dot) -> Option<&BTreeSet<String>> {
        self.by_dot.get(dot)
    }

    /// Whether entry `y` saw entry `x` under `forks`: [`saw_ctx`] over `y`'s
    /// alias-unioned ctx/obs and every dot of `x`. An op never saw itself, so
    /// a forged self-referencing ctx cannot make it causally dominated.
    pub(crate) fn saw_entry(y: &OpEntry, x: &OpEntry, forks: &Forks) -> bool {
        y.body.op_id != x.body.op_id && saw_ctx(&y.body.ctx, &y.body.obs, forks, &x.body.op_id, &x.dots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::ops::{Hlc, Kind};
    use serde_json::json;

    fn oid(n: u64) -> String {
        uuid::Builder::from_unix_timestamp_millis(1_700_000_000_000 + n, &[n as u8; 10]).into_uuid().to_string()
    }

    fn dot(device: &str, seq: u64) -> Dot {
        Dot { device: device.to_owned(), seq }
    }

    fn ctx(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(d, s)| ((*d).to_owned(), *s)).collect()
    }

    fn body(n: u64, origin: &str, c: &[(&str, u64)]) -> OpBody {
        OpBody {
            v: 1,
            op_id: oid(n),
            origin_device: origin.to_owned(),
            hlc: Hlc { wall_ms: n, ctr: 0 },
            ctx: ctx(c),
            obs: Vec::new(),
            kind: Kind::Memo,
            entity_id: "memo-1".to_owned(),
            op_type: "pin".to_owned(),
            payload: json!({"pinned": true}),
        }
    }

    #[test]
    fn eff_ctx_caps_each_forked_device_below_its_fork_start() {
        let c = ctx(&[("a", 9), ("b", 4), ("c", 2)]);
        let forks = Forks::from([("a".to_owned(), 5), ("c".to_owned(), 7)]);
        assert_eq!(eff_ctx(&c, &forks), ctx(&[("a", 4), ("b", 4), ("c", 2)]));
        assert_eq!(eff_ctx(&c, &Forks::new()), c);
        let at_one = Forks::from([("a".to_owned(), 1)]);
        assert_eq!(eff_ctx(&c, &at_one)["a"], 0);
    }

    #[test]
    fn saw_via_dot_alias_dot_and_obs() {
        let y = body(10, "b", &[("a", 3), ("b", 1)]);
        let none = Forks::new();
        assert!(saw(&y, &none, &oid(1), &[dot("a", 3)]));
        assert!(!saw(&y, &none, &oid(1), &[dot("a", 4)]));
        assert!(!saw(&y, &none, &oid(1), &[dot("c", 1)]));
        // An alias dot inside ctx is enough even if the primary dot is not.
        assert!(saw(&y, &none, &oid(1), &[dot("a2", 1), dot("b", 1)]));
        // A fork at a:3 hides a:3 from ctx; obs still makes it seen.
        let forked = Forks::from([("a".to_owned(), 3)]);
        assert!(!saw(&y, &forked, &oid(1), &[dot("a", 3)]));
        assert!(saw(&y, &forked, &oid(1), &[dot("a", 2)]));
        let with_obs = OpBody { obs: vec![oid(1)], ..y };
        assert!(saw(&with_obs, &forked, &oid(1), &[dot("a", 3)]));
        assert!(!saw(&with_obs, &forked, &oid(2), &[dot("a", 3)]));
    }

    #[test]
    fn alias_unions_ctx_and_obs_and_keeps_one_logical_op() {
        let mut set = OpSet::new();
        let original = body(1, "a", &[("a", 4), ("b", 2)]);
        let mut restamped = original.clone();
        restamped.origin_device = "a2".to_owned();
        restamped.ctx = ctx(&[("a", 3), ("c", 5)]);
        restamped.obs = vec![oid(7)];

        let first = set.insert(original.clone(), dot("a", 5)).unwrap();
        assert_eq!(first, Inserted { outcome: Insert::New, fork_started: false });
        let second = set.insert(restamped.clone(), dot("a2", 1)).unwrap();
        assert_eq!(second, Inserted { outcome: Insert::Alias, fork_started: false });
        assert_eq!(set.len(), 1);
        let entry = set.get(&oid(1)).unwrap();
        assert_eq!(entry.dots, BTreeSet::from([dot("a", 5), dot("a2", 1)]));
        assert_eq!(entry.body.ctx, ctx(&[("a", 4), ("b", 2), ("c", 5)]));
        assert_eq!(entry.body.obs, vec![oid(7)]);
        assert_eq!(entry.body.origin_device, "a");

        let mut reversed = OpSet::new();
        reversed.insert(restamped, dot("a2", 1)).unwrap();
        reversed.insert(original.clone(), dot("a", 5)).unwrap();
        assert_eq!(reversed, set, "alias merge is order-independent");

        let again = set.insert(original, dot("a", 5)).unwrap();
        assert_eq!(again.outcome, Insert::Alias);
        assert_eq!(set.get(&oid(1)).unwrap().dots.len(), 2);
    }

    #[test]
    fn same_op_id_with_different_content_is_a_conflict_and_stores_nothing() {
        let mut set = OpSet::new();
        set.insert(body(1, "a", &[]), dot("a", 1)).unwrap();
        let mut tampered = body(1, "a", &[]);
        tampered.payload = json!({"pinned": false});
        let before = set.clone();
        let out = set.insert(tampered, dot("a", 2)).unwrap();
        assert_eq!(out, Inserted { outcome: Insert::Conflict, fork_started: false });
        assert_eq!(set, before);
    }

    #[test]
    fn same_dot_with_another_op_id_records_the_lowest_fork_start() {
        let mut set = OpSet::new();
        set.insert(body(1, "a", &[]), dot("a", 7)).unwrap();
        let out = set.insert(body(2, "a", &[]), dot("a", 7)).unwrap();
        assert_eq!(out, Inserted { outcome: Insert::New, fork_started: true });
        assert_eq!(set.forks(), &Forks::from([("a".to_owned(), 7)]));
        assert_eq!(set.at_dot(&dot("a", 7)).unwrap().len(), 2);

        set.insert(body(3, "a", &[]), dot("a", 9)).unwrap();
        let later = set.insert(body(4, "a", &[]), dot("a", 9)).unwrap();
        assert!(!later.fork_started, "a later fork seq does not move fork_start");
        set.insert(body(5, "a", &[]), dot("a", 3)).unwrap();
        let earlier = set.insert(body(6, "a", &[]), dot("a", 3)).unwrap();
        assert!(earlier.fork_started);
        assert_eq!(set.forks()["a"], 3);
    }

    #[test]
    fn iteration_is_by_op_id_and_saw_entry_uses_alias_dots() {
        let mut set = OpSet::new();
        assert!(set.is_empty());
        for n in [5, 1, 3] {
            set.insert(body(n, "a", &[]), dot("a", n)).unwrap();
        }
        let ids: Vec<&str> = set.iter().map(|e| e.body.op_id.as_str()).collect();
        assert_eq!(ids, vec![oid(1), oid(3), oid(5)]);

        set.insert(body(3, "a", &[]), dot("b", 1)).unwrap();
        let y = OpEntry {
            body: body(9, "b", &[("b", 1)]),
            content_hash: [0; 32],
            dots: BTreeSet::new(),
        };
        let x = set.get(&oid(3)).unwrap();
        assert!(OpSet::saw_entry(&y, x, &Forks::new()));
        assert!(!OpSet::saw_entry(&y, x, &Forks::from([("b".to_owned(), 1)])));
        // A forged ctx naming the op's own dot does not make it see itself.
        let selfish = set.get(&oid(5)).unwrap();
        let mut forged = selfish.clone();
        forged.body.ctx = ctx(&[("a", 5)]);
        assert!(!OpSet::saw_entry(&forged, selfish, &Forks::new()));
    }
}
