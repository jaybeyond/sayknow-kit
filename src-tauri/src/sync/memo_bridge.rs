//! Bridge between clipboard memos (`clipboard.json`) and the sync store (G4).
//!
//! Once a [`MemoSink`] is installed, memo commands are store-first: under the
//! clipboard lock they compute the before/after entry, hand it to the sink,
//! and only when the sink commits do they mutate, save and emit. A sink error
//! leaves the entry untouched. A crash after the commit and before the save is
//! repaired by [`mirror`], a pure function of the current entries and the
//! projection. Lock order is always clipboard → store.
//!
//! Contract deviations (G4 design, memo_bridge.rs):
//! - `memo_ops` diffs `after` against `before` when both are given, and
//!   against `head` otherwise, so a local mirror that lags the projection
//!   never reverts a remote change the user did not touch. Entries that are
//!   not memos, or whose id differs from `entity_id`, produce nothing.
//! - A sink addresses a memo by [`resolve_memo_id`], so a clipboard that
//!   still carries pre-migration `memo-<v4>` ids reaches the migrated entity.
//!   `changed` returns the entity id the entry must carry from now on.
//! - Editing a memo whose entity is a tombstone recreates it (`create` with
//!   its pin, plus `note`) under a fresh UUIDv7: a create under the
//!   tombstoned id would have seen the delete and stay invisible. `delete`
//!   needs a live head.
//! - `delete_all` commits one `delete` per observed (resolved) id even when
//!   the store has no live entity for it: a tombstone of an unknown id is
//!   harmless.
//! - `mirror` takes `now_ms` explicitly, which keeps it deterministic.
//!   Existing memo slots are updated or dropped in place, also across the
//!   one-time `memo-<v4>` → migrated id remap (ts and position kept); memos
//!   new to the clipboard are prepended newest first, ties by id.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::merge::{EntityState, MemoState, Projection};
use super::migration::{memo_entity_id, memo_restore_id};
use super::ops::{HlcClock, Kind, OpBody, FORMAT_V};
use super::store::{now_ms, Store};
use crate::clipboard::{make_preview, ClipEntry, EntryKind};

/// Where memo changes go once sync is on (installed by G7).
pub(crate) trait MemoSink: Send + Sync {
    /// One memo command's effect; `None` before = created, `None` after =
    /// deleted. Clips never reach a sink. Commits atomically or not at all;
    /// `Ok(Some(id))` means the entry must take the entity id `id` from now
    /// on (resolved or recreated), `Ok(None)` keeps the entry id.
    fn changed(&self, before: Option<&ClipEntry>, after: Option<&ClipEntry>) -> Result<Option<String>, String>;
    /// One transaction with one `memo.delete` per id; on error nothing is
    /// committed.
    fn delete_all(&self, ids: &[String]) -> Result<(), String>;
}

/// The entity id a clipboard memo id addresses in `p`: `id` itself when it
/// names a memo entity; else the migrated `memo_entity_id(id)` when that
/// exists, unless it is a tombstone and finalize restored the memo under a
/// live `memo_restore_id(id)` (AD-A3), which then wins; else `id`.
pub(crate) fn resolve_memo_id(p: &Projection, id: &str) -> String {
    let state = |key: &str| p.entities.get(&(Kind::Memo, key.to_owned()));
    if state(id).is_some() {
        return id.to_owned();
    }
    let migrated = memo_entity_id(id);
    match state(&migrated) {
        Some(EntityState::Deleted) => {
            let restored = memo_restore_id(id);
            if matches!(state(&restored), Some(EntityState::Memo(_))) {
                restored
            } else {
                migrated
            }
        }
        Some(_) => migrated,
        None => id.to_owned(),
    }
}

fn note_text(entry: &ClipEntry) -> &str {
    entry.note.as_deref().unwrap_or("")
}

fn memo_entry<'a>(entity_id: &str, entry: Option<&'a ClipEntry>) -> Option<&'a ClipEntry> {
    entry.filter(|e| e.kind == EntryKind::Memo && e.id == entity_id)
}

/// The memo ops one change needs, as `(op_type, payload)`. Pure.
pub(crate) fn memo_ops(
    entity_id: &str,
    head: Option<&MemoState>,
    before: Option<&ClipEntry>,
    after: Option<&ClipEntry>,
) -> Vec<(&'static str, Value)> {
    let before = memo_entry(entity_id, before);
    let Some(after) = memo_entry(entity_id, after) else {
        // Only an explicit memo deletion deletes, and only a live entity.
        return match (before, head) {
            (Some(_), Some(_)) => vec![("delete", json!({}))],
            _ => Vec::new(),
        };
    };
    let Some(head) = head else {
        let mut ops = vec![("create", json!({"text": after.text, "pinned": after.pinned}))];
        if !note_text(after).is_empty() {
            ops.push(("note", json!({"note": note_text(after)})));
        }
        return ops;
    };
    let (text, pinned, note) = match before {
        Some(b) => (b.text.as_str(), b.pinned, note_text(b)),
        None => (head.text.as_str(), head.pinned, head.note.as_str()),
    };
    let mut ops = Vec::new();
    if after.text != text {
        ops.push(("edit", json!({"text": after.text, "base_op_id": head.head_op_id})));
    }
    if after.pinned != pinned {
        ops.push(("pin", json!({"pinned": after.pinned})));
    }
    if note_text(after) != note {
        ops.push(("note", json!({"note": note_text(after)})));
    }
    ops
}

/// The store-backed sink: one `insert_local_batch` transaction per call.
pub(crate) struct StoreMemoSink {
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) vault: String,
    pub(crate) clock: Arc<Mutex<HlcClock>>,
}

impl StoreMemoSink {
    fn body(&self, entity_id: &str, op_type: &str, payload: Value) -> OpBody {
        let hlc = self.clock.lock().unwrap_or_else(|e| e.into_inner()).tick(now_ms());
        OpBody {
            v: FORMAT_V,
            op_id: uuid::Uuid::now_v7().hyphenated().to_string(),
            // Replaced by the store with this vault's device id and frontier.
            origin_device: String::new(),
            hlc,
            ctx: Default::default(),
            obs: Vec::new(),
            kind: Kind::Memo,
            entity_id: entity_id.to_owned(),
            op_type: op_type.to_owned(),
            payload,
        }
    }

    fn insert(&self, store: &mut Store, bodies: Vec<OpBody>) -> Result<(), String> {
        if bodies.is_empty() {
            return Ok(());
        }
        store.insert_local_batch(&self.vault, bodies, Vec::new(), Vec::new()).map(|_| ()).map_err(|e| e.to_string())
    }
}

impl MemoSink for StoreMemoSink {
    fn changed(&self, before: Option<&ClipEntry>, after: Option<&ClipEntry>) -> Result<Option<String>, String> {
        let Some(entry_id) = after.or(before).filter(|e| e.kind == EntryKind::Memo).map(|e| e.id.clone()) else {
            return Ok(None);
        };
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let projection = store.projection(&self.vault).map_err(|e| e.to_string())?;
        let resolved = resolve_memo_id(&projection, &entry_id);
        let state = projection.entities.get(&(Kind::Memo, resolved.clone()));
        let rebind = |e: Option<&ClipEntry>, id: &str| {
            e.filter(|e| e.id == entry_id).map(|e| ClipEntry { id: id.to_owned(), ..e.clone() })
        };
        let after = rebind(after, &resolved);
        let (entity_id, ops) = match (state, &after) {
            // The entity was deleted (remotely, before the mirror ran) and the
            // user still holds the memo: recreate it under a fresh id.
            (Some(EntityState::Deleted), Some(after)) if after.kind == EntryKind::Memo => {
                let fresh = uuid::Uuid::now_v7().hyphenated().to_string();
                let after = ClipEntry { id: fresh.clone(), ..after.clone() };
                (fresh.clone(), memo_ops(&fresh, None, None, Some(&after)))
            }
            _ => {
                let head = match state {
                    Some(EntityState::Memo(m)) => Some(m),
                    _ => None,
                };
                let before = rebind(before, &resolved);
                (resolved.clone(), memo_ops(&resolved, head, before.as_ref(), after.as_ref()))
            }
        };
        let bodies = ops.into_iter().map(|(op_type, payload)| self.body(&entity_id, op_type, payload)).collect();
        self.insert(&mut store, bodies)?;
        Ok((after.is_some() && entity_id != entry_id).then_some(entity_id))
    }

    fn delete_all(&self, ids: &[String]) -> Result<(), String> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let projection = store.projection(&self.vault).map_err(|e| e.to_string())?;
        let unique: BTreeSet<String> = ids.iter().map(|id| resolve_memo_id(&projection, id)).collect();
        let bodies = unique.iter().map(|id| self.body(id, "delete", json!({}))).collect();
        self.insert(&mut store, bodies)
    }
}

/// The clipboard entries with every memo replaced by the projection's live
/// memos. Clips are unchanged and keep their positions; a memo whose entry id
/// resolves to its entity (the one-time `memo-<v4>` → migrated remap) keeps
/// its ts and slot. Pure.
pub(crate) fn mirror(entries: &[ClipEntry], p: &Projection, now_ms: i64) -> Vec<ClipEntry> {
    let entries: Vec<ClipEntry> = entries
        .iter()
        .map(|e| match e.kind {
            EntryKind::Memo => ClipEntry { id: resolve_memo_id(p, &e.id), ..e.clone() },
            EntryKind::Clip => e.clone(),
        })
        .collect();
    let mut memos: Vec<ClipEntry> = p
        .entities
        .iter()
        .filter_map(|((kind, id), state)| match (kind, state) {
            (Kind::Memo, EntityState::Memo(m)) => Some((id, m)),
            _ => None,
        })
        .map(|(id, m)| {
            let ts = entries
                .iter()
                .find(|e| e.kind == EntryKind::Memo && &e.id == id && e.text == m.text)
                .map_or(now_ms, |e| e.ts);
            ClipEntry {
                id: id.clone(),
                text: m.text.clone(),
                preview: make_preview(&m.text),
                ts,
                pinned: m.pinned,
                note: (!m.note.is_empty()).then(|| m.note.clone()),
                kind: EntryKind::Memo,
                conflict_of: m.conflict_of.clone(),
            }
        })
        .collect();
    // Deterministic: newest first, ties by id.
    memos.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.id.cmp(&b.id)));
    crate::clipboard::splice_memos(&entries, memos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::sim_tests::{all, device, publish, sync_stream, Remote, VAULT};

    const ID: &str = "memo-0190a000-0000-7000-8000-0000000000aa";
    const HEAD: &str = "0190a000-0000-7000-8000-0000000000cc";

    fn memo(text: &str, pinned: bool, note: Option<&str>) -> ClipEntry {
        ClipEntry {
            id: ID.into(),
            text: text.into(),
            preview: make_preview(text),
            ts: 10,
            pinned,
            note: note.map(Into::into),
            kind: EntryKind::Memo,
            conflict_of: None,
        }
    }

    fn clip(text: &str, ts: i64) -> ClipEntry {
        ClipEntry {
            id: format!("clip-{text}"),
            text: text.into(),
            preview: make_preview(text),
            ts,
            pinned: false,
            note: None,
            kind: EntryKind::Clip,
            conflict_of: None,
        }
    }

    fn head(text: &str, pinned: bool, note: &str) -> MemoState {
        MemoState { text: text.into(), pinned, note: note.into(), head_op_id: HEAD.into(), conflict_of: None }
    }

    fn types(ops: &[(&'static str, Value)]) -> Vec<&'static str> {
        ops.iter().map(|(t, _)| *t).collect()
    }

    #[test]
    fn create_without_head_carries_text_pin_and_a_note_only_when_set() {
        let ops = memo_ops(ID, None, None, Some(&memo("hi", true, None)));
        assert_eq!(ops, vec![("create", json!({"text": "hi", "pinned": true}))]);
        let ops = memo_ops(ID, None, None, Some(&memo("hi", false, Some("n"))));
        assert_eq!(
            ops,
            vec![("create", json!({"text": "hi", "pinned": false})), ("note", json!({"note": "n"}))]
        );
        // A local memo the store never saw (failed sink call) is created.
        let ops = memo_ops(ID, None, Some(&memo("old", false, None)), Some(&memo("new", false, None)));
        assert_eq!(types(&ops), ["create"]);
    }

    #[test]
    fn edit_bases_on_the_projection_head() {
        let ops = memo_ops(ID, Some(&head("a", false, "")), Some(&memo("a", false, None)), Some(&memo("b", false, None)));
        assert_eq!(ops, vec![("edit", json!({"text": "b", "base_op_id": HEAD}))]);
    }

    #[test]
    fn pin_and_note_changes_map_none_to_empty() {
        let h = head("a", false, "");
        let ops = memo_ops(ID, Some(&h), Some(&memo("a", false, None)), Some(&memo("a", true, None)));
        assert_eq!(ops, vec![("pin", json!({"pinned": true}))]);
        let ops = memo_ops(ID, Some(&h), Some(&memo("a", false, None)), Some(&memo("a", false, Some("n"))));
        assert_eq!(ops, vec![("note", json!({"note": "n"}))]);
        let h = head("a", false, "n");
        let ops = memo_ops(ID, Some(&h), Some(&memo("a", false, Some("n"))), Some(&memo("a", false, None)));
        assert_eq!(ops, vec![("note", json!({"note": ""}))]);
        // None and "" are the same note.
        let ops = memo_ops(ID, Some(&head("a", false, "")), Some(&memo("a", false, Some(""))), Some(&memo("a", false, None)));
        assert!(ops.is_empty());
    }

    #[test]
    fn delete_unchanged_and_clips() {
        let h = head("a", false, "");
        assert_eq!(memo_ops(ID, Some(&h), Some(&memo("a", false, None)), None), vec![("delete", json!({}))]);
        assert!(memo_ops(ID, None, Some(&memo("a", false, None)), None).is_empty(), "nothing live to delete");
        assert!(memo_ops(ID, Some(&h), Some(&memo("a", false, None)), Some(&memo("a", false, None))).is_empty());
        // Without a before, the head is the base: equal → nothing.
        assert!(memo_ops(ID, Some(&h), None, Some(&memo("a", false, None))).is_empty());
        let mut c = clip("x", 1);
        c.id = ID.into();
        assert!(memo_ops(ID, None, None, Some(&c)).is_empty(), "clips never become ops");
        assert!(memo_ops(ID, Some(&h), Some(&c), None).is_empty());
    }

    fn sink(store: Store) -> StoreMemoSink {
        StoreMemoSink {
            store: Arc::new(Mutex::new(store)),
            vault: VAULT.to_owned(),
            clock: Arc::new(Mutex::new(HlcClock::default())),
        }
    }

    fn op_count(sink: &StoreMemoSink) -> i64 {
        sink.store.lock().unwrap().conn().query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0)).unwrap()
    }

    fn projected(sink: &StoreMemoSink, id: &str) -> Option<EntityState> {
        sink.store.lock().unwrap().projection(VAULT).unwrap().entities.get(&(Kind::Memo, id.to_owned())).cloned()
    }

    #[test]
    fn store_sink_writes_memo_ops_and_ignores_clips() {
        let s = sink(device("a"));
        let m = memo("hello", false, Some("n"));
        s.changed(None, Some(&m)).unwrap();
        assert_eq!(op_count(&s), 2, "create + note in one batch");
        let Some(EntityState::Memo(state)) = projected(&s, ID) else { panic!("memo missing") };
        assert_eq!((state.text.as_str(), state.pinned, state.note.as_str()), ("hello", false, "n"));

        let mut edited = m.clone();
        edited.text = "bye".into();
        edited.pinned = true;
        s.changed(Some(&m), Some(&edited)).unwrap();
        assert_eq!(op_count(&s), 4);
        let Some(EntityState::Memo(state)) = projected(&s, ID) else { panic!("memo missing") };
        assert_eq!((state.text.as_str(), state.pinned), ("bye", true));

        s.changed(Some(&edited), Some(&edited)).unwrap();
        s.changed(None, Some(&clip("loose", 1))).unwrap();
        assert_eq!(op_count(&s), 4, "no op for unchanged memos or clips");

        s.changed(Some(&edited), None).unwrap();
        assert_eq!(projected(&s, ID), Some(EntityState::Deleted));
    }

    #[test]
    fn store_sink_needs_the_vault_identity_and_delete_all_is_one_transaction() {
        let s = StoreMemoSink { vault: "vault-x".into(), ..sink(device("a")) };
        assert!(s.changed(None, Some(&memo("a", false, None))).is_err());

        let s = sink(device("a"));
        let other = ClipEntry { id: "memo-2".into(), ..memo("b", false, None) };
        s.changed(None, Some(&memo("a", false, None))).unwrap();
        s.changed(None, Some(&other)).unwrap();
        let before = op_count(&s);
        // One bad id rolls back every delete.
        assert!(s.delete_all(&[ID.to_owned(), "bad id/".to_owned()]).is_err());
        assert_eq!(op_count(&s), before);
        assert!(matches!(projected(&s, ID), Some(EntityState::Memo(_))));
        s.delete_all(&[ID.to_owned(), "memo-2".to_owned(), ID.to_owned()]).unwrap();
        assert_eq!(op_count(&s), before + 2, "one delete per observed id");
        assert_eq!(projected(&s, ID), Some(EntityState::Deleted));
        assert_eq!(projected(&s, "memo-2"), Some(EntityState::Deleted));
        s.delete_all(&[]).unwrap();
        assert_eq!(op_count(&s), before + 2);
    }

    fn projection(s: &StoreMemoSink) -> Projection {
        s.store.lock().unwrap().projection(VAULT).unwrap()
    }

    #[test]
    fn mirror_round_trip_keeps_clips_and_surfaces_conflict_copies() {
        let m1 = memo("hello", false, Some("note"));
        let m2 = ClipEntry { id: "memo-2".into(), ts: 5, pinned: true, ..memo("second", true, None) };
        let entries = vec![clip("c1", 20), m1.clone(), clip("c2", 7), m2.clone(), clip("c3", 1)];
        let a = sink(device("a"));
        for e in &entries {
            a.changed(None, Some(e)).unwrap();
        }
        let p = projection(&a);
        assert_eq!(mirror(&entries, &p, 999), entries, "same memos, same ids, clips untouched");

        // A concurrent edit from b makes a conflict copy.
        let mut remote = Remote::new(false);
        publish(&mut a.store.lock().unwrap(), &mut remote).unwrap();
        let b = sink(device("b"));
        sync_stream(&mut b.store.lock().unwrap(), &remote, "a", &all);
        let from_b = memo("from b", false, Some("note"));
        b.changed(Some(&m1), Some(&from_b)).unwrap();
        let from_a = memo("from a", false, Some("note"));
        a.changed(Some(&m1), Some(&from_a)).unwrap();
        publish(&mut a.store.lock().unwrap(), &mut remote).unwrap();
        publish(&mut b.store.lock().unwrap(), &mut remote).unwrap();
        sync_stream(&mut a.store.lock().unwrap(), &remote, "b", &all);
        sync_stream(&mut b.store.lock().unwrap(), &remote, "a", &all);
        let pa = projection(&a);
        assert_eq!(pa.hash, projection(&b).hash);

        let local = vec![clip("c1", 20), from_a.clone(), clip("c2", 7), m2.clone(), clip("c3", 1)];
        let out = mirror(&local, &pa, 999);
        assert_eq!(out, mirror(&local, &pa, 999), "deterministic");
        let clips: Vec<&ClipEntry> = out.iter().filter(|e| e.kind == EntryKind::Clip).collect();
        let want: Vec<&ClipEntry> = local.iter().filter(|e| e.kind == EntryKind::Clip).collect();
        assert_eq!(clips, want);
        let memos: Vec<&ClipEntry> = out.iter().filter(|e| e.kind == EntryKind::Memo).collect();
        assert_eq!(memos.len(), 3, "{memos:?}");
        let copy = memos.iter().find(|e| e.conflict_of.is_some()).expect("conflict copy");
        assert_eq!(copy.conflict_of.as_deref(), Some(ID));
        assert_eq!(copy.ts, 999, "new to the clipboard: now");
        let original = memos.iter().find(|e| e.id == ID).unwrap();
        let texts = BTreeSet::from([original.text.as_str(), copy.text.as_str()]);
        assert_eq!(texts, BTreeSet::from(["from a", "from b"]));
        assert_eq!(original.note.as_deref(), Some("note"));
        assert_eq!(memos.iter().find(|e| e.id == "memo-2").unwrap(), &&m2, "unchanged memo keeps its ts");
        // Applying the mirror again changes nothing.
        assert_eq!(mirror(&out, &pa, 1_234), out);
    }

    #[test]
    fn mirror_drops_deleted_memos_and_keeps_clips() {
        let m1 = memo("hello", false, None);
        let entries = vec![clip("c1", 20), m1.clone(), clip("c2", 7)];
        let a = sink(device("a"));
        a.changed(None, Some(&m1)).unwrap();
        a.delete_all(&[ID.to_owned()]).unwrap();
        let out = mirror(&entries, &projection(&a), 50);
        assert_eq!(out, vec![clip("c1", 20), clip("c2", 7)]);
    }

    /// AC 26: a crash after step 2 (deletes committed, file not yet
    /// rewritten) loses nothing: the next mirror of the old file shows the
    /// memos deleted and the clips kept.
    #[test]
    fn wipe_crash_after_the_delete_commit_mirrors_to_clips_only() {
        let m2 = ClipEntry { id: "memo-2".into(), ..memo("two", true, None) };
        let on_disk = vec![clip("c1", 20), memo("one", false, None), clip("c2", 7), m2];
        let a = sink(device("a"));
        for e in &on_disk {
            a.changed(None, Some(e)).unwrap();
        }
        let handle = crate::clipboard::ClipboardHandle::new();
        handle.state.lock().unwrap().entries = on_disk.clone();
        // The save never happens: the process dies after the commit.
        crate::clipboard::wipe_entries(&handle, Some(&a), |_, _| {}).unwrap();
        let p = projection(&a);
        assert_eq!(p.entities.get(&(Kind::Memo, ID.to_owned())), Some(&EntityState::Deleted));
        assert_eq!(p.entities.get(&(Kind::Memo, "memo-2".to_owned())), Some(&EntityState::Deleted));
        assert_eq!(mirror(&on_disk, &p, 1), vec![clip("c1", 20), clip("c2", 7)]);

        // A failing commit (no identity) leaves the store and entries as they were.
        let broken = StoreMemoSink { vault: "vault-x".into(), ..sink(device("b")) };
        let handle = crate::clipboard::ClipboardHandle::new();
        handle.state.lock().unwrap().entries = on_disk.clone();
        assert!(crate::clipboard::wipe_entries(&handle, Some(&broken), |_, _| panic!("no save")).is_err());
        assert_eq!(handle.state.lock().unwrap().entries, on_disk);
        assert_eq!(op_count(&broken), 0);
    }

    fn memo_with(id: &str, text: &str, ts: i64) -> ClipEntry {
        ClipEntry { id: id.into(), ts, ..memo(text, false, None) }
    }

    fn handle_with(entries: Vec<ClipEntry>) -> std::sync::Arc<crate::clipboard::ClipboardHandle> {
        let handle = crate::clipboard::ClipboardHandle::new();
        handle.state.lock().unwrap().entries = entries;
        handle
    }

    fn entries_of(handle: &crate::clipboard::ClipboardHandle) -> Vec<ClipEntry> {
        handle.state.lock().unwrap().entries.clone()
    }

    fn memo_count(entries: &[ClipEntry]) -> usize {
        entries.iter().filter(|e| e.kind == EntryKind::Memo).count()
    }

    #[test]
    fn store_first_commands_write_one_op_each() {
        use crate::clipboard::{create_memo_in, delete_entry_in, set_note_in, toggle_pin_in, update_memo_in};
        let a = sink(device("a"));
        let handle = handle_with(vec![clip("c1", 1)]);
        let memo = create_memo_in(&handle, Some(&a), "first", 10, |_, _| {}).unwrap();
        assert_eq!(op_count(&a), 1, "create");
        update_memo_in(&handle, Some(&a), &memo.id, "second", 11, |_, _| {}).unwrap();
        assert_eq!(op_count(&a), 2, "edit");
        toggle_pin_in(&handle, Some(&a), &memo.id, |_, _| {}).unwrap();
        assert_eq!(op_count(&a), 3, "pin");
        set_note_in(&handle, Some(&a), &memo.id, Some("n".into()), |_, _| {}).unwrap();
        assert_eq!(op_count(&a), 4, "note");
        let Some(EntityState::Memo(state)) = projected(&a, &memo.id) else { panic!("memo missing") };
        assert_eq!((state.text.as_str(), state.pinned, state.note.as_str()), ("second", true, "n"));
        assert_eq!(mirror(&entries_of(&handle), &projection(&a), 99), entries_of(&handle), "in sync");
        delete_entry_in(&handle, Some(&a), &memo.id, |_, _| {}).unwrap();
        assert_eq!(op_count(&a), 5, "delete");
        assert_eq!(projected(&a, &memo.id), Some(EntityState::Deleted));
        assert_eq!(entries_of(&handle), vec![clip("c1", 1)]);
    }

    /// A store-first failure (no vault identity) leaves the entry, the file
    /// and the store untouched.
    #[test]
    fn store_first_commands_change_nothing_when_the_store_rejects() {
        use crate::clipboard::{create_memo_in, toggle_pin_in, update_memo_in};
        let broken = StoreMemoSink { vault: "vault-x".into(), ..sink(device("a")) };
        let entries = vec![memo_with(ID, "x", 5), clip("c1", 1)];
        let handle = handle_with(entries.clone());
        let no_save = |_: &[ClipEntry], _: usize| panic!("no save");
        assert!(create_memo_in(&handle, Some(&broken), "new", 10, no_save).is_err());
        assert!(update_memo_in(&handle, Some(&broken), ID, "edited", 10, no_save).is_err());
        assert!(toggle_pin_in(&handle, Some(&broken), ID, no_save).is_err());
        assert_eq!(entries_of(&handle), entries);
        assert_eq!(op_count(&broken), 0);
    }

    /// Native with an unmirrored clipboard: the store holds the migrated
    /// entity `memo_entity_id(old)`, the clipboard still the `memo-<v4>` id.
    fn migrated(old: &str, text: &str) -> StoreMemoSink {
        let s = sink(device("a"));
        s.changed(None, Some(&memo_with(&memo_entity_id(old), text, 1))).unwrap();
        s
    }

    #[test]
    fn unmirrored_edit_reaches_the_migrated_entity_and_mirrors_to_one_memo() {
        let old = "memo-5f0c1d2e-3b4a-4c5d-8e6f-7a8b9c0d1e2f";
        let a = migrated(old, "before");
        let before_ops = op_count(&a);
        let handle = handle_with(vec![clip("c1", 20), memo_with(old, "before", 7)]);
        let edited = crate::clipboard::update_memo_in(&handle, Some(&a), old, "after", 30, |_, _| {}).unwrap();
        assert_eq!(edited.id, memo_entity_id(old), "the entry takes the entity id");
        assert_eq!(op_count(&a), before_ops + 1, "one edit, no second create");
        let out = mirror(&entries_of(&handle), &projection(&a), 99);
        let memos: Vec<&ClipEntry> = out.iter().filter(|e| e.kind == EntryKind::Memo).collect();
        assert_eq!(memos.len(), 1, "{out:?}");
        assert_eq!((memos[0].id.as_str(), memos[0].text.as_str()), (memo_entity_id(old).as_str(), "after"));

        // The sink resolves even without the clipboard rewrite (crash before save).
        let b = migrated(old, "before");
        assert_eq!(b.changed(Some(&memo_with(old, "before", 7)), Some(&memo_with(old, "x", 8))).unwrap(), Some(memo_entity_id(old)));
        let unsaved = vec![clip("c1", 20), memo_with(old, "before", 7)];
        assert_eq!(memo_count(&mirror(&unsaved, &projection(&b), 99)), 1);
    }

    #[test]
    fn unmirrored_wipe_tombstones_the_migrated_entities_and_keeps_clips() {
        let (old1, old2) = ("memo-5f0c1d2e-3b4a-4c5d-8e6f-7a8b9c0d1e2f", "memo-6a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d");
        let a = migrated(old1, "one");
        a.changed(None, Some(&memo_with(&memo_entity_id(old2), "two", 1))).unwrap();
        let on_disk = vec![clip("c1", 20), memo_with(old1, "one", 7), clip("c2", 5), memo_with(old2, "two", 3)];
        let handle = handle_with(on_disk.clone());
        crate::clipboard::wipe_entries(&handle, Some(&a), |_, _| {}).unwrap();
        let p = projection(&a);
        assert_eq!(projected(&a, &memo_entity_id(old1)), Some(EntityState::Deleted));
        assert_eq!(projected(&a, &memo_entity_id(old2)), Some(EntityState::Deleted));
        // Crash before the save: the old file mirrors to its clips only.
        let out = mirror(&on_disk, &p, 99);
        assert_eq!(memo_count(&out), 0, "{out:?}");
        assert_eq!(out, vec![clip("c1", 20), clip("c2", 5)]);
    }

    /// AD-A3: the migrated v5 entity was deleted and finalize restored the
    /// live memo under `memo_restore_id(old)`; the old id addresses the
    /// restore, so an unmirrored edit lands there instead of a fresh copy.
    #[test]
    fn a_deleted_migrated_memo_resolves_to_its_live_restore() {
        let old = "memo-5f0c1d2e-3b4a-4c5d-8e6f-7a8b9c0d1e2f";
        let (v5, restore) = (memo_entity_id(old), memo_restore_id(old));
        let a = migrated(old, "one");
        a.changed(Some(&memo_with(&v5, "one", 1)), None).unwrap();
        assert_eq!(projected(&a, &v5), Some(EntityState::Deleted));
        assert_eq!(resolve_memo_id(&projection(&a), old), v5, "no restore yet: the tombstone");

        a.changed(None, Some(&memo_with(&restore, "one restored", 2))).unwrap();
        assert_eq!(resolve_memo_id(&projection(&a), old), restore);
        assert_eq!(resolve_memo_id(&projection(&a), &v5), v5, "an entity id names itself");

        let handle = handle_with(vec![clip("c1", 20), memo_with(old, "one restored", 7)]);
        let ops = op_count(&a);
        let edited = crate::clipboard::update_memo_in(&handle, Some(&a), old, "edited", 30, |_, _| {}).unwrap();
        assert_eq!(edited.id, restore, "the edit lands on the restore, not a fresh UUIDv7");
        assert_eq!(op_count(&a), ops + 1, "one edit, no create");
        let p = projection(&a);
        let Some(EntityState::Memo(state)) = p.entities.get(&(Kind::Memo, restore.clone())) else {
            panic!("restore missing")
        };
        assert_eq!(state.text, "edited");
        assert_eq!(projected(&a, &v5), Some(EntityState::Deleted));
        let out = mirror(&entries_of(&handle), &p, 99);
        assert_eq!(memo_count(&out), 1, "{out:?}");

        // A deleted restore is no target: the tombstone stays the answer.
        a.changed(Some(&memo_with(&restore, "edited", 30)), None).unwrap();
        assert_eq!(resolve_memo_id(&projection(&a), old), v5);
    }

    /// B deletes, A applies it, A edits before the mirror: the edit survives
    /// under a fresh id and the mirror shows exactly one memo.
    #[test]
    fn edit_after_an_applied_remote_delete_recreates_under_a_new_id() {
        let m = memo_with(ID, "shared", 10);
        let a = sink(device("a"));
        a.changed(None, Some(&m)).unwrap();
        let mut remote = Remote::new(false);
        publish(&mut a.store.lock().unwrap(), &mut remote).unwrap();
        let b = sink(device("b"));
        sync_stream(&mut b.store.lock().unwrap(), &remote, "a", &all);
        b.changed(Some(&m), None).unwrap();
        publish(&mut b.store.lock().unwrap(), &mut remote).unwrap();
        sync_stream(&mut a.store.lock().unwrap(), &remote, "b", &all);
        assert_eq!(projected(&a, ID), Some(EntityState::Deleted), "A applied the delete");

        let handle = handle_with(vec![clip("c1", 20), m.clone()]);
        let edited = crate::clipboard::update_memo_in(&handle, Some(&a), ID, "kept edit", 30, |_, _| {}).unwrap();
        assert_ne!(edited.id, ID, "fresh id");
        assert!(uuid::Uuid::parse_str(&edited.id).is_ok_and(|u| u.get_version_num() == 7), "{}", edited.id);
        assert_eq!(projected(&a, ID), Some(EntityState::Deleted));
        let Some(EntityState::Memo(state)) = projected(&a, &edited.id) else { panic!("recreated memo missing") };
        assert_eq!(state.text, "kept edit");

        let out = mirror(&entries_of(&handle), &projection(&a), 99);
        let memos: Vec<&ClipEntry> = out.iter().filter(|e| e.kind == EntryKind::Memo).collect();
        assert_eq!(memos.len(), 1, "{out:?}");
        assert_eq!((memos[0].id.as_str(), memos[0].text.as_str()), (edited.id.as_str(), "kept edit"));
        assert_eq!(memos[0].ts, 30, "the edited slot keeps its ts");
        // Deleting a tombstoned memo still commits nothing new.
        let ops = op_count(&a);
        assert_eq!(a.changed(Some(&m), None).unwrap(), None);
        assert_eq!(op_count(&a), ops);
    }

    #[test]
    fn remap_keeps_ts_and_order() {
        let (old1, old2) = ("memo-5f0c1d2e-3b4a-4c5d-8e6f-7a8b9c0d1e2f", "memo-6a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d");
        let a = migrated(old1, "one");
        a.changed(None, Some(&ClipEntry { pinned: true, ..memo_with(&memo_entity_id(old2), "two", 1) })).unwrap();
        // The older memo sits above a newer clip: positions, not ts, are kept.
        let entries = vec![
            memo_with(old2, "two", 3),
            clip("c1", 20),
            memo_with(old1, "one", 7),
            clip("c2", 5),
        ];
        let out = mirror(&entries, &projection(&a), 999);
        let want = vec![
            ClipEntry { id: memo_entity_id(old2), pinned: true, ..memo_with(old2, "two", 3) },
            clip("c1", 20),
            memo_with(&memo_entity_id(old1), "one", 7),
            clip("c2", 5),
        ];
        assert_eq!(out, want);
        assert_eq!(mirror(&out, &projection(&a), 1_234), out, "idempotent after the remap");
    }
}
