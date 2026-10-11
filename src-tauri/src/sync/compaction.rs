//! Compaction, local checkpoints, and retention (§4.5, §4.4 tombstones).
//!
//! A snapshot carries the whole vault op set (op id, every dot, the
//! alias-unioned ctx and obs) plus the recorded forks, so restoring it
//! projects to the same hash as the full log (AC 8). Remote segments are only
//! ever offered for deletion after the snapshot verified.
//!
//! Contract deviations (G3 design, compaction.rs):
//! - Functions take `&Store` / `&mut Store` explicitly and return `Result`.
//! - `restore_equivalent` returns the restored `Projection`; `verify_snapshot`
//!   is the AC 8 gate: canonical round trip, then equality with the live
//!   store's own `build_snapshot` (every op body, dot, ctx, obs, the forks and
//!   the frontier), then the restored projection hash.
//! - `segments_deletable(store, vault, own_device, active_cursors, verified,
//!   snapshot)` also takes the snapshot, so nothing past its own frontier is
//!   offered; it lists own sealed segments (`pending_segments`) by range
//!   through `Store::offer_deletable`, the single deletable predicate (fully
//!   `published`, not forked, not retired). The engine deletes them remotely,
//!   then calls `Store::forget_segments`, which accepts exactly the last
//!   offer re-checked against the current state; `compaction_due` counts only
//!   the own segments still listed there, so the trigger clears after a
//!   compaction.
//! - `checkpoint(store, dir)` takes the checkpoint directory; files are named
//!   `sync-checkpoint-<id>.db`.
//! - Retention: snapshots never drop ops, so tombstones survive; dropping
//!   payloads/blobs is only allowed where `payload_droppable` says the
//!   entity is deleted. No payload is dropped yet (a dropped payload would
//!   change `content_hash` and turn a late alias into a conflict).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::causal::{Forks, OpSet};
use super::merge::{project, EntityKey, EntityState, Projection};
use super::ops::{canonical_json, Dot, OpBody, OpError, FORMAT_V};
use super::store::{now_ms, Store, StoreError};

/// Own segments beyond this count trigger compaction.
pub(crate) const MAX_OWN_SEGMENTS: usize = 200;
/// The oldest own segment beyond this age triggers compaction.
pub(crate) const MAX_SEGMENT_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
pub(crate) const KEEP_CHECKPOINTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapOp {
    pub(crate) body: OpBody,
    pub(crate) dots: Vec<Dot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub(crate) v: u64,
    pub(crate) vault_id: String,
    pub(crate) device_id: String,
    /// Highest seq covered per device (own stream included).
    pub(crate) frontier: BTreeMap<String, u64>,
    pub(crate) forks: Forks,
    /// In op id order.
    pub(crate) ops: Vec<SnapOp>,
}

impl Snapshot {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, OpError> {
        canonical_json(&serde_json::to_value(self).map_err(|e| OpError::Schema(e.to_string()))?)
    }

    /// A newer `v` (snapshot or op body) is reported before the canonical
    /// and strict field checks: a newer format may use encodings v1 rejects.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Snapshot, OpError> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| OpError::Schema(e.to_string()))?;
        let future = |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_u64).filter(|v| *v > FORMAT_V);
        if let Some(v) = future(value.get("v")) {
            return Err(OpError::FutureVersion(v));
        }
        if let Some(ops) = value.get("ops").and_then(serde_json::Value::as_array) {
            if let Some(v) = ops.iter().find_map(|op| future(op.get("body").and_then(|b| b.get("v")))) {
                return Err(OpError::FutureVersion(v));
            }
        }
        if canonical_json(&value)? != bytes {
            return Err(OpError::NotCanonical);
        }
        let snapshot: Snapshot = serde_json::from_value(value).map_err(|e| OpError::Schema(e.to_string()))?;
        snapshot.ops.iter().try_for_each(|op| op.body.validate())?;
        Ok(snapshot)
    }
}

pub(crate) fn build_snapshot(store: &Store, vault: &str) -> Result<Snapshot, StoreError> {
    let (set, forks) = store.load_set(vault)?;
    let ops = set.iter().map(|e| SnapOp { body: e.body.clone(), dots: e.dot_list() }).collect();
    Ok(Snapshot {
        v: FORMAT_V,
        vault_id: vault.to_owned(),
        device_id: store.device_id(vault)?,
        frontier: store.frontier(vault)?,
        forks,
        ops,
    })
}

/// Rebuilds the op set from a snapshot and projects it.
pub(crate) fn restore_equivalent(snapshot: &Snapshot) -> Result<Projection, OpError> {
    let mut set = OpSet::new();
    for op in &snapshot.ops {
        for dot in &op.dots {
            set.insert(op.body.clone(), dot.clone())?;
        }
    }
    project(&set, &snapshot.forks)
}

/// AC 8 gate: the snapshot survives its canonical encoding, is exactly what
/// the live store holds (op set with every dot, ctx, obs, plus forks and
/// frontier; a projection hash alone misses ops of tombstoned entities,
/// alias dots, and blind deletes), and projects to the live store's hash.
pub(crate) fn verify_snapshot(store: &Store, vault: &str, snapshot: &Snapshot) -> Result<bool, StoreError> {
    let decoded = match Snapshot::decode(&snapshot.encode()?) {
        Ok(decoded) => decoded,
        Err(_) => return Ok(false),
    };
    if decoded != *snapshot || decoded.vault_id != vault {
        return Ok(false);
    }
    if decoded != build_snapshot(store, vault)? {
        return Ok(false);
    }
    let restored = match restore_equivalent(&decoded) {
        Ok(p) => p,
        Err(_) => return Ok(false),
    };
    Ok(restored.hash == store.projection(vault)?.hash)
}

/// Own sealed segments `(first, last)` that may be deleted remotely: only
/// after a verified snapshot of this vault, only below every active reader's
/// cursor and the snapshot frontier, only fully `published`, and never for a
/// retired or forked stream (`Store::offer_deletable`, the one predicate
/// `Store::forget_segments` also enforces). An unverified call revokes the
/// previous offer.
pub(crate) fn segments_deletable(
    store: &Store,
    vault: &str,
    own_device: &str,
    active_cursors: &[u64],
    verified: bool,
    snapshot: &Snapshot,
) -> Result<Vec<(u64, u64)>, StoreError> {
    let limit = if verified && snapshot.vault_id == vault {
        let covered = snapshot.frontier.get(own_device).copied().unwrap_or(0);
        covered.min(active_cursors.iter().copied().min().unwrap_or(covered))
    } else {
        0
    };
    store.offer_deletable(vault, own_device, limit)
}

/// Trigger: more than 200 own segments still live remotely (sealed and not
/// yet forgotten after a confirmed delete), or the oldest of them is 30 days
/// old.
pub(crate) fn compaction_due(store: &Store, vault: &str, now: u64) -> Result<bool, StoreError> {
    let own = store.device_id(vault)?;
    let segments = store.pending_segments(vault, &own)?;
    let oldest = segments.iter().map(|(_, _, created)| *created).min();
    Ok(segments.len() > MAX_OWN_SEGMENTS || oldest.is_some_and(|t| now.saturating_sub(t) >= MAX_SEGMENT_AGE_MS))
}

/// `VACUUM INTO` a new checkpoint under `dir`, record it, and keep only the
/// newest three (§4.5 local rollback).
pub(crate) fn checkpoint(store: &mut Store, dir: &Path) -> Result<PathBuf, StoreError> {
    std::fs::create_dir_all(dir)?;
    let frontier = match store.active_vault()? {
        // A vault that is active but not joined yet has no frontier; any
        // other failure must not record a wrong rollback reference.
        Some(vault) => match store.frontier(&vault) {
            Ok(frontier) => frontier,
            Err(StoreError::NoIdentity) => BTreeMap::new(),
            Err(e) => return Err(e),
        },
        None => BTreeMap::new(),
    };
    let frontier_json = serde_json::to_string(&frontier).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    let conn = store.conn();
    conn.execute(
        "INSERT INTO checkpoints(created_at, path, frontier_json) VALUES(?1, '', ?2)",
        params![now_ms(), frontier_json],
    )?;
    let id = conn.last_insert_rowid();
    let path = dir.join(format!("sync-checkpoint-{id}.db"));
    let path_text = path.to_string_lossy().into_owned();
    if let Err(e) = conn.execute("VACUUM INTO ?1", params![path_text]) {
        conn.execute("DELETE FROM checkpoints WHERE id = ?1", params![id])?;
        return Err(e.into());
    }
    conn.execute("UPDATE checkpoints SET path = ?2 WHERE id = ?1", params![id, path_text])?;

    let stale: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, path FROM checkpoints ORDER BY id DESC LIMIT -1 OFFSET ?1")?;
        let rows = stmt.query_map(params![KEEP_CHECKPOINTS as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (old_id, old_path) in stale {
        match std::fs::remove_file(&old_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        conn.execute("DELETE FROM checkpoints WHERE id = ?1", params![old_id])?;
    }
    Ok(path)
}

/// Retention: payloads and blobs may be dropped only for deleted entities;
/// the tombstone itself is never compacted away.
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) fn payload_droppable(projection: &Projection, key: &EntityKey) -> bool {
    matches!(projection.entities.get(key), Some(EntityState::Deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::sim_tests::{fork_world, sync_stream, all, TempDir, VAULT};

    /// AC 8: the snapshot keeps obs and forks and projects to the full log's
    /// hash; a failed verify offers 0 segments for deletion.
    #[test]
    fn ac8_snapshot_is_equivalent_and_a_failed_verify_deletes_nothing() {
        let mut world = fork_world();
        let b = &mut world.b;
        crate::sync::sim_tests::local(b, crate::sync::sim_tests::delete(5, 400, crate::sync::sim_tests::M));
        crate::sync::sim_tests::publish(b, &mut world.remote).unwrap();
        sync_stream(&mut world.c, &world.remote, "b", &all);
        let b = &world.b;

        let snapshot = build_snapshot(b, VAULT).unwrap();
        assert_eq!(snapshot.forks, BTreeMap::from([("a".to_owned(), 1)]), "forks preserved");
        assert!(snapshot.ops.iter().any(|op| !op.body.obs.is_empty()), "obs preserved");
        assert!(snapshot.ops.iter().any(|op| op.dots.len() == 1 && op.dots[0].device == "a"));
        let decoded = Snapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(restore_equivalent(&decoded).unwrap().hash, b.projection(VAULT).unwrap().hash);
        assert!(verify_snapshot(b, VAULT, &snapshot).unwrap());
        // Each is load-bearing: dropping the obs restores M (the re-delete no
        // longer saw the fork range), so the hash moves.
        let full = b.projection(VAULT).unwrap().hash;
        let mut no_obs = snapshot.clone();
        no_obs.ops.iter_mut().for_each(|op| op.body.obs.clear());
        assert_ne!(restore_equivalent(&no_obs).unwrap().hash, full);
        // The forks are also recomputed from the two op ids at (a, 1); a lost
        // forks row is caught by the round trip comparison.
        let mut no_forks = snapshot.clone();
        no_forks.forks.clear();
        assert_ne!(no_forks, snapshot);

        let c_cursor = world.c.cursor(VAULT, "b").unwrap().unwrap().applied_seq;
        let ok = segments_deletable(b, VAULT, "b", &[c_cursor], true, &snapshot).unwrap();
        assert_eq!(ok.last().map(|r| r.1), Some(c_cursor));
        let mut tampered = snapshot.clone();
        tampered.ops.pop();
        let verified = verify_snapshot(b, VAULT, &tampered).unwrap();
        assert!(!verified);
        assert!(segments_deletable(b, VAULT, "b", &[c_cursor], verified, &tampered).unwrap().is_empty());
        // A slower reader bounds the deletable prefix; retired/forked streams never qualify.
        assert!(segments_deletable(b, VAULT, "b", &[0], true, &snapshot).unwrap().is_empty());
        assert!(segments_deletable(b, VAULT, "a", &[c_cursor], true, &snapshot).unwrap().is_empty());
        assert!(!compaction_due(b, VAULT, now_ms()).unwrap());
        assert!(compaction_due(b, VAULT, now_ms() + MAX_SEGMENT_AGE_MS).unwrap());
        // Tombstones stay; only a deleted entity's payload may go.
        let p = b.projection(VAULT).unwrap();
        assert!(payload_droppable(&p, &(crate::sync::ops::Kind::Memo, crate::sync::sim_tests::M.to_owned())));
        assert!(!payload_droppable(&p, &(crate::sync::ops::Kind::Memo, world.copy_id.clone())));
        assert!(snapshot.ops.iter().any(|op| op.body.op_type == "delete"));
    }

    #[test]
    fn checkpoints_keep_the_newest_three() {
        let dir = TempDir::new("checkpoints");
        let mut store = crate::sync::sim_tests::device("a");
        crate::sync::sim_tests::local(&mut store, crate::sync::sim_tests::create(1, 1_000, crate::sync::sim_tests::M, "x"));
        let paths: Vec<PathBuf> = (0..5).map(|_| checkpoint(&mut store, &dir.0).unwrap()).collect();
        let files = std::fs::read_dir(&dir.0).unwrap().count();
        assert_eq!(files, KEEP_CHECKPOINTS);
        let rows: Vec<String> = {
            let mut stmt = store.conn().prepare("SELECT path FROM checkpoints ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
        };
        let newest: Vec<String> = paths[2..].iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(rows, newest);
        // A checkpoint is a working copy of the store.
        let copy = Store::open(&paths[4]).unwrap();
        assert_eq!(copy.projection(VAULT).unwrap().hash, store.projection(VAULT).unwrap().hash);
    }
}
