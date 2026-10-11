//! Settling uploads whose outcome is unknown (§4.2.1 R3 + Final Amendment W-1).
//!
//! An op is `attempted` once its segment's create-only upload was sent but no
//! answer was seen. Its name may already be on the remote, so it is never
//! re-stamped; the only allowed write is the very same bytes under the very
//! same key. Each cycle probes the key at 0/5/15 s, resends once, and then
//! verifies. Anything still uncertain stays `attempted` for the next cycle,
//! so the op is published exactly once and never stranded (AC 17).
//!
//! Only a store that refuses to overwrite an existing name (R2, WebDAV,
//! OneDrive) can prove our upload lost: any object other than ours under our
//! key (another writer's or an unreadable one) means ours was refused (§4.1
//! `rejected`). Drive allows duplicate names, so a foreign object there proves
//! nothing about ours; we resend our bytes and report a fork only next to
//! another writer's valid segment.

use std::future::Future;
use std::time::Duration;

/// What `put_create` reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PutOutcome {
    Created,
    /// The key is taken (AlreadyExists / 409 / 412); nothing was written.
    AlreadyExists,
}

/// A network or provider failure: the outcome is unknown, not negative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransportError;

/// One stored object under a key; `provider_ref` names this exact copy (the
/// Drive file id; elsewhere the key itself) so a duplicate can be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteObject {
    pub(crate) provider_ref: String,
    pub(crate) bytes: Vec<u8>,
}

/// The object surface the resolver needs. Adapters implement it.
pub(crate) trait SegmentRemote {
    /// Whether `put_create` refuses an existing name (true everywhere but Drive).
    fn create_only(&self) -> bool;
    /// Every stored copy under `key`; more than one only where names repeat.
    fn get_all(&self, key: &str) -> impl Future<Output = Result<Vec<RemoteObject>, TransportError>> + Send;
    fn put_create(&self, key: &str, bytes: &[u8]) -> impl Future<Output = Result<PutOutcome, TransportError>> + Send;
}

pub(crate) trait Pause {
    fn pause(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// Delays before each re-probe of the key.
pub(crate) const RECHECK_DELAYS: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(15)];

/// How the caller judged one stored object after decrypting it (W-1: chain
/// position and body hashes match ours).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Ours,
    /// A well-formed segment from another writer.
    Other,
    /// Unreadable or tampered; the caller quarantines it.
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Our segment is on the remote: mark its ops `published`.
    Published,
    /// Ours is on the remote next to another writer's valid segment under the
    /// same key (only possible on Drive): published, and the caller records a
    /// fork (§4.4) and notifies the user. An unreadable neighbour is not a
    /// fork; it is only reported through `invalid_seen` for quarantine.
    PublishedWithFork,
    /// A create-only store holds another object under our key, so ours was
    /// refused: mark `rejected`, then rotate (T6) and notify.
    Rejected,
    /// Still unknown: stay `attempted`, retry next cycle, never re-stamp.
    StillAttempted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settled {
    pub(crate) outcome: Outcome,
    /// Extra copies of ours (the judge says Ours: same chain position and
    /// body hashes), left by a Drive resend that raced a slow listing; the
    /// caller deletes them by `provider_ref`.
    pub(crate) extra_self_copies: Vec<String>,
    /// An unreadable object sat under our key in any probe of this cycle; the
    /// caller quarantines it (AC 7: state and cursor unchanged).
    pub(crate) invalid_seen: bool,
}

impl Settled {
    fn unknown(invalid_seen: bool) -> Self {
        Self { outcome: Outcome::StillAttempted, extra_self_copies: Vec::new(), invalid_seen }
    }
}

struct Listing {
    ours: Vec<String>,
    other: bool,
    invalid: bool,
}

fn classify(objects: &[RemoteObject], judge: &impl Fn(&[u8]) -> Verdict) -> Listing {
    let mut listing = Listing { ours: Vec::new(), other: false, invalid: false };
    for object in objects {
        match judge(&object.bytes) {
            Verdict::Ours => listing.ours.push(object.provider_ref.clone()),
            Verdict::Other => listing.other = true,
            Verdict::Invalid => listing.invalid = true,
        }
    }
    listing.ours.sort();
    listing
}

/// The decision for one listing, or `None` if it does not settle anything.
/// `invalid_seen` carries Invalid objects seen by earlier probes.
fn decide(listing: Listing, create_only: bool, invalid_seen: bool) -> Option<Settled> {
    let outcome = match (listing.ours.is_empty(), listing.other) {
        (false, false) => Outcome::Published,
        (false, true) => Outcome::PublishedWithFork,
        (true, true) if create_only => Outcome::Rejected,
        (true, false) if create_only && listing.invalid => Outcome::Rejected,
        (true, _) => return None,
    };
    let extra_self_copies = listing.ours.into_iter().skip(1).collect();
    Some(Settled { outcome, extra_self_copies, invalid_seen: invalid_seen || listing.invalid })
}

/// Copies of ours beyond the first (judge says Ours), from any listing of the
/// key; the engine's verify pass removes them so each name holds one copy.
#[cfg_attr(not(test), allow(dead_code))] // only tests call this so far
pub(crate) fn extra_self_copies(objects: &[RemoteObject], judge: impl Fn(&[u8]) -> Verdict) -> Vec<String> {
    classify(objects, &judge).ours.into_iter().skip(1).collect()
}

pub(crate) async fn settle_attempted(
    remote: &impl SegmentRemote,
    pause: &impl Pause,
    key: &str,
    local: &[u8],
    judge: impl Fn(&[u8]) -> Verdict,
) -> Settled {
    let create_only = remote.create_only();
    let mut invalid_seen = false;
    for delay in std::iter::once(None).chain(RECHECK_DELAYS.map(Some)) {
        if let Some(delay) = delay {
            pause.pause(delay).await;
        }
        let Ok(objects) = remote.get_all(key).await else {
            return Settled::unknown(invalid_seen);
        };
        let listing = classify(&objects, &judge);
        invalid_seen |= listing.invalid;
        if let Some(settled) = decide(listing, create_only, invalid_seen) {
            return settled;
        }
    }

    // Ours is not visible after every probe: resend the identical bytes. On
    // Drive this also publishes ours next to another writer's object (a fork)
    // or next to an unreadable one (quarantined, not a fork).
    if remote.put_create(key, local).await.is_err() {
        return Settled::unknown(invalid_seen);
    }
    let Ok(objects) = remote.get_all(key).await else {
        return Settled::unknown(invalid_seen);
    };
    let listing = classify(&objects, &judge);
    invalid_seen |= listing.invalid;
    decide(listing, create_only, invalid_seen).unwrap_or_else(|| Settled::unknown(invalid_seen))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const KEY: &str = "v1/vault/log/dev-a/000000000001-000000000003.seg";
    const OURS: &[u8] = b"sealed segment bytes";
    const THEIRS: &[u8] = b"someone else's segment";
    const GARBAGE: &[u8] = b"\x00not a segment";

    struct Stored {
        provider_ref: String,
        bytes: Vec<u8>,
        /// `get_all` calls this copy stays invisible for.
        hidden_reads: usize,
    }

    /// A store whose listings can lag behind writes and whose `put_create`
    /// response can be lost after the write lands. `create_only: false` is
    /// Drive: names repeat and nothing is ever refused.
    struct FakeStore {
        create_only: bool,
        new_write_lag: usize,
        objects: Mutex<Vec<Stored>>,
        lost_put_responses: Mutex<usize>,
        /// Scripted `get_all` results: `false` fails that call; empty succeeds.
        gets: Mutex<std::collections::VecDeque<bool>>,
        puts: Mutex<usize>,
        next_ref: Mutex<usize>,
    }

    impl FakeStore {
        fn new(create_only: bool, new_write_lag: usize) -> Self {
            Self {
                create_only,
                new_write_lag,
                objects: Mutex::new(Vec::new()),
                lost_put_responses: Mutex::new(0),
                gets: Mutex::new(std::collections::VecDeque::new()),
                puts: Mutex::new(0),
                next_ref: Mutex::new(0),
            }
        }

        fn landed(&self, bytes: &[u8], hidden_reads: usize) {
            let mut next = self.next_ref.lock().unwrap();
            *next += 1;
            let provider_ref = format!("file-{next}");
            self.objects.lock().unwrap().push(Stored { provider_ref, bytes: bytes.to_vec(), hidden_reads });
        }

        fn stored(&self) -> Vec<Vec<u8>> {
            self.objects.lock().unwrap().iter().map(|o| o.bytes.clone()).collect()
        }

        fn count(&self, bytes: &[u8]) -> usize {
            self.stored().iter().filter(|stored| stored.as_slice() == bytes).count()
        }

        fn delete(&self, provider_ref: &str) {
            self.objects.lock().unwrap().retain(|o| o.provider_ref != provider_ref);
        }

        fn puts(&self) -> usize {
            *self.puts.lock().unwrap()
        }

        fn take(counter: &Mutex<usize>) -> bool {
            let mut left = counter.lock().unwrap();
            let fired = *left > 0;
            *left = left.saturating_sub(1);
            fired
        }
    }

    impl SegmentRemote for FakeStore {
        fn create_only(&self) -> bool {
            self.create_only
        }

        async fn get_all(&self, key: &str) -> Result<Vec<RemoteObject>, TransportError> {
            assert_eq!(key, KEY);
            if self.gets.lock().unwrap().pop_front() == Some(false) {
                return Err(TransportError);
            }
            let mut objects = self.objects.lock().unwrap();
            let mut visible = Vec::new();
            for object in objects.iter_mut() {
                if object.hidden_reads == 0 {
                    visible.push(RemoteObject { provider_ref: object.provider_ref.clone(), bytes: object.bytes.clone() });
                } else {
                    object.hidden_reads -= 1;
                }
            }
            Ok(visible)
        }

        async fn put_create(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, TransportError> {
            assert_eq!(key, KEY);
            assert_eq!(bytes, OURS, "a resend must carry the same bytes");
            *self.puts.lock().unwrap() += 1;
            if self.create_only && !self.objects.lock().unwrap().is_empty() {
                return Ok(PutOutcome::AlreadyExists);
            }
            self.landed(bytes, self.new_write_lag);
            if Self::take(&self.lost_put_responses) {
                return Err(TransportError);
            }
            Ok(PutOutcome::Created)
        }
    }

    #[derive(Default)]
    struct RecordedPause(Mutex<Vec<Duration>>);

    impl Pause for RecordedPause {
        async fn pause(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    fn judge(object: &[u8]) -> Verdict {
        match object {
            OURS => Verdict::Ours,
            THEIRS => Verdict::Other,
            _ => Verdict::Invalid,
        }
    }

    async fn cycle(store: &FakeStore) -> (Settled, Vec<Duration>) {
        let pause = RecordedPause::default();
        let settled = settle_attempted(store, &pause, KEY, OURS, judge).await;
        (settled, pause.0.into_inner().unwrap())
    }

    /// Runs cycles until the op leaves `attempted`, as the engine would.
    async fn until_settled(store: &FakeStore, max_cycles: usize) -> (Outcome, usize) {
        for n in 1..=max_cycles {
            let outcome = cycle(store).await.0.outcome;
            if outcome != Outcome::StillAttempted {
                return (outcome, n);
            }
        }
        (Outcome::StillAttempted, max_cycles)
    }

    fn published() -> Settled {
        Settled { outcome: Outcome::Published, extra_self_copies: vec![], invalid_seen: false }
    }

    #[tokio::test]
    async fn upload_that_landed_before_the_crash_is_published_without_a_resend() {
        for create_only in [true, false] {
            let store = FakeStore::new(create_only, 0);
            store.landed(OURS, 0);
            assert_eq!(cycle(&store).await, (published(), vec![]));
            assert_eq!((store.puts(), store.count(OURS)), (0, 1));
        }
    }

    #[tokio::test]
    async fn upload_that_never_arrived_is_resent_once_with_the_same_bytes() {
        for create_only in [true, false] {
            let store = FakeStore::new(create_only, 0);
            assert_eq!(cycle(&store).await, (published(), RECHECK_DELAYS.to_vec()));
            assert_eq!((store.puts(), store.stored()), (1, vec![OURS.to_vec()]));
        }
    }

    /// AC 17 (W-1): on a create-only store a lagging listing never produces a
    /// second object, and the op settles exactly once.
    #[tokio::test]
    async fn a_lagging_listing_settles_once_without_a_duplicate() {
        for lag in 0..8 {
            let store = FakeStore::new(true, 0);
            store.landed(OURS, lag);
            assert_eq!(until_settled(&store, 4).await.0, Outcome::Published, "lag {lag}");
            assert_eq!(store.count(OURS), 1, "lag {lag}");
        }
    }

    #[tokio::test]
    async fn a_lost_resend_response_is_recovered_on_the_next_cycle() {
        let store = FakeStore::new(true, 1);
        *store.lost_put_responses.lock().unwrap() = 1;
        assert_eq!(cycle(&store).await.0.outcome, Outcome::StillAttempted);
        assert_eq!(until_settled(&store, 3).await, (Outcome::Published, 1));
        assert_eq!((store.puts(), store.count(OURS)), (1, 1));
    }

    #[tokio::test]
    async fn an_unknown_read_never_leads_to_a_resend() {
        // A failed probe ends the cycle with nothing written.
        for successful_probes in 0..3 {
            let store = FakeStore::new(true, 0);
            *store.gets.lock().unwrap() = (0..successful_probes).map(|_| true).chain([false]).collect();
            let (settled, pauses) = cycle(&store).await;
            assert_eq!(settled.outcome, Outcome::StillAttempted, "probe {successful_probes}");
            assert_eq!(pauses, RECHECK_DELAYS[..successful_probes].to_vec());
            assert_eq!(store.puts(), 0, "probe {successful_probes}: resent after an unknown read");
            assert_eq!(until_settled(&store, 2).await, (Outcome::Published, 1));
            assert_eq!((store.puts(), store.count(OURS)), (1, 1));
        }
        // The verifying read after a resend fails: unknown, settled next cycle.
        let store = FakeStore::new(true, 0);
        *store.gets.lock().unwrap() = [true, true, true, false].into();
        assert_eq!(cycle(&store).await.0.outcome, Outcome::StillAttempted);
        assert_eq!(until_settled(&store, 2).await, (Outcome::Published, 1));
        assert_eq!((store.puts(), store.count(OURS)), (1, 1));
    }

    #[tokio::test]
    async fn on_a_create_only_store_another_writers_object_rejects_ours() {
        let store = FakeStore::new(true, 0);
        store.landed(THEIRS, 0);
        assert_eq!(cycle(&store).await.0.outcome, Outcome::Rejected);
        assert_eq!(store.puts(), 0);

        let late = FakeStore::new(true, 0);
        late.landed(THEIRS, 3);
        assert_eq!(cycle(&late).await.0.outcome, Outcome::Rejected);
        assert_eq!(late.stored(), vec![THEIRS.to_vec()], "never overwritten, ours never stored");

        let corrupt = FakeStore::new(true, 0);
        corrupt.landed(GARBAGE, 0);
        let settled = cycle(&corrupt).await.0;
        assert_eq!((settled.outcome, settled.invalid_seen), (Outcome::Rejected, true));
    }

    /// B1: on Drive another writer's object proves nothing about ours, so it is
    /// never a rejection; our bytes are published next to it as a fork.
    #[tokio::test]
    async fn on_drive_a_foreign_object_is_a_fork_never_a_rejection() {
        for our_lag in [None, Some(0), Some(1), Some(3), Some(6)] {
            let store = FakeStore::new(false, 0);
            store.landed(THEIRS, 0);
            if let Some(lag) = our_lag {
                store.landed(OURS, lag);
            }
            let mut outcomes = Vec::new();
            for _ in 0..3 {
                let settled = cycle(&store).await.0;
                outcomes.push(settled.outcome);
                if settled.outcome != Outcome::StillAttempted {
                    break;
                }
            }
            assert!(!outcomes.contains(&Outcome::Rejected), "{our_lag:?}: {outcomes:?}");
            assert_eq!(outcomes.last(), Some(&Outcome::PublishedWithFork), "{our_lag:?}");
            assert_eq!(store.count(THEIRS), 1);
            assert!(store.count(OURS) >= 1);
        }

        // An unreadable object is quarantined, never a fork: ours is resent
        // and published, and the caller learns about the garbage.
        let corrupt = FakeStore::new(false, 0);
        corrupt.landed(GARBAGE, 0);
        let settled = cycle(&corrupt).await.0;
        assert_eq!((settled.outcome, settled.invalid_seen), (Outcome::Published, true));
        assert_eq!((corrupt.count(OURS), corrupt.count(GARBAGE)), (1, 1));

        let next_to_ours = FakeStore::new(false, 0);
        next_to_ours.landed(GARBAGE, 0);
        next_to_ours.landed(OURS, 0);
        let settled = cycle(&next_to_ours).await.0;
        assert_eq!((settled.outcome, settled.invalid_seen, next_to_ours.puts()), (Outcome::Published, true, 0));
    }

    /// Removes unreadable objects while the resolver waits between probes.
    struct GarbageCollectingPause<'a>(&'a FakeStore);

    impl Pause for GarbageCollectingPause<'_> {
        async fn pause(&self, _: Duration) {
            self.0.objects.lock().unwrap().retain(|object| object.bytes != GARBAGE);
        }
    }

    /// An unreadable object seen by an earlier probe is still reported when
    /// the deciding listing no longer shows it.
    #[tokio::test]
    async fn an_invalid_object_seen_earlier_in_the_cycle_is_still_reported() {
        let store = FakeStore::new(false, 0);
        store.landed(GARBAGE, 0);
        store.landed(OURS, 1);
        let settled = settle_attempted(&store, &GarbageCollectingPause(&store), KEY, OURS, judge).await;
        assert_eq!(settled, Settled { outcome: Outcome::Published, extra_self_copies: vec![], invalid_seen: true });
        assert_eq!((store.count(GARBAGE), store.puts()), (0, 0));
    }

    /// B2: when a slow Drive listing makes us resend, the second identical copy
    /// is reported once both are visible, and removing it leaves one object.
    #[tokio::test]
    async fn a_drive_self_duplicate_is_reported_and_removable() {
        let store = FakeStore::new(false, 0);
        store.landed(OURS, 4);
        assert_eq!(cycle(&store).await.0, published());
        assert_eq!(store.count(OURS), 2, "Drive kept both");

        // The next listing shows both copies; the extra one is named for removal.
        let settled = cycle(&store).await.0;
        assert_eq!(settled.outcome, Outcome::Published);
        let extras = settled.extra_self_copies;
        assert_eq!(extras.len(), 1);
        let listing = store.get_all(KEY).await.unwrap();
        assert_eq!(extra_self_copies(&listing, judge), extras);
        for provider_ref in &extras {
            store.delete(provider_ref);
        }
        assert_eq!(store.count(OURS), 1);
        assert_eq!(cycle(&store).await.0, published());
    }

    #[test]
    fn the_resolver_future_can_move_between_threads() {
        fn assert_send<T: Send>(_: &T) {}
        let store = FakeStore::new(true, 0);
        let pause = RecordedPause::default();
        let future = settle_attempted(&store, &pause, KEY, OURS, judge);
        assert_send(&future);
    }
}
