//! The retry of a create-only write to `cas/l/`: every failure the store may get over (a
//! connection that turns out to be dead, a 503 of either kind, a 500, a stalled body) is tried
//! again after the retry policy's waits, until the write's budget would pass before the next
//! attempt; then the write fails with the store's last answer, saying how many attempts were
//! made and over how long. Nothing else is repeated, and the waits are counted with the packs'
//! when the retry is told to. The waits are taken by a fake [`RetryClock`] that records each wait
//! and moves its time on by it, never by the real timer.

use kbf_front::action_tree::{DEFAULT_TOTAL, FIRST_WAIT, LONGEST_WAIT};
use kbf_front::start_retry::RetryClock;
use kbf_front::store_metrics::Op;
use kbf_front::store_retry::{Cause, RetryPolicy};

use super::*;

/// A [`RetryClock`] whose time moves only by the waits taken on it, which it records; each wait
/// returns at once.
#[derive(Default)]
struct Sleeps {
    now: Mutex<Duration>,
    taken: Mutex<Vec<Duration>>,
}

impl RetryClock for Sleeps {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) += wait;
        self.taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(wait);
        Box::pin(std::future::ready(()))
    }
}

impl Sleeps {
    fn waits(&self) -> Vec<Duration> {
        self.taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The default policy with no jitter, so that the waits are the nominal ones, and `budget`.
fn policy(budget: Duration) -> RetryPolicy {
    RetryPolicy {
        jitter_percent: 0,
        ..RetryPolicy::DEFAULT
    }
    .with_budget(budget)
}

/// A rig whose writer has `budget` for a write, and the waits it makes.
async fn rig(budget: Duration) -> (Rig, Arc<Sleeps>) {
    let sleeps = Arc::new(Sleeps::default());
    let retry = PutRetry::with_clock(policy(budget), Arc::clone(&sleeps) as Arc<dyn RetryClock>);
    (Rig::with_memo_retry(None, Some(retry)).await, sleeps)
}

fn dead_connection() -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(std::io::Error::other(
        "connection closed before message completed",
    )))
}

fn unavailable() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 503,
        code: "ServiceUnavailable".to_owned(),
        message: "retry later".to_owned(),
    }
}

/// The store's back-pressure, as tierstore answers it.
fn slow_down() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 503,
        code: "SlowDown".to_owned(),
        message: "The server is taking as many uploads as it can; retry after a moment.".to_owned(),
    }
}

/// The input root of one blob only a storage node holds, and the key it is written to.
async fn unlocated(rig: &Rig) -> (Digest, String, Bytes) {
    let bytes = Bytes::from_static(b"a blob only the write tier holds");
    let blob = rig.farm.cas(bytes.clone()).await;
    let root = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    (root, whole_key(&blob.hash_hex()), bytes)
}

fn script(rig: &Rig, key: &str, answers: Vec<ObjectStoreError>) {
    rig.farm
        .state()
        .faults
        .script
        .insert(key.to_owned(), answers.into());
}

/// The input root of one packed file, and the key and the bytes of the manifest written for it
/// (the root directory's manifest, named by its digest).
async fn packed_root(rig: &Rig) -> (Digest, String, String, Bytes) {
    let packed = rig.farm.packed("packed", "cas/f/w/p0", 0).await;
    let root = rig
        .store(&directory(vec![file("p", &packed, false)], vec![], vec![]))
        .await;
    let manifest = format!(
        r#"{{"v":1,"children":[{{"name":"p","kind":"file","sha256":"{}","size":6,"exec":false,"source":{{"key":"cas/f/w/p0","offset":0}}}}]}}"#,
        packed.hash_hex()
    );
    let hex = TreeDigest::of(manifest.as_bytes()).to_string();
    (root, whole_key(&hex), hex, Bytes::from(manifest))
}

/// A 500, as a store that failed the request answers it.
fn internal_error() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 500,
        code: "InternalError".to_owned(),
        message: "try again".to_owned(),
    }
}

/// tierstore's answer to a PUT of whose body no byte came for its idle timeout.
fn stalled() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 400,
        code: "RequestTimeout".to_owned(),
        message: "No byte of the request body arrived within 30s.".to_owned(),
    }
}

/// Catches: a stale pooled connection failing the tree, or being retried at once with no wait:
/// the write of a blob whose first attempt dies is made again after the policy's first wait,
/// reading the blob again, and stores the blob once.
#[tokio::test]
async fn a_dead_connection_is_retried_after_the_first_wait_and_the_blob_is_stored_once() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(&rig, &key, vec![dead_connection()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 2);
    assert_eq!(rig.farm.state().reads.len(), 2, "the blob was read again");
    assert_eq!(sleeps.waits(), [FIRST_WAIT]);
}

/// Catches: a manifest write failing the tree on a dead connection or a 503: its bytes are held,
/// so the same write is made again, after the first wait and then the doubled one.
#[tokio::test]
async fn a_manifest_write_is_repeated_too() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, hex, _) = packed_root(&rig).await;
    script(&rig, &key, vec![dead_connection(), unavailable()]);
    let tree = rig.write(&root).await.expect("a tree");
    rig.farm.flatten(&tree).expect("every manifest is stored");
    assert_eq!(rig.farm.puts_of(&hex), 3);
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2]);
}

/// Catches: a dead connection that is retried only once, or retried without end: a connection
/// that keeps dying is retried within the 5 s budget (waits of 0.5, 1 and 2 s, the fourth of 4 s
/// not fitting) and the write fails on the fourth attempt with the store's message exactly,
/// followed by the attempts, the time they took and the budget.
#[tokio::test]
async fn a_connection_that_keeps_dying_fails_the_tree_saying_the_attempts_and_the_budget() {
    let (rig, sleeps) = rig(Duration::from_secs(5)).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| dead_connection()).collect());
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "request to the store failed: connection closed before message completed \
                  (still failing after 4 attempts over 3.5 s, the budget being 5 s)"
                .to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 4);
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2, FIRST_WAIT * 4]);
}

/// Catches: a 503 failing the tree at once: it is waited out with doubling waits and the blob
/// is stored by the fourth attempt.
#[tokio::test]
async fn a_503_is_waited_out_with_doubling_waits() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(
        &rig,
        &key,
        vec![unavailable(), unavailable(), unavailable()],
    );
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 4);
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2, FIRST_WAIT * 4]);
}

/// Catches: a 500 of a tree's write failing the tree as a store answer that will not pass: it
/// is waited out like a 503, once, and the blob is stored by the second attempt.
#[tokio::test]
async fn a_500_of_a_tree_write_is_retried() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(&rig, &key, vec![internal_error()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 2);
    assert_eq!(sleeps.waits(), [FIRST_WAIT]);
}

/// Catches: a 503 retried for longer than the budget, or a wait cut short to end at it: the
/// waits stop when the next one (4 s) would end past the 5 s budget, and the write fails with
/// the store's answer exactly, saying how many times it was made, over how long, and the budget.
#[tokio::test]
async fn a_503_past_the_budget_fails_with_the_stores_answer() {
    let (rig, sleeps) = rig(Duration::from_secs(5)).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| unavailable()).collect());
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 503 ServiceUnavailable: retry later (still failing after 4 \
                  attempts over 3.5 s, the budget being 5 s)"
                .to_owned()
        }
    );
    assert_eq!(failed.why(), "store");
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2, FIRST_WAIT * 4]);
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 4);
    assert_eq!(rig.farm.object(&key), None);
}

/// Catches: a tree write that keeps failing hanging or failing without saying why: a manifest
/// whose write the store answers 503 on every attempt ends the tree with [`NoTree::Put`] naming
/// the manifest's key, the store's answer, the attempts made, the time they took and the budget.
#[tokio::test]
async fn a_tree_write_that_keeps_failing_makes_no_tree_saying_the_attempts_and_the_budget() {
    let (rig, sleeps) = rig(Duration::from_secs(5)).await;
    let (root, key, hex, _) = packed_root(&rig).await;
    script(&rig, &key, (0..50).map(|_| unavailable()).collect());
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key,
            why: "store answered 503 ServiceUnavailable: retry later (still failing after 4 \
                  attempts over 3.5 s, the budget being 5 s)"
                .to_owned()
        }
    );
    assert_eq!(failed.why(), "store");
    assert_eq!(rig.farm.puts_of(&hex), 4);
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2, FIRST_WAIT * 4]);
}

/// Catches: waits that grow without bound, or a budget overrun: the waits double to
/// [`LONGEST_WAIT`] and stay there, and the retries stop when the next wait would end past the 60 s
/// budget: four waits to 7.5 s, then six of 8 s (55.5 s), the seventh (63.5 s) not made.
#[tokio::test]
async fn the_waits_stop_doubling_at_the_longest_and_end_at_the_budget() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| unavailable()).collect());
    rig.write(&root).await.expect_err("no tree");
    let waits = sleeps.waits();
    assert_eq!(waits.len(), 10);
    assert_eq!(waits.iter().max(), Some(&LONGEST_WAIT));
    assert_eq!(waits[4], LONGEST_WAIT);
    assert!(waits[4..].iter().all(|wait| *wait == LONGEST_WAIT));
    assert_eq!(
        waits.iter().sum::<Duration>(),
        Duration::from_millis(55_500)
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 11);
}

/// Catches: a budget of zero still waiting: a 503 is the failure on the first attempt, with the
/// store's answer as it is.
#[tokio::test]
async fn a_zero_budget_never_retries_a_503() {
    let (rig, sleeps) = rig(Duration::ZERO).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, vec![unavailable()]);
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 503 ServiceUnavailable: retry later".to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 1);
    assert!(sleeps.waits().is_empty());
}

/// Catches: a 400 other than a stalled body's `RequestTimeout` repeated: a `BadDigest` is the
/// failure on the first attempt, naming the store's answer.
#[tokio::test]
async fn a_400_other_than_request_timeout_is_not_repeated() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, _) = unlocated(&rig).await;
    let bad = ObjectStoreError::Service {
        status: 400,
        code: "BadDigest".to_owned(),
        message: "no".to_owned(),
    };
    script(&rig, &key, vec![bad]);
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 400 BadDigest: no".to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 1);
    assert!(sleeps.waits().is_empty());
}

/// Catches: an answer that the retry does not take (not a connection failure, a stalled body,
/// a 503 or another 5xx) repeated: a 403, and a body whose bytes are not its digest's, are the
/// failure on the first attempt.
#[tokio::test]
async fn other_failures_are_not_repeated() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, _) = unlocated(&rig).await;
    let denied = ObjectStoreError::Service {
        status: 403,
        code: "AccessDenied".to_owned(),
        message: "no".to_owned(),
    };
    script(&rig, &key, vec![denied]);
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 403 AccessDenied: no".to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 1);

    let bytes = Bytes::from_static(b"the bytes the node holds");
    let blob = rig.farm.cas(bytes).await;
    rig.farm
        .state()
        .wrong
        .insert(blob, Bytes::from_static(b"other bytes of the nodes"));
    let root = root_of_one(&rig, &blob).await;
    rig.farm.state().reads.clear();
    let failed = rig.write(&root).await.expect_err("no tree");
    assert!(matches!(failed, NoTree::Mismatch { .. }), "{failed:?}");
    assert_eq!(rig.farm.state().reads.len(), 1);
    assert!(sleeps.waits().is_empty());
}

async fn root_of_one(rig: &Rig, blob: &Digest) -> Digest {
    rig.store(&directory(vec![file("f", blob, false)], vec![], vec![]))
        .await
}

/// Catches: a write that did store its object before the connection died counted as a failure:
/// the second attempt finds the key taken, which is success.
#[tokio::test]
async fn a_key_found_taken_after_a_dead_connection_is_written() {
    let (rig, _) = rig(DEFAULT_TOTAL).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, vec![dead_connection()]);
    rig.farm.state().late.insert(key);
    rig.write(&root).await.expect("a tree");
}

/// Catches: a writer with no retry configured not repeating at all: the default is the
/// default policy with the 60 s budget, which is also what the type reports.
#[test]
fn the_default_budget_is_sixty_seconds() {
    assert_eq!(
        PutRetry::default().policy(),
        RetryPolicy::DEFAULT.with_budget(Duration::from_secs(60))
    );
    assert_eq!(DEFAULT_TOTAL, Duration::from_secs(60));
}

/// Catches: a retry that never waits on the real timer: a writer made with [`PutRetry::new`]
/// waits out a 503 on tokio's clock (paused here, so the wait is not taken) and stores the blob.
#[tokio::test(start_paused = true)]
async fn the_timer_of_a_running_frontend_waits_out_a_503() {
    let retry = PutRetry::new(RetryPolicy::DEFAULT.with_budget(DEFAULT_TOTAL));
    let rig = Rig::with_memo_retry(None, Some(retry)).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(&rig, &key, vec![unavailable()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 2);
}

/// Catches: the store's back-pressure retried without end, or taken as no reason to wait: a
/// blob answered `SlowDown` twice is waited out with doubling waits and stored by the third
/// attempt.
#[tokio::test]
async fn a_slow_down_is_waited_out_within_the_budget() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(&rig, &key, vec![slow_down(), slow_down()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 3);
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2]);
}

/// Catches: a `SlowDown` waited out past the budget, as it was before it counted toward it:
/// fifty `SlowDown`s under a budget of 5 s are waited out only while the next wait fits (0.5, 1
/// and 2 s), and the write fails with the store's `SlowDown` and the attempts made.
#[tokio::test]
async fn a_slow_down_counts_toward_the_budget_and_ends_the_write() {
    let (rig, sleeps) = rig(Duration::from_secs(5)).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| slow_down()).collect());
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 503 SlowDown: The server is taking as many uploads as it can; \
                  retry after a moment. (still failing after 4 attempts over 3.5 s, the budget \
                  being 5 s)"
                .to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 4);
    assert_eq!(sleeps.waits(), [FIRST_WAIT, FIRST_WAIT * 2, FIRST_WAIT * 4]);
}

/// Catches: a manifest's write that gives up on a `SlowDown` under a budget of zero and then
/// succeeds anyway: with no budget, nothing is waited out, and the manifest's `SlowDown` is
/// the failure as the store gave it.
#[tokio::test]
async fn a_slow_down_is_not_waited_out_with_a_zero_budget() {
    let (rig, sleeps) = rig(Duration::ZERO).await;
    let (root, key, hex, _) = packed_root(&rig).await;
    script(&rig, &key, vec![slow_down()]);
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key,
            why: "store answered 503 SlowDown: The server is taking as many uploads as it can; \
                  retry after a moment."
                .to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&hex), 1);
    assert!(sleeps.waits().is_empty());
}

/// Catches: a manifest's write failing the tree on a stalled body (400 `RequestTimeout`), or
/// taking the object of 0 bytes the store kept of the stalled write for the manifest: the write
/// is retried after the first wait, finds the key taken by an object of another length, deletes
/// it and writes the manifest again; the tree is made and the manifest reads back whole.
#[tokio::test]
async fn a_manifest_s_stalled_body_is_retried_and_the_object_it_left_is_replaced() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, hex, manifest) = packed_root(&rig).await;
    rig.farm
        .state()
        .faults
        .keeps
        .insert(key.clone(), VecDeque::from([stalled()]));
    let tree = rig.write(&root).await.expect("a tree");
    rig.farm.flatten(&tree).expect("every manifest is stored");
    assert_eq!(rig.farm.object(&key), Some(manifest));
    assert_eq!(rig.farm.puts_of(&hex), 3);
    assert_eq!(rig.farm.state().deletes, [key]);
    assert_eq!(sleeps.waits(), [FIRST_WAIT]);
}

/// Catches: a manifest write that finds its key taken and trusts whatever is there: an object
/// of another length under the manifest's key (one of 0 bytes, one cut short) is deleted and the
/// manifest's bytes are written, and the key reads back as the manifest, after one more write.
#[tokio::test]
async fn a_manifest_write_that_finds_an_object_of_another_length_replaces_it() {
    for left in [Bytes::new(), Bytes::from_static(b"{\"v\":1,")] {
        let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
        let (root, key, hex, manifest) = packed_root(&rig).await;
        rig.farm.state().objects.insert(key.clone(), left.clone());
        let tree = rig.write(&root).await.expect("a tree");
        rig.farm.flatten(&tree).expect("every manifest is stored");
        assert_eq!(rig.farm.object(&key), Some(manifest), "{left:?}");
        assert_eq!(rig.farm.puts_of(&hex), 2, "{left:?}");
        assert_eq!(rig.farm.state().deletes, [key], "{left:?}");
        assert!(sleeps.waits().is_empty());
    }
}

/// Catches: a manifest write that finds its own bytes already stored (a write of another tree,
/// or one whose answer was lost) deleting them or writing them again: an object of the
/// manifest's length under the key is success, after the one write that found it taken.
#[tokio::test]
async fn a_manifest_write_that_finds_its_own_object_keeps_it() {
    let (rig, _) = rig(DEFAULT_TOTAL).await;
    let (root, key, hex, manifest) = packed_root(&rig).await;
    rig.farm
        .state()
        .objects
        .insert(key.clone(), manifest.clone());
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(manifest));
    assert_eq!(rig.farm.puts_of(&hex), 1);
    assert!(rig.farm.state().deletes.is_empty());
}

/// Catches a tree's waits for the store that the scrape never shows: with the retry counted in
/// the packs' waits, a blob answered 503 three times records its three waits there, as writes,
/// adding up to the waits taken, with their cause, and none as reads.
#[tokio::test]
async fn a_tree_s_waits_are_counted_with_the_packs() {
    let sleeps = Arc::new(Sleeps::default());
    let waits = Arc::new(kbf_front::store_wait::StoreWaits::default());
    let retry = PutRetry::with_clock(
        policy(DEFAULT_TOTAL),
        Arc::clone(&sleeps) as Arc<dyn RetryClock>,
    )
    .counted_in(Arc::clone(&waits));
    let rig = Rig::with_memo_retry(None, Some(retry)).await;
    let (root, key, _) = unlocated(&rig).await;
    script(
        &rig,
        &key,
        vec![unavailable(), unavailable(), unavailable()],
    );

    rig.write(&root).await.expect("a tree");

    let put = waits.waits(Op::Put);
    assert_eq!(put.count(), 3);
    assert_eq!(put.sum(), 3_500_000_000);
    assert_eq!(sleeps.waits().len(), 3);
    assert_eq!(waits.retries(Op::Put, Cause::Unavailable), 3);
    assert_eq!(waits.retries(Op::Put, Cause::SlowDown), 0);
    assert_eq!(waits.waits(Op::Get).count(), 0);
    assert_eq!(waits.exhausted(Op::Put), 0);
    assert!(!waits.down());
}

/// Catches: a write that spent its budget not counted, or a store that stays down costing each
/// write its whole budget again: with the retry counted in the packs' waits, a blob answered
/// 503 on every attempt ends its write exhausted and takes the store as down, and the next write
/// of the blob is answered at once, with no wait, the message saying it was not retried.
#[tokio::test]
async fn a_write_that_spends_its_budget_takes_the_store_as_down_for_the_next() {
    let sleeps = Arc::new(Sleeps::default());
    let waits = Arc::new(kbf_front::store_wait::StoreWaits::default());
    let retry = PutRetry::with_clock(
        policy(Duration::from_secs(5)),
        Arc::clone(&sleeps) as Arc<dyn RetryClock>,
    )
    .counted_in(Arc::clone(&waits));
    let rig = Rig::with_memo_retry(None, Some(retry)).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| unavailable()).collect());

    rig.write(&root).await.expect_err("no tree");
    assert_eq!(waits.exhausted(Op::Put), 1);
    assert!(waits.down());
    assert_eq!(sleeps.waits().len(), 3);

    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 503 ServiceUnavailable: retry later (not retried: a call to the \
                  store spent its budget of 5 s still failing, and no call since was answered)"
                .to_owned()
        }
    );
    assert_eq!(waits.exhausted(Op::Put), 2);
    assert_eq!(sleeps.waits().len(), 3, "the second write waited");
}
