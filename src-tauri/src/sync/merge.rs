//! Deterministic projection of an op set (§4.4).
//!
//! `project` is a pure function of (op set, forks): the same ops under the
//! same forks give the same entities and the same hash on every device, in
//! any arrival order (AC 1). Causality always comes first; the HLC only
//! breaks ties between concurrent ops (AC 3b).
//!
//! Contract deviations (G3 design, merge.rs):
//! - `project` returns `Result<Projection, OpError>`: hashing goes through
//!   `canonical_json`, which rejects floats. `OpSet::insert` already rejects
//!   them, so the error is unreachable for sets built through it.
//! - The forks used are `forks ∪ set.forks()` (minimum per device), so a
//!   caller cannot forget forks the set itself detected.
//! - `Projection.memo_lineage` (lineage op id → memo entity id) is the
//!   in-memory form of the `memo_lineage` table; it is not part of `hash`.
//! - Deleted entities project to `EntityState::Deleted` with no payload, so
//!   compaction may drop tombstoned payloads without changing the hash.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::causal::{Forks, OpEntry, OpSet};
use super::ops::{canonical_json, Kind, OpError};

pub(crate) type EntityKey = (Kind, String);

/// Bump when [`project`] (or the lineage routing it records) changes what
/// it produces for the same op set. The store rewrites its persisted
/// projection once on open when this differs from the stored value, so the
/// incremental re-projection path never diffs against rows an older build
/// wrote.
pub(crate) const PROJECTION_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum EntityState {
    /// Tombstone: kept forever so a late op cannot resurrect the entity.
    Deleted,
    Memo(MemoState),
    Conv(ConvState),
    Tr(TrState),
    Policy(PolicyState),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MemoState {
    pub(crate) text: String,
    pub(crate) pinned: bool,
    pub(crate) note: String,
    /// The lineage op whose text is shown; new edits use it as `base_op_id`.
    pub(crate) head_op_id: String,
    /// Set on a conflict copy: the memo it split from.
    pub(crate) conflict_of: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Message {
    pub(crate) parent_id: Option<String>,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) model: Option<String>,
    pub(crate) attachments: Vec<Value>,
    pub(crate) ts: u64,
    pub(crate) op_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConvState {
    pub(crate) title: String,
    pub(crate) active_leaf: Option<String>,
    pub(crate) messages: BTreeMap<String, Message>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TrState {
    pub(crate) data: Value,
    pub(crate) pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PolicyState {
    pub(crate) data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Notice {
    /// "Restored: this conflicted with a delete" (§4.4).
    RestoredAfterDeleteConflict { kind: Kind, entity_id: String },
    /// A concurrent edit of `memo_id` was kept as `copy_id`.
    MemoConflictCopy { memo_id: String, copy_id: String, loser_op_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Projection {
    pub(crate) entities: BTreeMap<EntityKey, EntityState>,
    /// Sorted and unique.
    pub(crate) notices: Vec<Notice>,
    pub(crate) memo_lineage: BTreeMap<String, String>,
    /// `SHA-256(canonical(entities))`, keyed `"<kind>/<id>"`.
    pub(crate) hash: [u8; 32],
}

/// Concurrent tie-break: max of (hlc_wall, hlc_ctr, origin_device, op_id).
fn tie_key(e: &OpEntry) -> (u64, u32, &str, &str) {
    (e.body.hlc.wall_ms, e.body.hlc.ctr, &e.body.origin_device, &e.body.op_id)
}

fn saw(y: &OpEntry, x: &OpEntry, forks: &Forks) -> bool {
    OpSet::saw_entry(y, x, forks)
}

/// Register winner: among the causally maximal candidates (seen by no other
/// candidate), the max tie key. If a forged causal cycle (x saw y and y saw
/// x) leaves no maximal candidate, all candidates compete on the tie key, so
/// the result is still a function of the set alone.
fn winner<'a>(cands: &[&'a OpEntry], forks: &Forks) -> Option<&'a OpEntry> {
    let maximal = cands.iter().copied().filter(|x| !cands.iter().any(|y| saw(y, x, forks)));
    maximal.max_by(|a, b| tie_key(a).cmp(&tie_key(b))).or_else(|| cands.iter().copied().max_by(|a, b| tie_key(a).cmp(&tie_key(b))))
}

enum Fate {
    Live,
    Restored,
    Deleted,
}

/// §4.4 edit vs delete. Content ops that saw a delete are set aside; the
/// entity is deleted only if every remaining one was seen by some delete.
fn fate(content: &[&OpEntry], deletes: &[&OpEntry], forks: &Forks) -> Fate {
    if deletes.is_empty() {
        return Fate::Live;
    }
    let fresh = content.iter().filter(|c| !deletes.iter().any(|d| saw(c, d, forks)));
    let unseen = fresh.into_iter().any(|c| !deletes.iter().any(|d| saw(d, c, forks)));
    if unseen {
        Fate::Restored
    } else {
        Fate::Deleted
    }
}

// Payload accessors. Every body reaching `project` passed `OpBody::validate`
// (store ingest, `Snapshot::decode`), which guarantees each key's type, so
// the defaults below only cover absent optional keys.
fn s<'a>(e: &'a OpEntry, key: &str) -> &'a str {
    e.body.payload.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn opt_s(e: &OpEntry, key: &str) -> Option<String> {
    e.body.payload.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn b(e: &OpEntry, key: &str) -> bool {
    e.body.payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn of_type<'a>(group: &[&'a OpEntry], types: &[&str]) -> Vec<&'a OpEntry> {
    group.iter().copied().filter(|e| types.contains(&e.body.op_type.as_str())).collect()
}

/// `uuidv5(memo_id, loser_op_id)`; a non-UUID memo id falls back to
/// `uuidv5(NAMESPACE_OID, memo_id ‖ loser_op_id)`.
pub(crate) fn conflict_copy_id(memo_id: &str, loser_op_id: &str) -> String {
    match uuid::Uuid::try_parse(memo_id) {
        Ok(ns) => uuid::Uuid::new_v5(&ns, loser_op_id.as_bytes()),
        Err(_) => uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, format!("{memo_id}{loser_op_id}").as_bytes()),
    }
    .to_string()
}

struct Out {
    entities: BTreeMap<EntityKey, EntityState>,
    notices: BTreeSet<Notice>,
}

impl Out {
    fn put(&mut self, kind: Kind, id: &str, fate: Fate, live: Option<EntityState>) {
        let state = match fate {
            Fate::Deleted => Some(EntityState::Deleted),
            Fate::Restored => {
                // Only a visible entity can be "restored".
                if live.is_some() {
                    self.notices.insert(Notice::RestoredAfterDeleteConflict { kind, entity_id: id.to_owned() });
                }
                live
            }
            Fate::Live => live,
        };
        if let Some(state) = state {
            self.entities.insert((kind, id.to_owned()), state);
        }
    }
}

pub(crate) fn project(set: &OpSet, forks: &Forks) -> Result<Projection, OpError> {
    let mut all_forks = forks.clone();
    for (d, start) in set.forks() {
        let slot = all_forks.entry(d.clone()).or_insert(*start);
        *slot = (*slot).min(*start);
    }
    let forks = &all_forks;

    let mut groups: BTreeMap<EntityKey, Vec<&OpEntry>> = BTreeMap::new();
    for e in set.iter() {
        groups.entry((e.body.kind, e.body.entity_id.clone())).or_default().push(e);
    }

    let mut out = Out { entities: BTreeMap::new(), notices: BTreeSet::new() };
    let memo_lineage = project_memos(set, &groups, forks, &mut out);
    for ((kind, id), group) in &groups {
        match kind {
            Kind::Memo => {}
            Kind::Conv => project_conv(id, group, forks, &mut out),
            Kind::Tr => project_tr(id, group, forks, &mut out),
            Kind::Policy => {
                if let Some(w) = winner(&of_type(group, &["policy.retention"]), forks) {
                    let state = EntityState::Policy(PolicyState { data: w.body.payload.clone() });
                    out.entities.insert((Kind::Policy, id.clone()), state);
                }
            }
        }
    }

    let hash = hash_entities(&out.entities)?;
    Ok(Projection { entities: out.entities, notices: out.notices.into_iter().collect(), memo_lineage, hash })
}

fn hash_entities(entities: &BTreeMap<EntityKey, EntityState>) -> Result<[u8; 32], OpError> {
    let mut map = Map::new();
    for ((kind, id), state) in entities {
        let value = serde_json::to_value(state).map_err(|e| OpError::Schema(e.to_string()))?;
        map.insert(format!("{}/{id}", kind.as_str()), value);
    }
    Ok(Sha256::digest(canonical_json(&Value::Object(map))?).into())
}

fn is_memo_node(e: &OpEntry) -> bool {
    e.body.kind == Kind::Memo && matches!(e.body.op_type.as_str(), "create" | "edit")
}

fn base_of(e: &OpEntry) -> Option<&str> {
    e.body.payload.get("base_op_id").and_then(Value::as_str)
}

/// Memo lineage forest (§4.4; plan-amendments "Memo lineage is
/// structural"). Nodes are memo creates and edits. A node's parent is its
/// `base_op_id` when that names another node and the node is not on a base
/// cycle; otherwise it is a root homed at its own `entity_id`. A create (no
/// base) is a main root. Any other root (missing base, a base naming a
/// non-lineage op such as a pin or note id, a self base, a base-cycle
/// member) has unknown lineage: an orphan of its own entity.
struct Forest<'a> {
    nodes: BTreeMap<&'a str, &'a OpEntry>,
    /// Node op id → its child nodes, in op id order.
    children: BTreeMap<&'a str, Vec<&'a OpEntry>>,
    /// Entity id → main roots homed there.
    roots: BTreeMap<&'a str, Vec<&'a OpEntry>>,
    /// Entity id → orphan roots homed there, in op id order.
    orphans: BTreeMap<&'a str, Vec<&'a OpEntry>>,
}

/// What a resolution hands one conflict copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Seed<'a> {
    /// Same-text losers the copy is walked from.
    starts: BTreeSet<&'a str>,
    /// Ops of losing orphan chains whose heads are starts: they show in the
    /// copy, so they are its content in fate accounting.
    carried: BTreeSet<&'a str>,
}

/// One entity's resolution: a pure function of (entity, starts, carried).
struct Resolved<'a> {
    /// The node whose text is shown; `None` leaves the entity without text.
    head: Option<&'a OpEntry>,
    /// Lineage ops this entity shows or supersedes.
    own: BTreeSet<&'a str>,
    /// Copy id → seed for every conflict copy this resolution starts.
    spawned: BTreeMap<String, Seed<'a>>,
    notices: BTreeSet<Notice>,
}

/// Entity id → (memo a conflict copy split from, resolution).
type Routed<'a> = BTreeMap<String, (Option<String>, Resolved<'a>)>;

/// Copy id → (memo it split from, seed).
type Seeds<'a> = BTreeMap<String, (String, Seed<'a>)>;

impl<'a> Forest<'a> {
    fn new(set: &'a OpSet) -> Self {
        let nodes: BTreeMap<&str, &OpEntry> =
            set.iter().filter(|e| is_memo_node(e)).map(|e| (e.body.op_id.as_str(), e)).collect();
        let up = |e: &'a OpEntry| base_of(e).filter(|b| *b != e.body.op_id && nodes.contains_key(b));
        // Base-cycle members: follow candidate parents from every node in op
        // id order; a walk that meets its own path closes a cycle.
        let mut on_cycle: BTreeSet<&str> = BTreeSet::new();
        let mut done: BTreeSet<&str> = BTreeSet::new();
        for &start in nodes.keys() {
            let mut path: Vec<&str> = Vec::new();
            let mut at = Some(start);
            while let Some(id) = at.filter(|id| !done.contains(id)) {
                if let Some(i) = path.iter().position(|p| *p == id) {
                    on_cycle.extend(&path[i..]);
                    break;
                }
                path.push(id);
                at = up(nodes[id]);
            }
            done.extend(path);
        }
        let mut children: BTreeMap<&str, Vec<&OpEntry>> = BTreeMap::new();
        let mut roots: BTreeMap<&str, Vec<&OpEntry>> = BTreeMap::new();
        let mut orphans: BTreeMap<&str, Vec<&OpEntry>> = BTreeMap::new();
        for &e in nodes.values() {
            let home = e.body.entity_id.as_str();
            match up(e).filter(|_| !on_cycle.contains(e.body.op_id.as_str())) {
                Some(parent) => children.entry(parent).or_default().push(e),
                None if base_of(e).is_none() => roots.entry(home).or_default().push(e),
                None => orphans.entry(home).or_default().push(e),
            }
        }
        Forest { nodes, children, roots, orphans }
    }

    /// Walks from `frontier`: at each step the winner continues and
    /// same-text losers merge back; losers with a different text start one
    /// conflict copy per distinct text (`spawn`). Returns the last winner
    /// and every op this walk kept in `id`.
    fn walk(
        &self,
        id: &str,
        mut frontier: Vec<&'a OpEntry>,
        forks: &Forks,
        r: &mut Resolved<'a>,
    ) -> (Option<&'a OpEntry>, Vec<&'a str>) {
        let (mut head, mut kept) = (None, Vec::new());
        // The forest is acyclic and each node has one parent, so every step
        // goes strictly down.
        while let Some(w) = winner(&frontier, forks) {
            head = Some(w);
            let mut next = Vec::new();
            let mut losers: BTreeMap<&str, Vec<&OpEntry>> = BTreeMap::new();
            for &e in &frontier {
                if e.body.op_id != w.body.op_id && s(e, "text") != s(w, "text") {
                    losers.entry(s(e, "text")).or_default().push(e);
                    continue;
                }
                kept.push(e.body.op_id.as_str());
                next.extend(self.children.get(e.body.op_id.as_str()).into_iter().flatten().copied());
            }
            for group in losers.into_values() {
                spawn(id, &group, &[], r);
            }
            frontier = next;
        }
        r.own.extend(&kept);
        (head, kept)
    }

    /// Resolves entity `id` (§4.4). Its main lineage is walked from its
    /// creates plus `starts`; each orphan homed at `id` is walked on its own.
    /// The heads then compete under the orphan rule: the causal winner is
    /// shown, and every head it did not see with a different text becomes a
    /// conflict copy (one per text, `spawn`), so no edit is invisible while
    /// its base is missing. A losing orphan head takes its whole chain along
    /// (`chain_of`), also when the chain was carried in; a losing main head
    /// moves alone. `carried` ops show here unless a spawn moved them out, so
    /// every lineage op is owned by exactly one entity.
    fn resolve(&self, id: &str, starts: Vec<&'a OpEntry>, carried: &BTreeSet<&'a str>, forks: &Forks) -> Resolved<'a> {
        let mut r = Resolved { head: None, own: BTreeSet::new(), spawned: BTreeMap::new(), notices: BTreeSet::new() };
        let mut heads: Vec<&OpEntry> = self.walk(id, starts, forks, &mut r).0.into_iter().collect();
        let mut chains: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for &orphan in self.orphans.get(id).into_iter().flatten() {
            if let (Some(head), kept) = self.walk(id, vec![orphan], forks, &mut r) {
                chains.insert(head.body.op_id.as_str(), kept);
                heads.push(head);
            }
        }
        if let Some(w) = winner(&heads, forks) {
            r.head = Some(w);
            let mut losers: BTreeMap<&str, Vec<&OpEntry>> = BTreeMap::new();
            for &h in &heads {
                if h.body.op_id != w.body.op_id && s(h, "text") != s(w, "text") && !saw(w, h, forks) {
                    losers.entry(s(h, "text")).or_default().push(h);
                }
            }
            for group in losers.into_values() {
                let moved: Vec<&str> = group.iter().flat_map(|h| self.chain_of(h, &chains, carried)).collect();
                spawn(id, &group, &moved, &mut r);
            }
        }
        // Every lineage op has one owner: what a spawn moved out leaves here,
        // including a carried chain whose head this entity no longer shows.
        let moved_out: BTreeSet<&str> =
            r.spawned.values().flat_map(|seed| seed.starts.iter().chain(&seed.carried)).copied().collect();
        r.own.retain(|op| !moved_out.contains(op));
        r.own.extend(carried.iter().copied().filter(|op| !moved_out.contains(op)));
        r
    }

    /// The chain a losing head `h` takes to its copy: the orphan chain walked
    /// here when `h` heads one, or else, when `h` was carried in, every
    /// carried op of the carried chain it heads (same forest root within
    /// `carried`). A main head that is neither moves alone.
    fn chain_of(&self, h: &'a OpEntry, chains: &BTreeMap<&'a str, Vec<&'a str>>, carried: &BTreeSet<&'a str>) -> Vec<&'a str> {
        let h_id = h.body.op_id.as_str();
        if let Some(chain) = chains.get(h_id) {
            return chain.clone();
        }
        if !carried.contains(h_id) {
            return Vec::new();
        }
        let root = |mut op: &'a str| {
            while let Some(parent) = self.nodes.get(op).and_then(|e| base_of(e)).filter(|b| {
                carried.contains(b) && self.children.get(b).is_some_and(|kids| kids.iter().any(|k| k.body.op_id == op))
            }) {
                op = parent;
            }
            op
        };
        let home = root(h_id);
        carried.iter().copied().filter(|op| root(op) == home).collect()
    }

    /// Resolves every entity that has roots, orphans or a seed.
    fn resolve_all(&self, seeds: &Seeds<'a>, forks: &Forks) -> Routed<'a> {
        let mut ids: BTreeSet<&str> = self.roots.keys().chain(self.orphans.keys()).copied().collect();
        ids.extend(seeds.keys().map(String::as_str));
        let no_carry = BTreeSet::new();
        ids.into_iter()
            .map(|id| {
                let seed = seeds.get(id);
                let mut starts: BTreeSet<&str> =
                    self.roots.get(id).into_iter().flatten().map(|e| e.body.op_id.as_str()).collect();
                starts.extend(seed.into_iter().flat_map(|(_, s)| s.starts.iter().copied()));
                let starts = starts.into_iter().filter_map(|op| self.nodes.get(op).copied()).collect();
                let carried = seed.map_or(&no_carry, |(_, s)| &s.carried);
                (id.to_owned(), (seed.map(|(from, _)| from.clone()), self.resolve(id, starts, carried, forks)))
            })
            .collect()
    }
}

/// Starts (or extends) the conflict copy of `from` for one group of
/// same-text losers, named after the smallest loser op id (§4.4 / AC 2).
fn spawn<'a>(from: &str, group: &[&'a OpEntry], carried: &[&'a str], r: &mut Resolved<'a>) {
    let Some(first) = group.iter().map(|e| e.body.op_id.as_str()).min() else { return };
    let copy = conflict_copy_id(from, first);
    r.notices.insert(Notice::MemoConflictCopy { memo_id: from.to_owned(), copy_id: copy.clone(), loser_op_id: first.to_owned() });
    let seed = r.spawned.entry(copy).or_default();
    seed.starts.extend(group.iter().map(|e| e.body.op_id.as_str()));
    seed.carried.extend(carried);
}

/// Every copy seed the resolutions start. Should two memos ever name the
/// same copy id, the smaller memo id is its source and the seeds merge.
fn seeds_of<'a>(routed: &Routed<'a>) -> Seeds<'a> {
    let mut seeds: Seeds<'a> = BTreeMap::new();
    for (id, (_, r)) in routed {
        for (copy, seed) in &r.spawned {
            let slot = seeds.entry(copy.clone()).or_insert_with(|| (id.clone(), Seed::default()));
            slot.1.starts.extend(&seed.starts);
            slot.1.carried.extend(&seed.carried);
        }
    }
    seeds
}

/// Routes every memo lineage op to the one memo or conflict copy that
/// shows or supersedes it. The result is the fixpoint of "resolve every
/// entity from the seeds the previous round produced", recomputed from
/// scratch each round, so nothing depends on visit order or on an earlier
/// round. Copy ids are `uuidv5(source, op)`, so the copy relation is acyclic
/// and the seeds settle after one round per copy level; the bound only
/// guards termination.
fn route_memos<'a>(set: &'a OpSet, forks: &Forks) -> Routed<'a> {
    let forest = Forest::new(set);
    let mut seeds: Seeds<'a> = BTreeMap::new();
    let mut routed = forest.resolve_all(&seeds, forks);
    let mut converged = false;
    for _ in 0..=forest.nodes.len() {
        let next = seeds_of(&routed);
        if next == seeds {
            converged = true;
            break;
        }
        seeds = next;
        routed = forest.resolve_all(&seeds, forks);
    }
    debug_assert!(converged, "memo routing must reach a fixpoint within one round per copy level");
    routed
}

/// Memo text lineage (§4.4): creates and edits form a forest by
/// `base_op_id` (`Forest`). At each step the concurrent children of the
/// current head compete; the winner continues the memo, a loser with
/// different text starts a conflict copy that its own descendants follow,
/// and a loser with the same text merges back. Returns the lineage op →
/// memo id routing.
///
/// An edit whose base is not a followable node (missing, not a create or
/// edit, itself, or on a base cycle) has unknown lineage: it is walked on
/// its own and competes as a head of the memo it addresses (`Forest::
/// resolve`). Once the base arrives it competes on it as an ordinary
/// same-base child.
///
/// Lineage is purely structural (plan-amendments "Memo lineage is
/// structural"): a base that is any memo's create or edit is followed,
/// whatever `entity_id` the edit carries, so an honest edit of a conflict
/// copy that names the source memo's lineage op (`head_op_id`) competes on
/// it (§4.4), and a forged cross-memo base deterministically and visibly
/// joins that memo's lineage. `entity_id` only homes a root. An edit that
/// names a base has seen it by construction; causality across a fork range
/// is decided by the edit-vs-delete rule (`fate`), not by lineage.
fn project_memos(
    set: &OpSet,
    groups: &BTreeMap<EntityKey, Vec<&OpEntry>>,
    forks: &Forks,
    out: &mut Out,
) -> BTreeMap<String, String> {
    let routed = route_memos(set, forks);
    let mut lineage = BTreeMap::new();
    for (id, (_, r)) in &routed {
        out.notices.extend(r.notices.iter().cloned());
        lineage.extend(r.own.iter().map(|op| ((*op).to_owned(), id.clone())));
    }
    let mut ids: BTreeSet<&str> = routed.keys().map(String::as_str).collect();
    ids.extend(groups.keys().filter(|(k, _)| *k == Kind::Memo).map(|(_, id)| id.as_str()));

    for id in ids {
        let group = groups.get(&(Kind::Memo, id.to_owned())).map(Vec::as_slice).unwrap_or_default();
        let deletes = of_type(group, &["delete"]);
        let notes = of_type(group, &["note"]);
        let resolved = routed.get(id);
        let own: Vec<&OpEntry> =
            resolved.into_iter().flat_map(|(_, r)| r.own.iter().filter_map(|op| set.get(op))).collect();
        let mut content = own.clone();
        content.extend(notes.iter().copied());
        let live = resolved.and_then(|(conflict_of, r)| {
            let head = r.head?;
            let pins: Vec<&OpEntry> = group
                .iter()
                .copied()
                .filter(|e| e.body.op_type == "pin")
                .chain(own.iter().copied().filter(|e| e.body.op_type == "create"))
                .collect();
            Some(EntityState::Memo(MemoState {
                text: s(head, "text").to_owned(),
                pinned: winner(&pins, forks).is_some_and(|w| b(w, "pinned")),
                note: winner(&notes, forks).map(|w| s(w, "note").to_owned()).unwrap_or_default(),
                head_op_id: head.body.op_id.clone(),
                conflict_of: conflict_of.clone(),
            }))
        });
        out.put(Kind::Memo, id, fate(&content, &deletes, forks), live);
    }
    lineage
}

/// Test oracle for "no invisible edit": memo creates and edits whose text
/// is neither shown nor superseded within its own lineage. Every node must
/// be routed (`memo_lineage`) to an existing entity, and unless that entity
/// is a tombstone the node must be (a) its owner's head, (b) the base of
/// another node (`Forest` parent: it was edited further), (c) a same-text
/// sibling (same forest parent, or same home for roots) of such a node or of
/// a live memo's head (it merged back into that text), (d) seen by its
/// owner's head, or (e) equal to its owner's shown text.
/// Returns the failing op ids.
#[cfg(test)]
pub(crate) fn invisible_memo_edits(set: &OpSet, p: &Projection) -> Vec<String> {
    let forest = Forest::new(set);
    let parent: BTreeMap<&str, &str> =
        forest.children.iter().flat_map(|(base, kids)| kids.iter().map(move |k| (k.body.op_id.as_str(), *base))).collect();
    let place = |e: &OpEntry| match parent.get(e.body.op_id.as_str()) {
        Some(base) => format!("child:{base}"),
        None => format!("root:{}", e.body.entity_id),
    };
    let edited = |e: &OpEntry| forest.children.contains_key(e.body.op_id.as_str());
    let forks: Forks = set.forks().clone();
    let heads: BTreeSet<&str> = p
        .entities
        .values()
        .filter_map(|e| match e {
            EntityState::Memo(m) => Some(m.head_op_id.as_str()),
            _ => None,
        })
        .collect();
    forest
        .nodes
        .values()
        .filter(|e| {
            let Some(owner) = p.memo_lineage.get(&e.body.op_id) else { return true };
            match p.entities.get(&(Kind::Memo, owner.clone())) {
                Some(EntityState::Deleted) => false,
                Some(EntityState::Memo(m)) => {
                    let shown = m.head_op_id == e.body.op_id
                        || edited(e)
                        || forest.nodes.values().any(|d| {
                            d.body.op_id != e.body.op_id
                                && (edited(d) || heads.contains(d.body.op_id.as_str()))
                                && s(d, "text") == s(e, "text")
                                && place(d) == place(e)
                        })
                        || set.get(&m.head_op_id).is_some_and(|h| saw(h, e, &forks))
                        || m.text == s(e, "text");
                    !shown
                }
                _ => true,
            }
        })
        .map(|e| e.body.op_id.clone())
        .collect()
}

/// Messages form a tree by `parent_id`; a regenerate is a sibling. The active
/// leaf is the `set_active` register, else the newest leaf.
fn project_conv(id: &str, group: &[&OpEntry], forks: &Forks, out: &mut Out) {
    let deletes = of_type(group, &["delete"]);
    let content = of_type(group, &["create", "rename", "msg.add"]);
    let mut by_msg: BTreeMap<&str, Vec<&OpEntry>> = BTreeMap::new();
    for e in of_type(group, &["msg.add"]) {
        by_msg.entry(s(e, "msg_id")).or_default().push(e);
    }
    let messages: BTreeMap<String, Message> = by_msg
        .into_iter()
        .filter_map(|(msg_id, cands)| {
            let w = winner(&cands, forks)?;
            let p = &w.body.payload;
            Some((
                msg_id.to_owned(),
                Message {
                    parent_id: opt_s(w, "parent_id"),
                    role: s(w, "role").to_owned(),
                    content: s(w, "content").to_owned(),
                    model: opt_s(w, "model"),
                    attachments: p.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default(),
                    ts: p.get("ts").and_then(Value::as_u64).unwrap_or(0),
                    op_id: w.body.op_id.clone(),
                },
            ))
        })
        .collect();
    let live = if content.is_empty() {
        None
    } else {
        let title = winner(&of_type(group, &["create", "rename"]), forks).map(|w| s(w, "title").to_owned());
        let chosen = winner(&of_type(group, &["set_active"]), forks)
            .map(|w| s(w, "leaf").to_owned())
            .filter(|leaf| messages.contains_key(leaf));
        let parents: BTreeSet<&str> = messages.values().filter_map(|m| m.parent_id.as_deref()).collect();
        let newest_leaf = messages
            .iter()
            .filter(|(msg_id, _)| !parents.contains(msg_id.as_str()))
            .max_by(|(ia, ma), (ib, mb)| (ma.ts, ia).cmp(&(mb.ts, ib)))
            .map(|(msg_id, _)| msg_id.clone());
        Some(EntityState::Conv(ConvState {
            title: title.unwrap_or_default(),
            active_leaf: chosen.or(newest_leaf),
            messages,
        }))
    };
    out.put(Kind::Conv, id, fate(&content, &deletes, forks), live);
}

fn project_tr(id: &str, group: &[&OpEntry], forks: &Forks, out: &mut Out) {
    let deletes = of_type(group, &["tr.delete"]);
    let adds = of_type(group, &["tr.add"]);
    let live = winner(&adds, forks).map(|w| {
        let pinned = winner(&of_type(group, &["tr.add", "tr.pin"]), forks).is_some_and(|p| b(p, "pinned"));
        EntityState::Tr(TrState { data: w.body.payload.clone(), pinned })
    });
    out.put(Kind::Tr, id, fate(&adds, &deletes, forks), live);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::causal::Insert;
    use crate::sync::crypto::hex;
    use crate::sync::ops::{Dot, Hlc, OpBody};
    use proptest::prelude::*;
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    use serde_json::json;

    const MEMO: &str = "0190a000-0000-7000-8000-00000000000a";
    const MIN: u64 = 60 * 1000;

    fn oid(n: u64) -> String {
        let mut rand = [0u8; 10];
        rand[2..].copy_from_slice(&n.to_be_bytes());
        uuid::Builder::from_unix_timestamp_millis(1_700_000_000_000, &rand).into_uuid().to_string()
    }

    fn dot(device: &str, seq: u64) -> Dot {
        Dot { device: device.to_owned(), seq }
    }

    /// One op as published: body plus the dot it arrived at.
    #[derive(Clone)]
    struct Pub(OpBody, Dot);

    fn op(
        n: u64,
        at: (&str, u64),
        wall: u64,
        ctx: &[(&str, u64)],
        (kind, entity, op_type): (Kind, &str, &str),
        payload: Value,
    ) -> Pub {
        let body = OpBody {
            v: 1,
            op_id: oid(n),
            origin_device: at.0.to_owned(),
            hlc: Hlc { wall_ms: wall, ctr: 0 },
            ctx: ctx.iter().map(|(d, s)| ((*d).to_owned(), *s)).collect(),
            obs: Vec::new(),
            kind,
            entity_id: entity.to_owned(),
            op_type: op_type.to_owned(),
            payload,
        };
        assert_eq!(body.validate(), Ok(()), "test op {n} must be valid");
        Pub(body, dot(at.0, at.1))
    }

    fn read(ops: &[&Pub]) -> (OpSet, Projection) {
        let mut set = OpSet::new();
        for Pub(body, d) in ops {
            let got = set.insert(body.clone(), d.clone()).unwrap();
            assert_ne!(got.outcome, Insert::Conflict);
        }
        let projection = project(&set, &Forks::new()).unwrap();
        (set, projection)
    }

    fn memo(p: &Projection, id: &str) -> Option<MemoState> {
        match p.entities.get(&(Kind::Memo, id.to_owned())) {
            Some(EntityState::Memo(m)) => Some(m.clone()),
            _ => None,
        }
    }

    fn is_deleted(p: &Projection, kind: Kind, id: &str) -> bool {
        p.entities.get(&(kind, id.to_owned())) == Some(&EntityState::Deleted)
    }

    fn restored(p: &Projection, kind: Kind, id: &str) -> bool {
        p.notices.contains(&Notice::RestoredAfterDeleteConflict { kind, entity_id: id.to_owned() })
    }

    fn create(n: u64, at: (&str, u64), wall: u64, ctx: &[(&str, u64)], text: &str) -> Pub {
        op(n, at, wall, ctx, (Kind::Memo, MEMO, "create"), json!({"text": text, "pinned": false}))
    }

    fn edit(n: u64, at: (&str, u64), wall: u64, ctx: &[(&str, u64)], base: u64, text: &str) -> Pub {
        op(n, at, wall, ctx, (Kind::Memo, MEMO, "edit"), json!({"text": text, "base_op_id": oid(base)}))
    }

    fn delete(n: u64, at: (&str, u64), wall: u64, ctx: &[(&str, u64)], entity: &str) -> Pub {
        op(n, at, wall, ctx, (Kind::Memo, entity, "delete"), json!({}))
    }

    #[test]
    fn conflict_copy_ids_match_python_uuid5() {
        // python3 -c 'import uuid;print(uuid.uuid5(uuid.UUID("0190a000-0000-7000-8000-00000000000a"),"0190a000-0000-7000-8000-00000000000b"));print(uuid.uuid5(uuid.NAMESPACE_OID,"memo-1"+"0190a000-0000-7000-8000-00000000000b"))'
        let loser = "0190a000-0000-7000-8000-00000000000b";
        assert_eq!(conflict_copy_id(MEMO, loser), "9d91431e-0d8c-5b0d-806b-ffeb74657e4b");
        assert_eq!(conflict_copy_id("memo-1", loser), "8dc44ab2-63f0-5a22-9985-152f029d3ed0");
    }

    #[test]
    fn ac2_concurrent_edits_on_one_base_give_one_winner_and_one_copy() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let e1 = edit(2, ("a", 1), 200, &[("b", 1)], 1, "from a");
        let e2 = edit(3, ("b", 2), 300, &[("b", 1)], 1, "from b");
        let (_, p) = read(&[&c, &e1, &e2]);
        let m = memo(&p, MEMO).unwrap();
        assert_eq!((m.text.as_str(), m.head_op_id.clone()), ("from b", oid(3)), "max hlc wins the body");
        let copy_id = conflict_copy_id(MEMO, &oid(2));
        let copy = memo(&p, &copy_id).unwrap();
        assert_eq!(copy.text, "from a");
        assert_eq!(copy.conflict_of.as_deref(), Some(MEMO));
        let memos = p.entities.keys().filter(|(k, _)| *k == Kind::Memo).count();
        assert_eq!(memos, 2, "exactly one winner plus one copy");
        assert_eq!(
            p.notices,
            vec![Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: copy_id.clone(), loser_op_id: oid(2) }]
        );
        assert_eq!(p.memo_lineage[&oid(2)], copy_id);
        assert_eq!(p.memo_lineage[&oid(3)], MEMO);

        // A later edit on the loser's lineage goes to the copy, not the memo.
        let e4 = edit(4, ("a", 2), 400, &[("a", 1), ("b", 1)], 2, "a again");
        let (_, p) = read(&[&e4, &e2, &c, &e1]);
        assert_eq!(memo(&p, MEMO).unwrap().text, "from b");
        assert_eq!(memo(&p, &copy_id).unwrap().text, "a again");
        assert_eq!(p.memo_lineage[&oid(4)], copy_id);
    }

    #[test]
    fn ac2_identical_concurrent_text_makes_no_copy() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let e1 = edit(2, ("a", 1), 200, &[("b", 1)], 1, "same");
        let e2 = edit(3, ("b", 2), 300, &[("b", 1)], 1, "same");
        let after_loser = edit(4, ("a", 2), 400, &[("a", 1), ("b", 1)], 2, "next");
        let (_, p) = read(&[&c, &e1, &e2, &after_loser]);
        assert_eq!(p.entities.len(), 1, "no conflict copy");
        assert!(p.notices.is_empty());
        assert_eq!(memo(&p, MEMO).unwrap().text, "next", "the merged branch keeps going");
    }

    #[test]
    fn ac3_unseen_edit_beats_delete_and_a_delete_that_saw_it_wins() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let e = edit(2, ("a", 1), 200, &[("b", 1)], 1, "edited");
        let blind = delete(3, ("b", 2), 300, &[("b", 1)], MEMO);
        let (_, p) = read(&[&c, &e, &blind]);
        assert_eq!(memo(&p, MEMO).unwrap().text, "edited");
        assert!(restored(&p, Kind::Memo, MEMO));

        let seeing = delete(3, ("b", 2), 300, &[("a", 1), ("b", 1)], MEMO);
        let (_, p) = read(&[&c, &e, &seeing]);
        assert!(is_deleted(&p, Kind::Memo, MEMO));
        assert!(p.notices.is_empty());

        // An op that saw the delete does not restore it; registers never block.
        let late_pin = op(4, ("a", 2), 400, &[("a", 1), ("b", 2)], (Kind::Memo, MEMO, "pin"), json!({"pinned": true}));
        let concurrent_pin = op(5, ("c", 1), 500, &[("b", 1)], (Kind::Memo, MEMO, "pin"), json!({"pinned": true}));
        let (_, p) = read(&[&c, &e, &seeing, &late_pin, &concurrent_pin]);
        assert!(is_deleted(&p, Kind::Memo, MEMO));
        let late_note = op(6, ("a", 2), 400, &[("a", 1), ("b", 2)], (Kind::Memo, MEMO, "note"), json!({"note": "n"}));
        let (_, p) = read(&[&c, &e, &seeing, &late_note]);
        assert!(is_deleted(&p, Kind::Memo, MEMO), "content that saw the delete is ignored");
    }

    #[test]
    fn ac3b_clock_skewed_delete_does_not_beat_an_unseen_edit() {
        let c = create(1, ("b", 1), 1_000, &[], "base");
        let e = edit(2, ("a", 1), 2_000, &[("b", 1)], 1, "a's edit");
        // B's clock runs 30 minutes fast; it deletes without having seen A's edit.
        let d = delete(3, ("b", 2), 1_000 + 30 * MIN, &[("b", 1)], MEMO);
        let pure_hlc_deletes = d.0.hlc > e.0.hlc;
        assert!(pure_hlc_deletes, "a last-writer-wins-by-HLC rule would delete");
        let (_, p) = read(&[&c, &e, &d]);
        assert_eq!(memo(&p, MEMO).unwrap().text, "a's edit");
        assert!(restored(&p, Kind::Memo, MEMO));
    }

    #[test]
    fn registers_follow_causality_before_the_hlc() {
        let c = create(1, ("b", 1), 1_000, &[], "base");
        let fast_pin = op(2, ("b", 2), 9_000_000, &[("b", 1)], (Kind::Memo, MEMO, "pin"), json!({"pinned": true}));
        let later_unpin = op(3, ("a", 1), 2_000, &[("b", 2)], (Kind::Memo, MEMO, "pin"), json!({"pinned": false}));
        let (_, p) = read(&[&c, &fast_pin, &later_unpin]);
        assert!(!memo(&p, MEMO).unwrap().pinned, "causally later unpin wins over a bigger HLC");
        let concurrent_unpin = op(3, ("a", 1), 2_000, &[("b", 1)], (Kind::Memo, MEMO, "pin"), json!({"pinned": false}));
        let (_, p) = read(&[&c, &fast_pin, &concurrent_unpin]);
        assert!(memo(&p, MEMO).unwrap().pinned, "concurrent: max HLC wins");
    }

    fn msg(n: u64, at: (&str, u64), ctx: &[(&str, u64)], msg_id: &str, parent: Option<&str>, ts: u64) -> Pub {
        let mut payload = json!({"conv_id": "conv-1", "msg_id": msg_id, "role": "assistant",
                                 "content": format!("c{n}"), "attachments": [], "ts": ts});
        if let Some(parent) = parent {
            payload["parent_id"] = json!(parent);
        }
        op(n, at, ts, ctx, (Kind::Conv, "conv-1", "msg.add"), payload)
    }

    #[test]
    fn conversations_keep_regenerated_siblings_and_pick_the_newest_leaf() {
        let c = op(1, ("a", 1), 10, &[], (Kind::Conv, "conv-1", "create"), json!({"title": "t"}));
        let root = msg(2, ("a", 2), &[("a", 1)], "m1", None, 20);
        let r1 = msg(3, ("a", 3), &[("a", 2)], "m2", Some("m1"), 30);
        let r2 = msg(4, ("b", 1), &[("a", 2)], "m3", Some("m1"), 40);
        let rename = op(5, ("b", 2), 50, &[("a", 1), ("b", 1)], (Kind::Conv, "conv-1", "rename"), json!({"title": "u"}));
        let (_, p) = read(&[&r2, &rename, &root, &c, &r1]);
        let Some(EntityState::Conv(conv)) = p.entities.get(&(Kind::Conv, "conv-1".to_owned())) else {
            panic!("conversation missing")
        };
        assert_eq!(conv.title, "u");
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages["m2"].parent_id, conv.messages["m3"].parent_id);
        assert_eq!(conv.active_leaf.as_deref(), Some("m3"), "newest leaf by default");

        let pick = op(6, ("a", 4), 60, &[("a", 3), ("b", 1)], (Kind::Conv, "conv-1", "set_active"), json!({"leaf": "m2"}));
        let (_, p) = read(&[&c, &root, &r1, &r2, &pick]);
        let Some(EntityState::Conv(conv)) = p.entities.get(&(Kind::Conv, "conv-1".to_owned())) else {
            panic!("conversation missing")
        };
        assert_eq!(conv.active_leaf.as_deref(), Some("m2"));

        let blind = op(7, ("c", 1), 70, &[("a", 2)], (Kind::Conv, "conv-1", "delete"), json!({}));
        let (_, p) = read(&[&c, &root, &r1, &r2, &blind]);
        assert!(restored(&p, Kind::Conv, "conv-1"), "unseen regenerations survive a delete");
        let full = op(7, ("c", 1), 70, &[("a", 3), ("b", 1)], (Kind::Conv, "conv-1", "delete"), json!({}));
        let (_, p) = read(&[&c, &root, &r1, &r2, &full]);
        assert!(is_deleted(&p, Kind::Conv, "conv-1"));
    }

    #[test]
    fn translations_follow_the_same_rules() {
        let add = op(1, ("a", 1), 10, &[], (Kind::Tr, "tr-1", "tr.add"), json!({"src": "a", "dst": "b"}));
        let pin = op(2, ("b", 1), 20, &[("a", 1)], (Kind::Tr, "tr-1", "tr.pin"), json!({"pinned": true}));
        let (_, p) = read(&[&pin, &add]);
        assert_eq!(
            p.entities[&(Kind::Tr, "tr-1".to_owned())],
            EntityState::Tr(TrState { data: json!({"src": "a", "dst": "b"}), pinned: true })
        );
        let del = op(3, ("c", 1), 30, &[("a", 1)], (Kind::Tr, "tr-1", "tr.delete"), json!({}));
        let (_, p) = read(&[&add, &pin, &del]);
        assert!(is_deleted(&p, Kind::Tr, "tr-1"), "a pin never blocks a delete");
        let blind = op(3, ("c", 1), 30, &[], (Kind::Tr, "tr-1", "tr.delete"), json!({}));
        let (_, p) = read(&[&add, &blind]);
        assert!(restored(&p, Kind::Tr, "tr-1"));
    }

    #[test]
    fn ac16c_common_op_published_by_original_and_clone_is_one_logical_op() {
        let c = create(1, ("b", 1), 100, &[], "base");
        // Written by A before the clone; A publishes it as (a,5), the clone
        // restamps it to (a2,1) with its ctx cut back to S.
        let common = edit(2, ("a", 5), 200, &[("a", 4), ("b", 1)], 1, "common");
        let mut restamped = common.clone();
        restamped.0.origin_device = "a2".to_owned();
        restamped.0.ctx = BTreeMap::from([("a".to_owned(), 3), ("b".to_owned(), 1)]);
        restamped.1 = dot("a2", 1);

        let (set, both) = read(&[&c, &restamped, &common]);
        let (_, original_only) = read(&[&common, &c]);
        let (_, clone_only) = read(&[&c, &restamped]);
        assert_eq!(set.len(), 2);
        assert_eq!(set.get(&oid(2)).unwrap().dots, BTreeSet::from([dot("a", 5), dot("a2", 1)]));
        assert!(set.forks().is_empty(), "an alias is not a fork");
        assert_eq!(both.entities.len(), 1, "no duplicate memo");
        assert!(both.notices.is_empty(), "no conflict copy");
        assert_eq!(memo(&both, MEMO).unwrap().text, "common");
        assert_eq!(both.hash, original_only.hash);
        assert_eq!(both.hash, clone_only.hash);
    }

    #[test]
    fn ac19_ac19b_fork_variants_converge_and_blind_deletes_do_not_count() {
        // Device b created the memo; a (and its VM clone, also named "a")
        // each wrote a different op at the same dot (a,1).
        let c = create(1, ("b", 1), 100, &[], "base");
        let v1 = edit(2, ("a", 1), 200, &[("b", 1)], 1, "variant one");
        let v2 = edit(3, ("a", 1), 210, &[("b", 1)], 1, "variant two");
        // Reader b saw only v1 first and deleted; ctx {a:1} names the forked dot.
        let blind_delete = delete(4, ("b", 2), 300, &[("a", 1), ("b", 1)], MEMO);

        let (_, before_fork) = read(&[&c, &v1, &blind_delete]);
        assert!(is_deleted(&before_fork, Kind::Memo, MEMO), "b alone thinks it saw everything");

        // Three readers see the same objects in different list orders.
        let reader_b = read(&[&c, &v1, &blind_delete, &v2]);
        let reader_c = read(&[&v2, &c, &blind_delete, &v1]);
        let reader_a = read(&[&blind_delete, &v2, &v1, &c]);
        for (set, p) in [&reader_b, &reader_c, &reader_a] {
            assert_eq!(set.forks(), &Forks::from([("a".to_owned(), 1)]), "fork detected");
            assert_eq!(p.hash, reader_b.1.hash, "all readers converge");
        }
        let p = &reader_b.1;
        assert!(restored(p, Kind::Memo, MEMO), "a delete without obs did not see fork-range ops");
        let texts: BTreeSet<String> = p
            .entities
            .values()
            .filter_map(|s| match s {
                EntityState::Memo(m) => Some(m.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, BTreeSet::from(["variant one".to_owned(), "variant two".to_owned()]), "no edit lost");
        assert_eq!(memo(p, MEMO).unwrap().text, "variant two");
        let copy_id = conflict_copy_id(MEMO, &oid(2));

        // Without the fork the same delete's ctx would have covered both.
        let unforked = project(&reader_b.0, &Forks::new()).unwrap();
        assert_eq!(unforked.hash, p.hash, "set forks are always applied");

        // After learning of the fork, b re-deletes with obs naming the fork-range ops.
        let mut redelete = delete(5, ("b", 3), 400, &[("a", 1), ("b", 2)], MEMO);
        redelete.0.obs = vec![oid(2), oid(3)];
        redelete.0.validate().unwrap();
        let mut copy_delete = delete(6, ("b", 4), 410, &[("a", 1), ("b", 3)], &copy_id);
        copy_delete.0.obs = vec![oid(2)];
        copy_delete.0.validate().unwrap();
        let all = [&c, &v1, &v2, &blind_delete, &redelete, &copy_delete];
        let orders: [[usize; 6]; 3] = [[0, 1, 2, 3, 4, 5], [5, 4, 2, 0, 3, 1], [2, 3, 5, 1, 4, 0]];
        let projections: Vec<Projection> =
            orders.iter().map(|o| read(&o.map(|i| all[i])).1).collect();
        for p in &projections {
            assert_eq!(p.hash, projections[0].hash);
            assert!(is_deleted(p, Kind::Memo, MEMO), "the re-delete with obs converges to deleted");
            assert!(is_deleted(p, Kind::Memo, &copy_id));
            assert!(p.notices.iter().all(|n| !matches!(n, Notice::RestoredAfterDeleteConflict { .. })));
        }

        // A fork the store recorded elsewhere (two-object variant) is honoured too.
        let (set, _) = read(&[&c, &v1, &blind_delete]);
        let external = Forks::from([("a".to_owned(), 1)]);
        assert!(restored(&project(&set, &external).unwrap(), Kind::Memo, MEMO));
    }

    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut all = Vec::new();
        for rest in permutations(n - 1) {
            for at in 0..=rest.len() {
                let mut p = rest.clone();
                p.insert(at, n - 1);
                all.push(p);
            }
        }
        all
    }

    /// Projects `ops` in every arrival order and returns the common projection.
    fn read_all_orders(ops: &[&Pub]) -> Projection {
        let first = read(ops).1;
        for order in permutations(ops.len()) {
            let shuffled: Vec<&Pub> = order.iter().map(|&i| ops[i]).collect();
            assert_eq!(read(&shuffled).1, first, "order {order:?}");
        }
        first
    }

    fn memo_count(p: &Projection) -> usize {
        p.entities.keys().filter(|(k, _)| *k == Kind::Memo).count()
    }

    /// Test view of `route_memos`: lineage op → every entity whose
    /// resolution owns it.
    fn lineage_owners(set: &OpSet) -> BTreeMap<String, Vec<String>> {
        let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, (_, r)) in route_memos(set, set.forks()) {
            for op in &r.own {
                owners.entry((*op).to_owned()).or_default().push(id.clone());
            }
        }
        owners
    }

    /// Every memo create/edit is owned by exactly one entity.
    fn assert_single_owner(set: &OpSet, what: &str) {
        let owners = lineage_owners(set);
        let doubled: Vec<_> = owners.iter().filter(|(_, ids)| ids.len() != 1).collect();
        assert!(doubled.is_empty(), "{what}: lineage ops owned more than once {doubled:?}");
        let nodes: BTreeSet<&str> = set.iter().filter(|e| is_memo_node(e)).map(|e| e.body.op_id.as_str()).collect();
        assert_eq!(owners.keys().map(String::as_str).collect::<BTreeSet<_>>(), nodes, "{what}: every lineage op is owned");
    }

    #[test]
    fn orphan_base_edit_makes_no_copy_and_converges_once_the_base_arrives() {
        // C applied B's edit E (base X from A) before A's segment with X.
        let c = create(1, ("b", 1), 100, &[], "base");
        let x = edit(2, ("a", 1), 200, &[("b", 1)], 1, "x");
        let e = edit(3, ("b", 2), 300, &[("a", 1), ("b", 1)], 2, "e");

        let partial = read_all_orders(&[&c, &e]);
        assert!(partial.notices.is_empty(), "no spurious copy notice: {:?}", partial.notices);
        assert_eq!(memo_count(&partial), 1, "the create is not turned into a copy");
        let m = memo(&partial, MEMO).unwrap();
        assert_eq!((m.text.as_str(), m.head_op_id.as_str(), m.conflict_of), ("e", oid(3).as_str(), None));
        assert_eq!(partial.memo_lineage[&oid(1)], MEMO);
        assert_eq!(partial.memo_lineage[&oid(3)], MEMO);

        let full = read_all_orders(&[&c, &x, &e]);
        assert!(full.notices.is_empty());
        assert_eq!(full.hash, partial.hash, "the base arriving changes nothing visible");

        // The orphan still counts as content: a delete that did not see it is restored.
        let blind = delete(4, ("c", 1), 400, &[("b", 1)], MEMO);
        let p = read_all_orders(&[&c, &e, &blind]);
        assert!(restored(&p, Kind::Memo, MEMO));
        assert_eq!(memo(&p, MEMO).unwrap().text, "e");
        assert_eq!(p.hash, read_all_orders(&[&c, &x, &e, &blind]).hash);
        let seeing = delete(4, ("c", 1), 400, &[("a", 1), ("b", 2)], MEMO);
        assert!(is_deleted(&read_all_orders(&[&c, &e, &seeing]), Kind::Memo, MEMO));

        // An orphan alone (create not applied yet either) shows as the memo.
        let alone = read(&[&e]).1;
        assert!(alone.notices.is_empty());
        assert_eq!(memo(&alone, MEMO).unwrap().text, "e");
    }

    /// Leader decision p2_o3: while a base is missing, every orphan head that
    /// loses to the memo's current head (and was not seen by it) is a conflict
    /// copy; once the base arrives, ordinary same-base competition applies.
    #[test]
    fn orphan_edits_on_one_missing_base_compete_only_once_it_arrives() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let x = edit(2, ("a", 1), 200, &[("b", 1)], 1, "x");
        let e1 = edit(3, ("b", 2), 300, &[("a", 1), ("b", 1)], 2, "e1");
        let e2 = edit(4, ("c", 1), 400, &[("a", 1), ("b", 1)], 2, "e2");
        let copy = conflict_copy_id(MEMO, &oid(3));
        let notice = vec![Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: copy.clone(), loser_op_id: oid(3) }];

        let partial = read_all_orders(&[&c, &e1, &e2]);
        assert_eq!(partial.notices, notice, "the losing orphan is a copy while its base is missing");
        assert_eq!(memo_count(&partial), 2);
        assert_eq!(memo(&partial, MEMO).unwrap().text, "e2", "concurrent orphans: max HLC shows");
        let m = memo(&partial, &copy).unwrap();
        assert_eq!((m.text.as_str(), m.head_op_id, m.conflict_of.as_deref()), ("e1", oid(3), Some(MEMO)));
        assert_eq!(partial.memo_lineage[&oid(3)], copy);
        assert_eq!(partial.memo_lineage[&oid(4)], MEMO);

        let full = read_all_orders(&[&c, &x, &e1, &e2]);
        assert_eq!(memo(&full, MEMO).unwrap().text, "e2");
        assert_eq!(memo(&full, &copy).unwrap().text, "e1");
        assert_eq!(full.notices, notice);
        assert_eq!(full.hash, partial.hash, "the base arriving moves nothing here");
    }

    #[test]
    fn orphan_losing_to_a_known_lineage_edit_is_a_copy_and_the_base_reroutes_it() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let x = edit(2, ("a", 1), 200, &[("b", 1)], 1, "x");
        let y = edit(3, ("d", 1), 400, &[("b", 1)], 1, "y");
        let e = edit(4, ("b", 2), 300, &[("a", 1), ("b", 1)], 2, "e");

        let partial = read_all_orders(&[&c, &y, &e]);
        let orphan_copy = conflict_copy_id(MEMO, &oid(4));
        assert_eq!(memo(&partial, MEMO).unwrap().text, "y");
        let m = memo(&partial, &orphan_copy).unwrap();
        assert_eq!((m.text.as_str(), m.conflict_of.as_deref()), ("e", Some(MEMO)));
        assert_eq!(
            partial.notices,
            vec![Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: orphan_copy, loser_op_id: oid(4) }]
        );

        // With x present, y and x compete on c; e follows its base into x's copy.
        let full = read_all_orders(&[&c, &x, &y, &e]);
        let x_copy = conflict_copy_id(MEMO, &oid(2));
        assert_eq!(memo_count(&full), 2);
        assert_eq!(memo(&full, MEMO).unwrap().text, "y");
        assert_eq!(memo(&full, &x_copy).unwrap().text, "e");
        assert_eq!(full.memo_lineage[&oid(4)], x_copy);
        assert_eq!(
            full.notices,
            vec![Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: x_copy, loser_op_id: oid(2) }]
        );
    }

    #[test]
    fn a_main_head_losing_to_an_orphan_is_a_copy_and_seen_heads_are_not() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let y = edit(3, ("d", 1), 200, &[("b", 1)], 1, "y");
        let e = edit(4, ("b", 2), 300, &[("a", 1), ("b", 1)], 2, "e");
        let p = read_all_orders(&[&c, &y, &e]);
        let y_copy = conflict_copy_id(MEMO, &oid(3));
        assert_eq!(memo(&p, MEMO).unwrap().text, "e");
        let m = memo(&p, &y_copy).unwrap();
        assert_eq!((m.text.as_str(), m.head_op_id, m.conflict_of.as_deref()), ("y", oid(3), Some(MEMO)));
        assert_eq!(p.notices, vec![Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: y_copy, loser_op_id: oid(3) }]);

        // An orphan that saw y supersedes it: no copy.
        let seen = edit(4, ("b", 2), 300, &[("a", 1), ("b", 1), ("d", 1)], 2, "e");
        let p = read_all_orders(&[&c, &y, &seen]);
        assert!(p.notices.is_empty(), "{:?}", p.notices);
        assert_eq!((memo_count(&p), memo(&p, MEMO).unwrap().text.as_str()), (1, "e"));
    }

    #[test]
    fn losing_orphans_with_one_text_share_one_copy_and_winner_text_makes_none() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let y = edit(2, ("d", 1), 900, &[("b", 1)], 1, "y");
        let o1 = edit(5, ("b", 2), 300, &[("a", 1), ("b", 1)], 9, "dup");
        let o2 = edit(4, ("c", 1), 400, &[("a", 1), ("b", 1)], 9, "dup");
        let same = edit(6, ("e", 1), 500, &[("a", 1), ("b", 1)], 9, "y");
        let p = read_all_orders(&[&c, &y, &o1, &o2, &same]);
        let copy = conflict_copy_id(MEMO, &oid(4));
        assert_eq!(memo_count(&p), 2, "one copy for the shared losing text, none for the winner's text");
        assert_eq!(memo(&p, MEMO).unwrap().text, "y");
        assert_eq!(memo(&p, &copy).unwrap().text, "dup");
        assert_eq!((p.memo_lineage[&oid(4)].as_str(), p.memo_lineage[&oid(5)].as_str()), (copy.as_str(), copy.as_str()));
        assert_eq!(p.memo_lineage[&oid(6)], MEMO);
        assert_eq!(p.notices, vec![Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: copy, loser_op_id: oid(4) }]);
    }

    /// Architect G3 cleanup b: an orphan chain whose head moves to a copy
    /// takes all its ops along. A blind delete of the memo that saw only the
    /// memo's own lineage deletes it: the chain's text lives in the copy.
    #[test]
    fn a_losing_orphan_chain_is_the_copys_content_in_fate_accounting() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let y = edit(2, ("d", 1), 900, &[("b", 1)], 1, "y");
        let o1 = edit(3, ("a", 1), 200, &[("b", 1)], 9, "o1");
        let o2 = edit(4, ("a", 2), 300, &[("a", 1), ("b", 1)], 3, "o2");
        let copy = conflict_copy_id(MEMO, &oid(4));
        let p = read_all_orders(&[&c, &y, &o1, &o2]);
        assert_eq!(memo(&p, MEMO).unwrap().text, "y");
        assert_eq!(memo(&p, &copy).unwrap().text, "o2");
        assert_eq!((p.memo_lineage[&oid(3)].as_str(), p.memo_lineage[&oid(4)].as_str()), (copy.as_str(), copy.as_str()));
        assert_eq!((p.memo_lineage[&oid(1)].as_str(), p.memo_lineage[&oid(2)].as_str()), (MEMO, MEMO));

        // b saw c and y, not the chain: the memo goes, the copy stays.
        let blind = delete(5, ("b", 2), 1_000, &[("b", 1), ("d", 1)], MEMO);
        let p = read_all_orders(&[&c, &y, &o1, &o2, &blind]);
        assert!(is_deleted(&p, Kind::Memo, MEMO), "{:?}", p.entities.get(&(Kind::Memo, MEMO.to_owned())));
        assert!(!restored(&p, Kind::Memo, MEMO));
        assert_eq!(memo(&p, &copy).unwrap().text, "o2");
        // Control: an unseen op of the memo's own lineage still restores it.
        let y2 = edit(6, ("d", 2), 950, &[("b", 1), ("d", 1)], 2, "y2");
        let p = read_all_orders(&[&c, &y, &y2, &o1, &o2, &blind]);
        assert!(restored(&p, Kind::Memo, MEMO));
        assert_eq!(memo(&p, MEMO).unwrap().text, "y2");
    }

    /// Architect G3 watch w1: a copy C seeded by a losing orphan chain
    /// {o1,o2} loses its carried head o2 to an orphan x homed at C. The
    /// respawned copy D takes the whole carried chain, and C keeps none of it:
    /// every lineage op has one owner, and a delete of C that saw x (all C
    /// shows) but not the chain deletes C while D stays live.
    #[test]
    fn a_respawned_carried_head_takes_its_chain_and_is_owned_once() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let e3 = edit(3, ("b", 2), 900, &[("b", 1)], 1, "main");
        let o1 = edit(4, ("a", 1), 150, &[], 99, "x");
        let o2 = edit(5, ("a", 2), 200, &[("a", 1)], 4, "y");
        let copy_c = conflict_copy_id(MEMO, &oid(5));
        let copy_d = conflict_copy_id(&copy_c, &oid(5));
        let x = op(6, ("c", 1), 1_000, &[("b", 2)], (Kind::Memo, copy_c.as_str(), "edit"), json!({"text": "z", "base_op_id": oid(98)}));
        let del = delete(7, ("c", 2), 1_100, &[("b", 2), ("c", 1)], &copy_c);
        let all = [&c, &e3, &o1, &o2, &x, &del];

        let (set, want) = read(&all);
        assert_single_owner(&set, "w1");
        let owners = lineage_owners(&set);
        assert_eq!(owners[&oid(5)], vec![copy_d.clone()], "o2 moved to D, not back into C");
        assert_eq!(owners[&oid(4)], vec![copy_d.clone()], "o1 moved with its head");
        let lineage: Vec<(String, String)> = want.memo_lineage.clone().into_iter().collect();
        let mut expected = vec![
            (oid(1), MEMO.to_owned()),
            (oid(3), MEMO.to_owned()),
            (oid(4), copy_d.clone()),
            (oid(5), copy_d.clone()),
            (oid(6), copy_c.clone()),
        ];
        expected.sort();
        assert_eq!(lineage, expected);

        assert_eq!(memo(&want, MEMO).unwrap().text, "main");
        assert!(is_deleted(&want, Kind::Memo, &copy_c), "{:?}", want.entities.get(&(Kind::Memo, copy_c.clone())));
        assert!(!restored(&want, Kind::Memo, &copy_c));
        let d = memo(&want, &copy_d).unwrap();
        assert_eq!((d.text.as_str(), d.head_op_id.clone(), d.conflict_of.as_deref()), ("y", oid(5), Some(copy_c.as_str())));
        assert!(invisible_memo_edits(&set, &want).is_empty());

        // Without the delete C shows x, D shows the chain head.
        let (_, live) = read(&all[..5]);
        assert_eq!(memo(&live, &copy_c).unwrap().text, "z");
        assert_eq!(memo(&live, &copy_d).unwrap().text, "y");

        let mut rng = Rng(0x00E1);
        let mut order: Vec<usize> = (0..all.len()).collect();
        for round in 0..200 {
            for i in (1..order.len()).rev() {
                order.swap(i, rng.next(i + 1));
            }
            let shuffled: Vec<&Pub> = order.iter().map(|&i| all[i]).collect();
            let (set, got) = read(&shuffled);
            assert_eq!(got.hash, want.hash, "round {round}");
            assert_eq!(got, want, "round {round}");
            assert_single_owner(&set, &format!("w1 round {round}"));
        }
    }

    /// Architect G3 cleanup b: an edit whose base names an existing non-lineage
    /// op (a pin or note id) has unknown lineage, like a missing base: it is
    /// never a root that competes with the create and hides the memo's edits.
    #[test]
    fn an_edit_based_on_a_pin_or_note_id_is_an_orphan_not_a_root() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let pin = op(2, ("b", 2), 150, &[("b", 1)], (Kind::Memo, MEMO, "pin"), json!({"pinned": true}));
        let note = op(3, ("b", 3), 160, &[("b", 2)], (Kind::Memo, MEMO, "note"), json!({"note": "n"}));
        let y = edit(4, ("d", 1), 300, &[("b", 1)], 1, "y");
        let bad = |base: u64| edit(5, ("a", 1), 900, &[("b", 3)], base, "bad");
        let (on_pin, on_note, on_missing) = (bad(2), bad(3), bad(99));
        let missing = read_all_orders(&[&c, &pin, &note, &y, &on_missing]);
        assert_eq!(memo(&missing, MEMO).unwrap().text, "bad", "the orphan saw no y but wins on HLC");
        let y_copy = conflict_copy_id(MEMO, &oid(4));
        assert_eq!(memo(&missing, &y_copy).unwrap().text, "y", "y stays visible as a copy");
        for (name, odd) in [("pin", &on_pin), ("note", &on_note)] {
            let p = read_all_orders(&[&c, &pin, &note, &y, odd]);
            assert_eq!(p.entities, missing.entities, "base on a {name} id");
            assert_eq!(p.notices, missing.notices, "base on a {name} id");
            assert_eq!(p.memo_lineage, missing.memo_lineage, "base on a {name} id");
            assert!(!p.memo_lineage.contains_key(&oid(2)) && !p.memo_lineage.contains_key(&oid(3)));
        }
    }

    /// G3 o3: an edit whose base is itself is an orphan of its own memo,
    /// like a missing base; two edits naming each other are both orphan
    /// heads and stay visible. An edit naming another memo's create joins
    /// that memo's lineage: lineage is structural
    /// (`_sync/plan-amendments.md`, "Memo lineage is structural"), so
    /// `entity_id` never overrides a create/edit base. Every arrival order
    /// gives one projection.
    ///
    /// Re-baselined when the unrecorded causal-past filter was removed (G3
    /// delta 3, architect b1): this asserted that an edit blind to OTHER's
    /// create equals a missing base. An edit that names `base_op_id` X has
    /// seen X by construction; causality across a fork range is decided only
    /// by the edit-vs-delete `saw`/obs rule, not by lineage. So the blind and
    /// the sighted cross-memo edit now project identically.
    #[test]
    fn self_and_cyclic_bases_are_orphans_and_cross_memo_bases_join_their_lineage() {
        const OTHER: &str = "0190a000-0000-7000-8000-00000000000b";
        let c = create(1, ("b", 1), 100, &[], "base");
        let y = edit(4, ("d", 1), 300, &[("b", 1)], 1, "y");
        let k = op(10, ("k", 1), 50, &[], (Kind::Memo, OTHER, "create"), json!({"text": "kay", "pinned": false}));
        let bad = |base: u64| edit(5, ("a", 1), 900, &[("b", 1)], base, "bad");
        let (on_self, on_other, on_missing) = (bad(5), bad(10), bad(99));
        let missing = read_all_orders(&[&c, &y, &k, &on_missing]);
        assert_eq!(memo(&missing, MEMO).unwrap().text, "bad");
        assert_eq!(memo(&missing, &conflict_copy_id(MEMO, &oid(4))).unwrap().text, "y");
        let p = read_all_orders(&[&c, &y, &k, &on_self]);
        assert_eq!(p.entities, missing.entities, "base on itself");
        assert_eq!(p.notices, missing.notices, "base on itself");
        assert_eq!(p.memo_lineage, missing.memo_lineage, "base on itself");
        assert_eq!(memo(&p, OTHER).unwrap().text, "kay", "base on itself");
        assert_eq!(p.memo_lineage[&oid(5)], MEMO, "base on itself");

        // Structural (plan-amendments): an edit naming OTHER's create
        // continues OTHER's lineage, whether or not its ctx covers that
        // create; MEMO keeps its own lineage, no copy.
        let sighted = edit(5, ("a", 1), 900, &[("b", 1), ("k", 1)], 10, "bad");
        let p = read_all_orders(&[&c, &y, &k, &sighted]);
        assert_eq!(memo(&p, OTHER).unwrap().text, "bad", "named another memo's create");
        assert_eq!(memo(&p, MEMO).unwrap().text, "y", "named another memo's create");
        assert_eq!(memo_count(&p), 2, "no conflict copy");
        assert_eq!(p.memo_lineage[&oid(5)], OTHER);
        assert_eq!((p.memo_lineage[&oid(1)].as_str(), p.memo_lineage[&oid(4)].as_str()), (MEMO, MEMO));
        let blind = read_all_orders(&[&c, &y, &k, &on_other]);
        assert_eq!(blind, p, "a ctx blind to the named base routes the same");

        let cyc5 = edit(5, ("a", 1), 900, &[("b", 1)], 6, "cyc5");
        let cyc6 = edit(6, ("e", 1), 910, &[("b", 1)], 5, "cyc6");
        let p = read_all_orders(&[&c, &y, &k, &cyc5, &cyc6]);
        assert_eq!(memo(&p, MEMO).unwrap().text, "cyc6", "the newest unseen head wins");
        assert_eq!(memo(&p, &conflict_copy_id(MEMO, &oid(5))).unwrap().text, "cyc5");
        assert_eq!(memo(&p, &conflict_copy_id(MEMO, &oid(4))).unwrap().text, "y");
        assert_eq!(memo(&p, OTHER).unwrap().text, "kay");
        assert_eq!(memo_count(&p), 4, "no edit of the cycle is invisible");
    }

    /// A base in a fork range: an honest child whose ctx names the forked
    /// dot but not the base's op id (no obs) is an ordinary child of its
    /// base, exactly like the same child with the base in `obs`. Every
    /// reader converges whatever its order.
    ///
    /// Re-baselined when the unrecorded causal-past filter was removed (G3
    /// delta 3, architect b1): the obs-less half asserted an orphan split
    /// (child cut from v1, copy + notice), and the test
    /// `a_child_blind_to_its_base_is_an_orphan` asserted the same inside one
    /// memo; that test is removed. An edit that names `base_op_id` X has seen
    /// X by construction; causality across a fork range is decided only by
    /// the edit-vs-delete `saw`/obs rule (`fate`, kept in
    /// `ac19_ac19b_fork_variants_converge_and_blind_deletes_do_not_count`),
    /// not by lineage.
    #[test]
    fn a_base_in_a_fork_range_is_lineage_with_or_without_obs() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let v1 = edit(2, ("a", 1), 200, &[("b", 1)], 1, "variant one");
        let v2 = edit(3, ("a", 1), 210, &[("b", 1)], 1, "variant two");
        let child = edit(4, ("b", 2), 300, &[("a", 1), ("b", 1)], 2, "child");
        let mut seen = child.clone();
        seen.0.obs = vec![oid(2)];
        let v1_copy = conflict_copy_id(MEMO, &oid(2));
        for (name, child) in [("no obs", &child), ("obs", &seen)] {
            let readers = [
                read(&[&c, &v1, child, &v2]),
                read(&[&v2, child, &c, &v1]),
                read(&[child, &v2, &v1, &c]),
            ];
            for (set, p) in &readers {
                assert_eq!(set.forks(), &Forks::from([("a".to_owned(), 1)]), "{name}: fork detected");
                assert_eq!(p, &readers[0].1, "{name}: all readers converge");
                assert!(invisible_memo_edits(set, p).is_empty(), "{name}: {:?}", invisible_memo_edits(set, p));
            }
            let p = &readers[0].1;
            // v1 and v2 race on the create (v2 newer), and the child follows
            // v1 into its copy as an ordinary child: no orphan split.
            assert_eq!(memo(p, MEMO).unwrap().text, "variant two", "{name}");
            assert_eq!(memo(p, &v1_copy).unwrap().text, "child", "{name}: an ordinary child of v1");
            assert_eq!((p.memo_lineage[&oid(2)].as_str(), p.memo_lineage[&oid(4)].as_str()), (v1_copy.as_str(), v1_copy.as_str()));
            assert_eq!(memo_count(p), 2, "{name}");
        }
    }

    /// G3 delta d1: edits addressed to a conflict copy name the source memo's
    /// lineage op as base (`head_op_id`) and still compete on it as same-base
    /// children (§4.4), even when one of them carries a chain: the first copy
    /// edits compete at op 2, not the chain tip.
    #[test]
    fn copy_addressed_edits_with_a_chain_compete_on_their_shared_base() {
        let copy = conflict_copy_id(MEMO, &oid(2));
        let on_copy = |n, at, wall, ctx: &[(&str, u64)], base: u64, text: &str| {
            op(n, at, wall, ctx, (Kind::Memo, copy.as_str(), "edit"), json!({"text": text, "base_op_id": oid(base)}))
        };
        let ops = [
            create(1, ("a", 1), 1_000, &[], "base"),
            edit(2, ("a", 2), 1_001, &[("a", 1)], 1, "a1"),
            edit(3, ("b", 1), 1_002, &[("a", 1)], 1, "b1"),
            on_copy(5, ("a", 3), 1_004, &[("a", 2), ("b", 1)], 2, "x1"),
            on_copy(6, ("a", 4), 1_010, &[("a", 3), ("b", 1)], 5, "x2"),
            on_copy(7, ("b", 2), 1_006, &[("a", 2), ("b", 1)], 2, "y1"),
        ];
        let refs: Vec<&Pub> = ops.iter().collect();
        let p = read(&refs).1;
        assert_eq!(memo(&p, MEMO).unwrap().text, "b1");
        let m = memo(&p, &copy).unwrap();
        assert_eq!((m.text.as_str(), m.head_op_id, m.conflict_of.as_deref()), ("y1", oid(7), Some(MEMO)));
        let nested = conflict_copy_id(&copy, &oid(5));
        let n = memo(&p, &nested).unwrap();
        assert_eq!((n.text.as_str(), n.head_op_id, n.conflict_of.as_deref()), ("x2", oid(6), Some(copy.as_str())));
        assert_eq!(memo_count(&p), 3, "memo, copy, nested copy");
        assert!(memo(&p, &conflict_copy_id(&copy, &oid(7))).is_none(), "y1 is not a loser");
        assert_eq!(
            p.notices,
            vec![
                Notice::MemoConflictCopy { memo_id: MEMO.to_owned(), copy_id: copy.clone(), loser_op_id: oid(2) },
                Notice::MemoConflictCopy { memo_id: copy.clone(), copy_id: nested.clone(), loser_op_id: oid(5) },
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        for (n, owner) in [(1, MEMO), (3, MEMO), (2, copy.as_str()), (7, copy.as_str()), (5, nested.as_str()), (6, nested.as_str())] {
            assert_eq!(p.memo_lineage[&oid(n)], owner, "op {n}");
        }

        // 200 seeded arrival orders give one projection and one hash.
        let mut rng: u64 = 0x5D1_C0DE;
        for round in 0..200 {
            let mut order: Vec<usize> = (0..refs.len()).collect();
            for i in (1..order.len()).rev() {
                rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                order.swap(i, (rng >> 33) as usize % (i + 1));
            }
            let shuffled: Vec<&Pub> = order.iter().map(|&i| refs[i]).collect();
            let q = read(&shuffled).1;
            assert_eq!(q.hash, p.hash, "round {round} order {order:?}");
            assert_eq!(q, p, "round {round} order {order:?}");
        }
    }

    #[test]
    fn identical_losing_texts_share_one_copy_named_after_the_smallest_loser() {
        let c = create(1, ("b", 1), 100, &[], "base");
        let d1 = edit(2, ("a", 1), 200, &[("b", 1)], 1, "dup");
        let d2 = edit(3, ("c", 1), 300, &[("b", 1)], 1, "dup");
        let other = edit(4, ("d", 1), 350, &[("b", 1)], 1, "other");
        let win = edit(5, ("e", 1), 400, &[("b", 1)], 1, "winner");
        let p = read_all_orders(&[&c, &d1, &d2, &other, &win]);
        assert_eq!(memo(&p, MEMO).unwrap().text, "winner");
        let dup_copy = conflict_copy_id(MEMO, &oid(2));
        let other_copy = conflict_copy_id(MEMO, &oid(4));
        assert_eq!(memo_count(&p), 3, "one copy per distinct losing text");
        assert_eq!(memo(&p, &dup_copy).unwrap().text, "dup");
        assert_eq!(memo(&p, &other_copy).unwrap().text, "other");
        assert_eq!(p.memo_lineage[&oid(2)], dup_copy);
        assert_eq!(p.memo_lineage[&oid(3)], dup_copy);
        let mut losers: Vec<&str> = p
            .notices
            .iter()
            .map(|n| match n {
                Notice::MemoConflictCopy { loser_op_id, .. } => loser_op_id.as_str(),
                Notice::RestoredAfterDeleteConflict { .. } => panic!("unexpected {n:?}"),
            })
            .collect();
        losers.sort_unstable();
        assert_eq!(losers, [oid(2).as_str(), oid(4).as_str()]);
    }

    #[test]
    fn forged_causal_cycles_fall_back_to_the_tie_key_in_any_order() {
        // x@(a,1) claims to have seen (c,1) and y@(c,1) claims to have seen (a,1).
        let c = create(1, ("b", 1), 100, &[], "base");
        let pin = |n, at, wall, ctx: &[(&str, u64)], pinned: bool| {
            op(n, at, wall, ctx, (Kind::Memo, MEMO, "pin"), json!({"pinned": pinned}))
        };
        let x = pin(2, ("a", 1), 300, &[("b", 1), ("c", 1)], true);
        let y = pin(3, ("c", 1), 200, &[("a", 1), ("b", 1)], false);
        let (set, _) = read(&[&x, &y]);
        let (ex, ey) = (set.get(&oid(2)).unwrap(), set.get(&oid(3)).unwrap());
        assert!(saw(ex, ey, &Forks::new()) && saw(ey, ex, &Forks::new()), "a real cycle");
        let p = read_all_orders(&[&c, &x, &y]);
        assert!(memo(&p, MEMO).unwrap().pinned, "no maximal pin: max tie key (x, wall 300) wins");

        let e1 = edit(4, ("a", 1), 200, &[("b", 1), ("c", 1)], 1, "one");
        let e2 = edit(5, ("c", 1), 300, &[("a", 1), ("b", 1)], 1, "two");
        let p = read_all_orders(&[&c, &e1, &e2]);
        let copy = conflict_copy_id(MEMO, &oid(4));
        assert_eq!(memo(&p, MEMO).unwrap().text, "two", "max tie key continues the memo");
        assert_eq!(memo(&p, &copy).unwrap().text, "one");
        assert_eq!(memo_count(&p), 2);
    }

    #[test]
    fn ac24_far_future_op_is_kept_verbatim_and_hashes_converge() {
        let hour = 60 * MIN;
        let now = 1_700_000_000_000;
        let c = create(1, ("b", 1), now, &[], "base");
        let future = edit(2, ("a", 1), now + 48 * hour, &[("b", 1)], 1, "from the future");
        let local = edit(3, ("c", 1), now + 1, &[("b", 1)], 1, "local");
        let readers = [read(&[&c, &future, &local]), read(&[&local, &c, &future]), read(&[&future, &local, &c])];
        for (set, p) in &readers {
            assert_eq!(set.get(&oid(2)).unwrap().body.hlc, Hlc { wall_ms: now + 48 * hour, ctr: 0 });
            assert_eq!(p.hash, readers[0].1.hash);
        }
    }

    #[test]
    fn projection_hash_is_canonical_json_of_the_entities() {
        let add = op(1, ("a", 1), 10, &[], (Kind::Tr, "tr-1", "tr.add"), json!({"src": "가"}));
        let (_, p) = read(&[&add]);
        // python3 -c 'import json,hashlib;v={"tr/tr-1":{"state":"tr","data":{"src":"가"},"pinned":False}};print(hashlib.sha256(json.dumps(v,sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()).hexdigest())'
        assert_eq!(hex(&p.hash), "d2eb2766716d61f64e3132cf987c1a09c3a782e36af544f49f0dd6e7cc70661b");
    }

    // ---- Memo routing: order-free, no invisible edit ----------------------

    struct Rng(u64);

    impl Rng {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    /// One memo history: an honest three-level copy-of-copy seed (memo, its
    /// copy, the copy's nested copy, all with chains) plus random honest
    /// edits (addressed to any memo or copy), orphans (missing base), self
    /// bases, base cycles, cross-memo bases, deletes and pins. Op ids are
    /// shuffled against creation order.
    fn memo_history(seed: u64) -> Vec<Pub> {
        const OTHER: &str = "0190a000-0000-7000-8000-00000000000b";
        let mut rng = Rng(seed);
        let mut ids: Vec<u64> = (100..160).collect();
        for i in (1..ids.len()).rev() {
            ids.swap(i, rng.next(i + 1));
        }
        let mut ids = ids.into_iter();
        let mut fresh = move || ids.next().expect("id pool");
        let copy = conflict_copy_id(MEMO, &oid(2));
        let nested = conflict_copy_id(&copy, &oid(5));
        let on = |n: u64, at: (&str, u64), wall: u64, ctx: &[(&str, u64)], entity: &str, base: u64, text: &str| {
            op(n, at, wall, ctx, (Kind::Memo, entity, "edit"), json!({"text": text, "base_op_id": oid(base)}))
        };
        let mut out = vec![
            create(1, ("a", 1), 1_000, &[], "base"),
            edit(2, ("a", 2), 1_001, &[("a", 1)], 1, "a1"),
            edit(3, ("b", 1), 1_002, &[("a", 1)], 1, "b1"),
            on(5, ("a", 3), 1_004, &[("a", 2), ("b", 1)], &copy, 2, "x1"),
            on(6, ("a", 4), 1_010, &[("a", 3), ("b", 1)], &copy, 5, "x2"),
            on(7, ("b", 2), 1_006, &[("a", 2), ("b", 1)], &copy, 2, "y1"),
            on(8, ("c", 1), 1_020, &[("a", 4), ("b", 2)], &nested, 6, "z1"),
            on(9, ("c", 2), 1_021, &[("a", 4), ("b", 2), ("c", 1)], &nested, 6, "w1"),
            op(10, ("k", 1), 900, &[], (Kind::Memo, OTHER, "create"), json!({"text": "kay", "pinned": false})),
        ];
        let memos = [MEMO, OTHER, copy.as_str(), nested.as_str()];
        let devices = ["a", "b", "c", "k", "d", "e"];
        let mut seqs: BTreeMap<&str, u64> = BTreeMap::from([("a", 4), ("b", 2), ("c", 2), ("k", 1)]);
        let mut nodes: Vec<u64> = vec![1, 2, 3, 5, 6, 7, 8, 9, 10];
        let texts = ["t0", "t1", "t2", "x1", "y1", "b1"];
        for _ in 0..14 {
            let dev = devices[rng.next(devices.len())];
            let mut ctx: Vec<(&str, u64)> = Vec::new();
            for (&d, &s) in &seqs {
                if d == dev {
                    ctx.push((d, s));
                } else if rng.next(3) > 0 {
                    ctx.push((d, 1 + rng.next(s as usize) as u64));
                }
            }
            fn bump(seqs: &mut BTreeMap<&'static str, u64>, d: &'static str) -> (&'static str, u64) {
                let s = seqs.entry(d).or_insert(0);
                *s += 1;
                (d, *s)
            }
            let next_at = bump;
            let wall = 1_000 + rng.next(60) as u64;
            let entity = memos[rng.next(memos.len())];
            let text = texts[rng.next(texts.len())];
            let n = fresh();
            match rng.next(10) {
                0..=3 => {
                    let base = nodes[rng.next(nodes.len())];
                    out.push(on(n, next_at(&mut seqs, dev), wall, &ctx, entity, base, text));
                    nodes.push(n);
                }
                4 => out.push(on(n, next_at(&mut seqs, dev), wall, &ctx, entity, 9_000 + n, text)),
                5 => out.push(on(n, next_at(&mut seqs, dev), wall, &ctx, entity, n, text)),
                6 => {
                    let m = fresh();
                    out.push(on(n, next_at(&mut seqs, dev), wall, &ctx, entity, m, text));
                    let other = memos[rng.next(memos.len())];
                    out.push(on(m, next_at(&mut seqs, "e"), wall + 1, &ctx, other, n, "cyc"));
                    nodes.extend([n, m]);
                }
                7 => {
                    let base = nodes[rng.next(nodes.len())];
                    out.push(on(n, next_at(&mut seqs, dev), wall, &ctx, OTHER, base, text));
                    nodes.push(n);
                }
                8 => out.push(delete(n, next_at(&mut seqs, dev), 2_000 + wall, &ctx, entity)),
                _ => out.push(op(n, next_at(&mut seqs, dev), wall, &ctx, (Kind::Memo, entity, "pin"), json!({"pinned": true}))),
            }
        }
        out
    }

    /// Memo routing is one pure function of the op set: across 500 seeded
    /// arrival orders of each mixed history the projection (hash, notices,
    /// routing) is identical, and every memo create/edit is routed to one
    /// entity and is shown or superseded within its own lineage.
    #[test]
    fn memo_routing_is_order_free_and_hides_no_edit_over_500_permutations() {
        for history in 0..12u64 {
            let ops = memo_history(0x6E0_0000 + history);
            let refs: Vec<&Pub> = ops.iter().collect();
            let (set, want) = read(&refs);
            let invisible = invisible_memo_edits(&set, &want);
            assert!(invisible.is_empty(), "history {history}: invisible {invisible:?}");
            assert_single_owner(&set, &format!("history {history}"));
            if history == 0 {
                // The oracle is not vacuous: an unrouted node is reported.
                let mut cut = want.clone();
                cut.memo_lineage.remove(&oid(7));
                assert_eq!(invisible_memo_edits(&set, &cut), vec![oid(7)]);
            }
            let mut rng = Rng(history);
            let mut order: Vec<usize> = (0..refs.len()).collect();
            for round in 0..500 {
                for i in (1..order.len()).rev() {
                    order.swap(i, rng.next(i + 1));
                }
                let shuffled: Vec<&Pub> = order.iter().map(|&i| refs[i]).collect();
                let (got_set, got) = read(&shuffled);
                assert_eq!(got.hash, want.hash, "history {history} round {round}");
                assert_eq!(got, want, "history {history} round {round}");
                assert_single_owner(&got_set, &format!("history {history} round {round}"));
            }
        }
    }

    /// The generator really mixes every form: orphans, self bases, cycles,
    /// cross-memo bases and edits addressed to copies of copies.
    #[test]
    fn memo_history_covers_every_base_form() {
        let mut forms = BTreeSet::new();
        for history in 0..12u64 {
            let ops = memo_history(0x6E0_0000 + history);
            let refs: Vec<&Pub> = ops.iter().collect();
            let (set, p) = read(&refs);
            let ids: BTreeSet<&str> = set.iter().map(|e| e.body.op_id.as_str()).collect();
            for e in set.iter().filter(|e| e.body.op_type == "edit") {
                let base = e.body.payload["base_op_id"].as_str().unwrap();
                let named = set.iter().find(|b| b.body.op_id == base);
                forms.insert(match named {
                    _ if base == e.body.op_id => "self",
                    None if !ids.contains(base) => "orphan",
                    Some(b) if b.body.payload.get("base_op_id").and_then(Value::as_str) == Some(e.body.op_id.as_str()) => "cycle",
                    Some(b) if b.body.entity_id != e.body.entity_id => "cross",
                    _ => "plain",
                });
            }
            let copy = conflict_copy_id(MEMO, &oid(2));
            if p.notices.iter().any(|n| matches!(n, Notice::MemoConflictCopy { memo_id, .. } if *memo_id == copy)) {
                forms.insert("copy of copy");
            }
        }
        assert_eq!(forms, BTreeSet::from(["copy of copy", "cross", "cycle", "orphan", "plain", "self"]));
    }

    // ---- AC 1: permutation invariance -------------------------------------

    /// One generated op: which device/entity, what it does, and how it is
    /// republished (0 plain, 1 alias, 2 restamp, 3 reissue, 4 fork, 5 obs).
    type Spec = (u8, u8, u8, u8, u8, u8, u8, u8);

    const DEVICES: [&str; 3] = ["dev-a", "dev-b", "dev-c"];

    fn build(specs: &[Spec]) -> Vec<Pub> {
        let mut out: Vec<Pub> = Vec::new();
        let mut seqs = [0u64; 4];
        let mut next_id = 0u64;
        let mut fresh = || {
            next_id += 1;
            next_id
        };
        let mut memo_nodes: Vec<String> = Vec::new();
        for &(dev, ent, what, wall, text, base, ctx_bits, special) in specs {
            let d = (dev % 3) as usize;
            seqs[d] += 1;
            let at = (DEVICES[d], seqs[d]);
            let ctx: Vec<(&str, u64)> = (0..3)
                .filter(|i| ctx_bits & (1 << i) != 0)
                .map(|i| (DEVICES[i], (seqs[i] * u64::from(ctx_bits >> 4) / 15).max(u64::from(i as u64 != d as u64))))
                .collect();
            let wall = u64::from(wall % 4) * 10;
            let entity = format!("e{}", ent % 3);
            let text = format!("t{}", text % 3);
            let (kind, op_type, payload) = match what % 12 {
                0 => (Kind::Memo, "create", json!({"text": text, "pinned": base % 2 == 0})),
                1 | 2 => {
                    let base_id = if memo_nodes.is_empty() || base % 5 == 0 {
                        oid(9_999_000 + u64::from(base))
                    } else {
                        memo_nodes[base as usize % memo_nodes.len()].clone()
                    };
                    (Kind::Memo, "edit", json!({"text": text, "base_op_id": base_id}))
                }
                3 => (Kind::Memo, "pin", json!({"pinned": base % 2 == 0})),
                4 => (Kind::Memo, "note", json!({"note": text})),
                5 => (Kind::Memo, "delete", json!({})),
                6 => (Kind::Conv, "create", json!({"title": text})),
                7 => {
                    let mut p = json!({"conv_id": entity, "msg_id": format!("m{}", base % 4), "role": "user",
                                       "content": text, "attachments": [], "ts": wall});
                    if base % 3 != 0 {
                        p["parent_id"] = json!(format!("m{}", base % 2));
                    }
                    (Kind::Conv, "msg.add", p)
                }
                8 => (Kind::Conv, "set_active", json!({"leaf": format!("m{}", base % 4)})),
                9 => (Kind::Conv, "delete", json!({})),
                10 => (Kind::Tr, if base % 2 == 0 { "tr.add" } else { "tr.pin" }, json!({"pinned": base % 3 == 0})),
                _ => (Kind::Tr, "tr.delete", json!({})),
            };
            let n = fresh();
            let mut p = op(n, at, wall, &ctx, (kind, &entity, op_type), payload);
            p.0.hlc.ctr = u32::from(base % 2);
            if kind == Kind::Memo && matches!(op_type, "create" | "edit") {
                memo_nodes.push(p.0.op_id.clone());
            }
            match special % 6 {
                1 => out.push(Pub(p.0.clone(), dot(DEVICES[(d + 1) % 3], 900 + n))),
                2 => {
                    seqs[3] += 1;
                    let mut r = p.0.clone();
                    r.origin_device = "dev-r".to_owned();
                    r.ctx.retain(|k, _| k != at.0);
                    out.push(Pub(r, dot("dev-r", seqs[3])));
                }
                3 => {
                    seqs[d] += 1;
                    let mut r = p.0.clone();
                    r.op_id = oid(fresh());
                    out.push(Pub(r, dot(at.0, seqs[d])));
                }
                4 => {
                    let mut f = p.0.clone();
                    f.op_id = oid(fresh());
                    if let Some(t) = f.payload.get_mut("text") {
                        *t = json!(format!("{text}-fork"));
                    }
                    out.push(Pub(f, p.1.clone()));
                }
                5 => {
                    if let Some(prev) = out.last() {
                        p.0.obs = vec![prev.0.op_id.clone()];
                    }
                }
                _ => {}
            }
            out.push(p);
        }
        out
    }

    fn arb_spec() -> impl Strategy<Value = Spec> {
        (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>())
    }

    #[test]
    fn ac1_projection_hash_is_independent_of_insertion_order() {
        let mut config = Config::with_cases(1_000);
        config.rng_seed = RngSeed::Fixed(0x5A7_C0DE);
        config.failure_persistence = None;
        let mut runner = TestRunner::new(config);
        let strategy = proptest::collection::vec(arb_spec(), 1..24).prop_flat_map(|specs| {
            let n = build(&specs).len();
            let order = || Just((0..n).collect::<Vec<usize>>()).prop_shuffle();
            (Just(specs), order(), order())
        });
        runner
            .run(&strategy, |(specs, o1, o2)| {
                let pubs = build(&specs);
                let insert = |order: &[usize]| {
                    let mut set = OpSet::new();
                    for &i in order {
                        let Pub(body, d) = &pubs[i];
                        prop_assert_eq!(body.validate(), Ok(()));
                        let got = set.insert(body.clone(), d.clone()).unwrap();
                        prop_assert_ne!(got.outcome, Insert::Conflict);
                    }
                    Ok(set)
                };
                let base = insert(&(0..pubs.len()).collect::<Vec<_>>())?;
                let want = project(&base, &Forks::new()).unwrap();
                for order in [&o1, &o2] {
                    let set = insert(order)?;
                    prop_assert_eq!(&set, &base);
                    let got = project(&set, &Forks::new()).unwrap();
                    prop_assert_eq!(got.hash, want.hash);
                    prop_assert_eq!(&got, &want);
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn ac1_generator_covers_restamp_reissue_alias_and_fork() {
        let specs: Vec<Spec> = (0..6).map(|k| (k, 0, 1, 0, k, 1, 0xF7, k)).collect();
        let pubs = build(&specs);
        let mut set = OpSet::new();
        for Pub(body, d) in &pubs {
            set.insert(body.clone(), d.clone()).unwrap();
        }
        assert!(!set.forks().is_empty(), "fork");
        assert!(set.iter().any(|e| e.dots.iter().any(|d| d.device == "dev-r")), "restamp");
        assert!(set.iter().any(|e| e.dots.len() == 2 && e.dots.iter().all(|d| d.device != "dev-r")), "alias");
        assert!(set.iter().any(|e| !e.body.obs.is_empty()), "obs");
        assert!(set.len() > specs.len(), "reissue and fork add op ids");
    }
}
