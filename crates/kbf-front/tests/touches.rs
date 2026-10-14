//! Touches off the reply path: a read records the touch it owes and answers without a
//! metadata commit; recorded touches are committed in batches; a collection commits
//! them first, so nothing a read reported present is collected.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{Cache, MemoryMetaLog, MetaLog, MetaLogError, TouchConfig, VerifiedBlob};
use kbf_meta::{Applied, Command, MetaState, Retention, Role, Touch};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_proto::reapi::ActionResult;
use kbf_types::{Digest, FarmTime};
use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Farm time `n` days after the epoch.
fn days(n: u32) -> FarmTime {
    FarmTime::from_millis(0).saturating_add(DAY.saturating_mul(n))
}

/// Which commands [`HeldLog`] can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Touch,
    Collect,
    Other,
}

fn kind(command: &Command) -> Kind {
    match command {
        Command::Touch(_) => Kind::Touch,
        Command::Collect => Kind::Collect,
        _ => Kind::Other,
    }
}

/// A [`MemoryMetaLog`] that keeps every command it is asked to commit (when it is
/// asked, before it applies), can fail the next touch commits, and can hold commits of
/// one kind until released: a commit stalled in its sync.
#[derive(Debug)]
struct HeldLog {
    inner: MemoryMetaLog,
    asked: Mutex<Vec<Command>>,
    held: Mutex<Option<Kind>>,
    fail_touches: Mutex<usize>,
    entered: Notify,
    release: Semaphore,
}

impl HeldLog {
    fn new() -> Self {
        Self {
            inner: MemoryMetaLog::new(Retention::default()),
            asked: Mutex::new(Vec::new()),
            held: Mutex::new(None),
            fail_touches: Mutex::new(0),
            entered: Notify::new(),
            release: Semaphore::new(0),
        }
    }

    fn hold(&self, kind: Kind) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(kind);
    }

    fn release_all(&self) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.release.add_permits(Semaphore::MAX_PERMITS / 2);
    }

    fn fail_next_touches(&self, n: usize) {
        *self
            .fail_touches
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = n;
    }

    /// The touch commits asked for so far, each as the blobs it names.
    fn touches(&self) -> Vec<BTreeSet<Digest>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|c| match c {
                Command::Touch(t) => Some(t.blobs.clone()),
                _ => None,
            })
            .collect()
    }

    /// How many `Collect` commits have been asked for so far.
    fn collects(&self) -> usize {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|c| kind(c) == Kind::Collect)
            .count()
    }

    /// The touch commits asked for so far, blobs and actions.
    fn touch_commands(&self) -> Vec<Touch> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|c| match c {
                Command::Touch(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }
}

impl MetaLog for HeldLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        let k = kind(&command);
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command.clone());
        if k == Kind::Touch {
            let mut fail = self
                .fail_touches
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *fail > 0 {
                *fail -= 1;
                // The first refusal is an error, the second another command's outcome.
                return if *fail % 2 == 1 {
                    Err(MetaLogError::Unavailable("planted".to_owned()))
                } else {
                    Ok(Applied::Ticked(days(0)))
                };
            }
        }
        let held = *self.held.lock().unwrap_or_else(PoisonError::into_inner) == Some(k);
        if held {
            self.entered.notify_one();
            self.release.acquire().await.expect("open").forget();
        }
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.inner.query(f).await
    }
}

type TestCache = Cache<HeldLog, MemoryStore>;

async fn cache() -> TestCache {
    Cache::open(
        HeldLog::new(),
        MemoryStore::new(Capabilities::default()),
        KeyPrefix::default(),
    )
    .await
    .expect("open the cache")
}

async fn store(cache: &TestCache, text: &str) -> Digest {
    let blob = VerifiedBlob::hashed(Bytes::from(text.to_owned()));
    let digest = blob.digest();
    cache.store_blobs(vec![blob]).await.expect("store");
    digest
}

/// Writes an action-cache entry for a new action, with an empty result, as a daemon.
async fn write_action(cache: &TestCache, name: &str) -> Digest {
    let action = VerifiedBlob::hashed(Bytes::from(name.to_owned())).digest();
    let result = ActionResult {
        exit_code: 0,
        ..Default::default()
    };
    cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("write the action result");
    action
}

fn config(every: Duration, batch: usize) -> TouchConfig {
    TouchConfig {
        every,
        batch: NonZeroUsize::new(batch).expect("non-zero"),
    }
}

/// `future`'s output, or a failure after 10 s: a test whose commit stays held must
/// fail, not hang.
async fn within<F: Future>(future: F) -> F::Output {
    timeout(Duration::from_secs(10), future)
        .await
        .expect("done within 10 s")
}

/// Polls until no touch is pending, for at most 10 s.
async fn flushed(cache: &TestCache) {
    timeout(Duration::from_secs(10), async {
        while cache.touches_pending() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the recorded touches are committed within 10 s");
}

/// Catches (red before this change; mutant "commit the touch on the reply path"):
/// FindMissingBlobs waiting for a metadata commit before it answers. The blobs are two
/// days old, so the report owes a touch, and every touch commit is held as a commit
/// stalled in its disk sync would be. The answer must come within 5 s regardless, and
/// no touch commit may have been asked for by then.
#[tokio::test]
async fn find_missing_answers_while_a_touch_commit_is_held() {
    let cache = cache().await;
    let log = cache.meta();
    let present = store(&cache, "present for two days").await;
    let absent = VerifiedBlob::hashed(Bytes::from_static(b"never stored")).digest();
    cache.tick(days(2)).await.expect("tick");
    log.hold(Kind::Touch);

    let answer = timeout(
        Duration::from_secs(5),
        cache.find_missing(&[present, absent]),
    )
    .await;
    let touches_at_answer = log.touches().len();
    log.release_all();

    let missing = answer
        .expect("FindMissingBlobs answered while a touch commit was held")
        .expect("find");
    assert_eq!(missing, [absent]);
    assert_eq!(
        touches_at_answer, 0,
        "a touch was committed on the reply path"
    );
}

/// Catches (mutants "a recorded touch is dropped", "a digest recorded twice is
/// committed twice", "a batch larger than the bound"): five blobs reported twice each
/// are committed as touches exactly once each, in commits of at most two, without a
/// collection or another read forcing them; and the touches keep the blobs past the
/// collection that takes their unreported neighbour.
#[tokio::test]
async fn recorded_touches_are_committed_once_each_in_bounded_batches() {
    let cache = cache()
        .await
        .with_touches(config(Duration::from_millis(20), 2));
    let log = cache.meta();
    let mut reported = Vec::new();
    for i in 0..5 {
        reported.push(store(&cache, &format!("reported {i}")).await);
    }
    let unreported = store(&cache, "never reported").await;
    cache.tick(days(5)).await.expect("tick");
    for _ in 0..2 {
        assert!(
            cache
                .find_missing(&reported)
                .await
                .expect("find")
                .is_empty()
        );
    }
    flushed(&cache).await;

    let touches = log.touches();
    assert!(
        touches.iter().all(|t| !t.is_empty() && t.len() <= 2),
        "{touches:?}"
    );
    let mut each: Vec<Digest> = touches.iter().flatten().copied().collect();
    each.sort();
    let mut want = reported.clone();
    want.sort();
    assert_eq!(each, want, "every reported blob touched exactly once");

    // Stored on day 0: untouched, they expire after day 8; touched on day 5, after 13.
    cache.tick(days(9)).await.expect("tick");
    cache.collect().await.expect("collect");
    let mut ask = reported.clone();
    ask.push(unreported);
    assert_eq!(cache.find_missing(&ask).await.expect("find"), [unreported]);
}

/// Catches (mutant "a batch whose commit failed is not put back"): a touch whose
/// commit the log refused being lost. The first touch commit fails with an error, the
/// second is answered with another command's outcome; the touch is committed by a
/// later flush all the same.
#[tokio::test]
async fn a_touch_whose_commit_fails_is_committed_later() {
    let cache = cache()
        .await
        .with_touches(config(Duration::from_millis(20), 100));
    let log = cache.meta();
    let blob = store(&cache, "reported while the log refuses touches").await;
    cache.tick(days(5)).await.expect("tick");
    log.fail_next_touches(2);
    assert!(cache.find_missing(&[blob]).await.expect("find").is_empty());
    flushed(&cache).await;

    let touches = log.touches();
    assert_eq!(touches.len(), 3, "two refused, one applied: {touches:?}");
    assert!(touches.iter().all(|t| t == &BTreeSet::from([blob])));
}

/// Catches (mutant "collect without committing pending touches"): a collection that
/// removes a blob reported present whose touch is still pending, so a client that
/// skipped its upload finds it gone. The flusher waits an hour, so the touch is only
/// recorded when the collection runs; the blob must survive it, and its unreported
/// neighbour must not.
#[tokio::test]
async fn a_collection_keeps_a_blob_whose_touch_is_pending() {
    let cache = cache()
        .await
        .with_touches(config(Duration::from_secs(3600), 100));
    let log = cache.meta();
    let reported = store(&cache, "reported on day 5").await;
    let unreported = store(&cache, "never reported").await;
    cache.tick(days(5)).await.expect("tick");
    assert!(
        cache
            .find_missing(&[reported])
            .await
            .expect("find")
            .is_empty()
    );
    assert_eq!(cache.touches_pending(), 1);
    assert!(log.touches().is_empty(), "the flusher ran early");

    cache.tick(days(9)).await.expect("tick");
    cache.collect().await.expect("collect");
    assert_eq!(cache.touches_pending(), 0);
    let missing = cache
        .find_missing(&[reported, unreported])
        .await
        .expect("find");
    assert_eq!(missing, [unreported]);
}

/// Catches (mutants "a collection goes ahead when its flush fails", "a failed flush
/// reports success"): a collection whose commit of the pending touches fails, yet
/// commits `Collect` anyway and removes a blob a read reported present while its touch
/// was only recorded. The first collection's touch commit gets an error and the
/// second's another command's outcome: neither may ask for `Collect`, and the touch
/// must stay pending. The third commits it, then removes only the unreported neighbour.
#[tokio::test]
async fn a_collection_whose_touch_commit_fails_collects_nothing() {
    let cache = cache()
        .await
        .with_touches(config(Duration::from_secs(3600), 100));
    let log = cache.meta();
    let reported = store(&cache, "reported on day 5, touch not yet committed").await;
    let unreported = store(&cache, "never reported").await;
    cache.tick(days(5)).await.expect("tick");
    assert!(
        cache
            .find_missing(&[reported])
            .await
            .expect("find")
            .is_empty()
    );
    // Untouched since day 0, both blobs have expired by day 9.
    cache.tick(days(9)).await.expect("tick");
    log.fail_next_touches(2);

    for refusal in ["an error", "another command's outcome"] {
        assert!(
            cache.collect().await.is_err(),
            "a collection succeeded though its touch commit got {refusal}"
        );
        assert_eq!(log.collects(), 0, "Collect committed after {refusal}");
        assert_eq!(
            cache.touches_pending(),
            1,
            "the touch was lost on {refusal}"
        );
    }

    let collected = cache.collect().await.expect("collect");
    let removed: Vec<Digest> = collected.blobs.iter().map(|(d, _)| *d).collect();
    assert_eq!(removed, [unreported], "only the unreported blob expires");
    assert_eq!(log.touches(), vec![BTreeSet::from([reported]); 3]);
}

/// Catches (mutant "a read records its touch outside the gate"): a read that reports a
/// blob present while a collection is applying, after the collection committed what
/// was pending: its touch reaches the log after the `Collect` that removes the blob,
/// so a client is told "present" about a blob that is gone. The `Collect` commit is
/// held while the read runs; whatever the read answers must still hold once the
/// collection has applied.
#[tokio::test]
async fn a_read_during_a_collection_never_reports_what_it_removes() {
    let cache = Arc::new(
        cache()
            .await
            .with_touches(config(Duration::from_secs(3600), 100)),
    );
    let log = cache.meta();
    let blob = store(&cache, "expires at the collection").await;
    cache.tick(days(9)).await.expect("tick");
    log.hold(Kind::Collect);
    let collecting = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { cache.collect().await.map(|c| c.blobs.len()) }
    });
    within(log.entered.notified()).await;
    let reading = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { cache.find_missing(&[blob]).await }
    });
    // Long enough for a read that does not wait for the collection to answer.
    tokio::time::sleep(Duration::from_millis(200)).await;
    log.release_all();
    let collected = within(collecting).await.expect("join").expect("collect");
    let answer = within(reading).await.expect("join").expect("find");

    assert_eq!(collected, 1, "the blob had expired");
    assert_eq!(answer, [blob], "reported present, then collected");
}

/// Catches (mutant "the flusher commits without holding the gate"): a collection that
/// runs while the flusher's batch has left the pending set but is not committed yet.
/// Its own flush then finds nothing, its `Collect` lands before the touch, and a blob a
/// read reported present is removed. The flusher's touch commit is held; the blob
/// expires by farm time meanwhile; the collection is started and given time to run; then
/// the touch is released. The collection must wait for the touch and remove nothing.
#[tokio::test]
async fn a_collection_waits_for_the_touch_batch_being_committed() {
    let cache = Arc::new(
        cache()
            .await
            .with_touches(config(Duration::from_secs(3600), 1)),
    );
    let log = cache.meta();
    let blob = store(&cache, "reported while its touch is being committed").await;
    cache.tick(days(5)).await.expect("tick");
    log.hold(Kind::Touch);
    let found = within(cache.find_missing(&[blob])).await;
    assert!(found.expect("find").is_empty());
    // The batch of one is full: the flusher took it and its commit is now held.
    within(log.entered.notified()).await;
    assert_eq!(cache.touches_pending(), 1, "the batch is being committed");
    // Untouched since day 0, the blob has expired by day 9.
    log.inner
        .commit(Command::Tick(days(9)))
        .await
        .expect("tick");
    let collecting = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { cache.collect().await.map(|c| c.blobs.len()) }
    });
    // Long enough for a collection that does not wait for the flusher to apply.
    tokio::time::sleep(Duration::from_millis(200)).await;
    log.release_all();
    let collected = within(collecting).await.expect("join").expect("collect");

    assert_eq!(collected, 0, "a blob reported present was collected");
    assert!(
        cache.find_missing(&[blob]).await.expect("find").is_empty(),
        "the reported blob is gone"
    );
}

/// Catches (mutant "a recorded touch drops its actions"): an action-cache hit whose
/// touch is never committed, so a result used every day still expires 30 days after it
/// was written. Written on day 0, hit on day 5: the flusher must commit a touch naming
/// the action, and a collection on day 33 must keep it (untouched, it goes after day 30).
#[tokio::test]
async fn an_action_cache_hit_is_touched_and_kept_by_a_collection() {
    let cache = cache()
        .await
        .with_touches(config(Duration::from_millis(20), 100));
    let log = cache.meta();
    let action = write_action(&cache, "an action hit on day 5").await;
    let unused = write_action(&cache, "an action never hit").await;
    cache.tick(days(5)).await.expect("tick");
    assert!(
        cache
            .action_result(&action)
            .await
            .expect("lookup")
            .is_some(),
        "a hit"
    );
    flushed(&cache).await;
    let touched: BTreeSet<Digest> = log
        .touch_commands()
        .iter()
        .flat_map(|t| t.actions.iter().copied())
        .collect();
    assert_eq!(touched, BTreeSet::from([action]));

    cache.tick(days(33)).await.expect("tick");
    let collected = cache.collect().await.expect("collect");
    assert_eq!(
        collected.actions,
        [unused],
        "only the unused action expires"
    );
}

/// Catches (mutant "a batch's actions fill a whole batch of their own"): a batch that
/// mixes blobs and actions exceeding the bound, up to twice it. Two blobs and three
/// actions are recorded with a bound of two; outside a runtime no flusher runs, so the
/// collection's flush commits all five in batches, each of at most two entries, each
/// entry once.
#[test]
fn a_batch_of_blobs_and_actions_together_stays_within_the_bound() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (cache, blobs, actions) = runtime.block_on(async {
        let cache = cache()
            .await
            .with_touches(config(Duration::from_secs(3600), 2));
        let blobs = [
            store(&cache, "blob one").await,
            store(&cache, "blob two").await,
        ];
        let mut actions = BTreeSet::new();
        for i in 0..3 {
            actions.insert(write_action(&cache, &format!("action {i}")).await);
        }
        cache.tick(days(5)).await.expect("tick");
        (cache, blobs, actions)
    });
    futures::executor::block_on(async {
        assert!(cache.find_missing(&blobs).await.expect("find").is_empty());
        for action in &actions {
            assert!(cache.action_result(action).await.expect("lookup").is_some());
        }
        assert!(
            cache.meta().touch_commands().is_empty(),
            "nothing flushed yet"
        );
        cache.collect().await.expect("collect");
    });

    let touches = cache.meta().touch_commands();
    assert!(
        touches.iter().all(|t| t.blobs.len() + t.actions.len() <= 2),
        "{touches:?}"
    );
    let touched_actions: Vec<Digest> = touches.iter().flat_map(|t| t.actions.clone()).collect();
    assert_eq!(touched_actions, actions.into_iter().collect::<Vec<_>>());
    let touched_blobs: BTreeSet<Digest> = touches.iter().flat_map(|t| t.blobs.clone()).collect();
    assert!(
        touched_blobs.is_superset(&BTreeSet::from(blobs)),
        "{touches:?}"
    );
}

/// Catches (mutant "put a lost entry back"): a touch whose entry a collection outside
/// [`Cache::collect`] removed being retried for ever; it is logged and dropped.
#[tokio::test]
async fn a_touch_whose_entry_is_gone_is_dropped_not_retried() {
    let cache = cache()
        .await
        .with_touches(config(Duration::from_secs(3600), 1));
    let log = cache.meta();
    let blob = store(&cache, "collected behind the cache's back").await;
    cache.tick(days(5)).await.expect("tick");
    log.hold(Kind::Touch);
    let found = within(cache.find_missing(&[blob])).await;
    assert!(found.expect("find").is_empty());
    within(log.entered.notified()).await;
    log.inner
        .commit(Command::Tick(days(30)))
        .await
        .expect("tick");
    log.inner.commit(Command::Collect).await.expect("collect");
    log.release_all();
    flushed(&cache).await;
    assert_eq!(log.touches().len(), 1, "the lost touch was retried");
}

/// Catches: a read outside a Tokio runtime panicking where it would start the flusher.
/// Nothing flushes there, so the touch stays recorded until a collection commits it,
/// which keeps the reported blob.
#[test]
fn outside_a_runtime_a_collection_still_commits_the_recorded_touches() {
    // Stores spawn their segment writes, so they run on a runtime; the reads do not.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (cache, reported, unreported) = runtime.block_on(async {
        let cache = cache().await;
        let reported = store(&cache, "reported without a runtime").await;
        let unreported = store(&cache, "never reported").await;
        cache.tick(days(5)).await.expect("tick");
        (cache, reported, unreported)
    });
    futures::executor::block_on(async {
        assert!(
            cache
                .find_missing(&[reported])
                .await
                .expect("find")
                .is_empty()
        );
        assert_eq!(cache.touches_pending(), 1);
        cache.tick(days(9)).await.expect("tick");
        cache.collect().await.expect("collect");
        assert_eq!(cache.meta().touches().len(), 1);
        let missing = cache
            .find_missing(&[reported, unreported])
            .await
            .expect("find");
        assert_eq!(missing, [unreported]);
    });
}
