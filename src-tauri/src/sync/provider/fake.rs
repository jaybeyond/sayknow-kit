//! In-memory [`ObjectStore`] for tests: fault injection, list lag, a request
//! counter, and an overwrite detector that fails the test the moment any path
//! would replace the bytes of a create-only object (AC 18).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use crate::sync::publish::RemoteObject;
use super::{check_key, keyed_listing, ListQuery, ListedObject, ObjectStore, ProviderError, Quota};

/// The operation a queued fault applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Op {
    List,
    Get,
    PutCreate,
    PutReplace,
    Delete,
    Quota,
    EnsureContainer,
}

struct StoredCopy {
    provider_ref: String,
    etag: String,
    bytes: Vec<u8>,
    /// `list` calls this copy stays invisible for.
    hidden_lists: usize,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Vec<StoredCopy>>,
    /// Keys first written by `put_create`: their bytes must never change.
    create_only_keys: BTreeSet<String>,
    faults: BTreeMap<Op, VecDeque<ProviderError>>,
    list_lag: usize,
    requests: usize,
    next_id: u64,
    /// Test-only bug: `put_create` skips its existence check.
    ignore_existing: bool,
}

pub(crate) struct FakeStore {
    create_only: bool,
    identity: Mutex<String>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FakeStore {
    /// `create_only == false` behaves like Drive: names repeat, nothing is refused.
    pub(crate) fn new(create_only: bool) -> Self {
        Self { create_only, identity: Mutex::new("fake|default".to_owned()), state: Mutex::default() }
    }

    pub(crate) fn set_identity(&self, identity: &str) {
        *lock(&self.identity) = identity.to_owned();
    }

    /// Requests that reached the store (key-safety refusals never do).
    pub(crate) fn requests(&self) -> usize {
        lock(&self.state).requests
    }

    /// The next call of `op` fails with `err` (queued after earlier faults).
    pub(crate) fn fail_next(&self, op: Op, err: ProviderError) {
        self.fail_next_n(op, err, 1);
    }

    pub(crate) fn fail_next_n(&self, op: Op, err: ProviderError, n: usize) {
        let mut state = lock(&self.state);
        let queue = state.faults.entry(op).or_default();
        queue.extend(std::iter::repeat_n(err, n));
    }

    /// Objects written from now on stay out of the next `lag` listings.
    pub(crate) fn set_list_lag(&self, lag: usize) {
        lock(&self.state).list_lag = lag;
    }

    /// Simulates an adapter bug: `put_create` no longer refuses a taken name.
    pub(crate) fn inject_overwrite_bug(&self) {
        lock(&self.state).ignore_existing = true;
    }

    /// Every stored copy under `key`, oldest first.
    pub(crate) fn copies(&self, key: &str) -> Vec<Vec<u8>> {
        lock(&self.state).objects.get(key).map(|copies| copies.iter().map(|c| c.bytes.clone()).collect()).unwrap_or_default()
    }

    /// Writes `bytes` under `key` without counting a request, as another
    /// device would.
    pub(crate) fn seed(&self, key: &str, bytes: &[u8]) {
        let mut state = lock(&self.state);
        let copy = new_copy(&mut state, self.create_only, key, bytes);
        state.create_only_keys.insert(key.to_owned());
        state.objects.entry(key.to_owned()).or_default().push(copy);
    }

    /// Counts the request, then pops a queued fault for `op`.
    fn begin(&self, op: Op) -> Result<MutexGuard<'_, State>, ProviderError> {
        let mut state = lock(&self.state);
        state.requests += 1;
        match state.faults.get_mut(&op).and_then(VecDeque::pop_front) {
            Some(err) => Err(err),
            None => Ok(state),
        }
    }
}

fn new_copy(state: &mut State, create_only: bool, key: &str, bytes: &[u8]) -> StoredCopy {
    state.next_id += 1;
    let id = state.next_id;
    let provider_ref = if create_only { key.to_owned() } else { format!("{key}#{id}") };
    StoredCopy { provider_ref, etag: format!("\"etag-{id}\""), bytes: bytes.to_vec(), hidden_lists: state.list_lag }
}

fn listed(key: &str, copy: &StoredCopy) -> ListedObject {
    ListedObject {
        key: key.to_owned(),
        provider_ref: copy.provider_ref.clone(),
        size: Some(copy.bytes.len() as u64),
        etag: Some(copy.etag.clone()),
    }
}

/// AC 18's failure condition: the bytes of a create-only key would change.
fn assert_no_overwrite(state: &State, key: &str, bytes: &[u8]) {
    let changes = state.objects.get(key).is_some_and(|copies| copies.iter().any(|c| c.bytes != bytes));
    if changes && state.create_only_keys.contains(key) {
        panic!("overwrite of create-only key {key}");
    }
}

impl ObjectStore for FakeStore {
    fn create_only(&self) -> bool {
        self.create_only
    }

    fn identity(&self) -> String {
        lock(&self.identity).clone()
    }

    async fn list(&self, q: ListQuery<'_>) -> Result<Vec<ListedObject>, ProviderError> {
        let listing = keyed_listing(q)?;
        let mut state = self.begin(Op::List)?;
        let mut out = Vec::new();
        for (key, copies) in state.objects.iter_mut() {
            for copy in copies.iter_mut() {
                if copy.hidden_lists > 0 {
                    copy.hidden_lists -= 1;
                } else if listing.matches(key) {
                    out.push(listed(key, copy));
                }
            }
        }
        Ok(out)
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>, ProviderError> {
        check_key(key)?;
        let state = self.begin(Op::Get)?;
        state.objects.get(key).and_then(|copies| copies.first()).map(|c| c.bytes.clone()).ok_or(ProviderError::NotFound)
    }

    async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        let mut state = self.begin(Op::PutCreate)?;
        let exists = state.objects.get(key).is_some_and(|copies| !copies.is_empty());
        if exists && self.create_only {
            if !state.ignore_existing {
                return Err(ProviderError::AlreadyExists);
            }
            // The buggy path replaces the stored object.
            assert_no_overwrite(&state, key, bytes);
            let copy = new_copy(&mut state, true, key, bytes);
            let out = listed(key, &copy);
            state.objects.insert(key.to_owned(), vec![copy]);
            return Ok(out);
        }
        let copy = new_copy(&mut state, self.create_only, key, bytes);
        let out = listed(key, &copy);
        state.create_only_keys.insert(key.to_owned());
        state.objects.entry(key.to_owned()).or_default().push(copy);
        Ok(out)
    }

    async fn put_replace(&self, key: &str, bytes: &[u8], if_match: Option<&str>) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        let mut state = self.begin(Op::PutReplace)?;
        let current = state.objects.get(key).and_then(|copies| copies.first()).map(|c| c.etag.clone());
        if let Some(expected) = if_match {
            if current.as_deref() != Some(expected) {
                return Err(ProviderError::PreconditionFailed);
            }
        }
        // D3: the RK-reissue manifest swap (If-Match) is the one sanctioned
        // replace of a created key; everything else stays guarded (AC 18).
        let manifest_swap = if_match.is_some()
            && super::parse_layout_key(key).is_some_and(|props| props.kind == super::RemoteKind::Manifest);
        if !manifest_swap {
            assert_no_overwrite(&state, key, bytes);
        }
        let copy = new_copy(&mut state, self.create_only, key, bytes);
        let out = listed(key, &copy);
        state.objects.insert(key.to_owned(), vec![copy]);
        Ok(out)
    }

    /// Overwrites the one copy `provider_ref` names; a copy that is gone is
    /// NotFound (never created), as Drive's ref-targeted media update.
    async fn put_replace_ref(&self, key: &str, provider_ref: &str, bytes: &[u8]) -> Result<ListedObject, ProviderError> {
        check_key(key)?;
        let mut state = self.begin(Op::PutReplace)?;
        assert_no_overwrite(&state, key, bytes);
        let next = {
            state.next_id += 1;
            state.next_id
        };
        let copy = state
            .objects
            .get_mut(key)
            .and_then(|copies| copies.iter_mut().find(|c| c.provider_ref == provider_ref))
            .ok_or(ProviderError::NotFound)?;
        copy.bytes = bytes.to_vec();
        copy.etag = format!("\"etag-{next}\"");
        Ok(listed(key, copy))
    }

    async fn delete(&self, key: &str) -> Result<(), ProviderError> {
        check_key(key)?;
        let mut state = self.begin(Op::Delete)?;
        state.objects.remove(key);
        state.create_only_keys.remove(key);
        Ok(())
    }

    /// Every copy, as Drive would return them. One request, faulted as `Get`.
    async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, ProviderError> {
        check_key(key)?;
        let state = match self.begin(Op::Get) {
            Ok(state) => state,
            Err(ProviderError::NotFound) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let copies = state.objects.get(key).map(Vec::as_slice).unwrap_or_default();
        Ok(copies.iter().map(|c| RemoteObject { provider_ref: c.provider_ref.clone(), bytes: c.bytes.clone() }).collect())
    }

    /// Removes the one copy `provider_ref` names; a missing copy is Ok.
    async fn delete_ref(&self, provider_ref: &str) -> Result<(), ProviderError> {
        let mut state = self.begin(Op::Delete)?;
        let emptied = state.objects.iter_mut().find_map(|(key, copies)| {
            let before = copies.len();
            copies.retain(|c| c.provider_ref != provider_ref);
            (copies.len() != before && copies.is_empty()).then(|| key.clone())
        });
        if let Some(key) = emptied {
            state.objects.remove(&key);
            state.create_only_keys.remove(&key);
        }
        Ok(())
    }

    async fn quota(&self) -> Result<Quota, ProviderError> {
        let state = self.begin(Op::Quota)?;
        let used = state.objects.values().flatten().map(|c| c.bytes.len() as u64).sum();
        Ok(Quota { used: Some(used), available: None })
    }

    async fn ensure_container(&self) -> Result<(), ProviderError> {
        self.begin(Op::EnsureContainer).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: &str = "v1/vault-a/log/dev-a/000000000001-000000000003.seg";
    const REC: &str = "v1/vault-a/devices/dev-a.rec";

    #[tokio::test]
    #[should_panic(expected = "overwrite of create-only key")]
    async fn detector_fires_when_put_create_overwrites() {
        let store = FakeStore::new(true);
        store.put_create(SEG, b"first").await.unwrap();
        store.inject_overwrite_bug();
        let _ = store.put_create(SEG, b"second").await;
    }

    #[tokio::test]
    #[should_panic(expected = "overwrite of create-only key")]
    async fn detector_fires_when_put_replace_targets_a_created_key() {
        let store = FakeStore::new(true);
        store.put_create(SEG, b"first").await.unwrap();
        let _ = store.put_replace(SEG, b"second", None).await;
    }

    #[tokio::test]
    async fn detector_stays_quiet_for_replaceable_keys_and_drive_duplicates() {
        let store = FakeStore::new(true);
        store.put_replace(REC, b"v1", None).await.unwrap();
        store.put_replace(REC, b"v2", None).await.unwrap();
        assert_eq!(store.copies(REC), vec![b"v2".to_vec()]);

        let drive = FakeStore::new(false);
        drive.put_create(SEG, b"ours").await.unwrap();
        drive.put_create(SEG, b"theirs").await.unwrap();
        assert_eq!(drive.copies(SEG), vec![b"ours".to_vec(), b"theirs".to_vec()]);
        let listed = drive.list(ListQuery::Prefix("v1/")).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_ne!(listed[0].provider_ref, listed[1].provider_ref);
    }

    #[tokio::test]
    async fn faults_fire_for_the_named_op_in_order_and_count_requests() {
        let store = FakeStore::new(true);
        store.fail_next_n(Op::Get, ProviderError::Transient("down".to_owned()), 2);
        store.fail_next(Op::Get, ProviderError::QuotaExceeded);
        store.put_create(SEG, b"x").await.unwrap();
        assert_eq!(store.get(SEG).await, Err(ProviderError::Transient("down".to_owned())));
        assert_eq!(store.get(SEG).await, Err(ProviderError::Transient("down".to_owned())));
        assert_eq!(store.get(SEG).await, Err(ProviderError::QuotaExceeded));
        assert_eq!(store.get(SEG).await.unwrap(), b"x");
        assert_eq!(store.requests(), 5);
    }

    #[tokio::test]
    async fn list_lag_hides_new_objects_for_the_next_listings_only() {
        let store = FakeStore::new(true);
        store.put_create("v1/vault-a/old.seg", b"o").await.unwrap();
        store.set_list_lag(2);
        store.put_create(SEG, b"n").await.unwrap();
        let keys = |l: Vec<ListedObject>| l.into_iter().map(|o| o.key).collect::<Vec<_>>();
        for _ in 0..2 {
            assert_eq!(keys(store.list(ListQuery::Prefix("v1/")).await.unwrap()), ["v1/vault-a/old.seg"]);
        }
        assert_eq!(keys(store.list(ListQuery::Prefix("v1/")).await.unwrap()), [SEG, "v1/vault-a/old.seg"]);
        // `get` is never lagged.
        assert_eq!(store.get(SEG).await.unwrap(), b"n");
    }

    #[tokio::test]
    async fn quota_reports_stored_bytes() {
        let store = FakeStore::new(true);
        store.put_create(SEG, b"1234").await.unwrap();
        assert_eq!(store.quota().await, Ok(Quota { used: Some(4), available: None }));
    }
}
