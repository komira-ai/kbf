//! The retry of a create-only write to `cas/l/`: a connection that turns out to be dead is
//! tried once more, a 503 is waited out within the configured total, and nothing else is
//! repeated. The waits are recorded by a fake [`Sleeper`], never taken.

use kbf_front::action_tree::{DEFAULT_TOTAL, FIRST_WAIT, LONGEST_WAIT, Sleeper};

use super::*;

/// A [`Sleeper`] that records each wait and returns at once.
#[derive(Default)]
struct Sleeps(Mutex<Vec<Duration>>);

impl Sleeper for Sleeps {
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(wait);
        Box::pin(std::future::ready(()))
    }
}

impl Sleeps {
    fn waits(&self) -> Vec<Duration> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A rig whose writer waits `total` for a 503, and the waits it makes.
async fn rig(total: Duration) -> (Rig, Arc<Sleeps>) {
    let sleeps = Arc::new(Sleeps::default());
    let retry = PutRetry::with_sleeper(total, Arc::clone(&sleeps) as Arc<dyn Sleeper>);
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
        code: "SlowDown".to_owned(),
        message: "retry later".to_owned(),
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

/// Catches: a stale pooled connection failing the tree: the write of a blob whose first
/// attempt dies is made again, reading the blob again, and stores the blob once, with no wait.
#[tokio::test]
async fn a_dead_connection_is_tried_once_more_and_the_blob_is_stored_once() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(&rig, &key, vec![dead_connection()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 2);
    assert_eq!(rig.farm.state().reads.len(), 2, "the blob was read again");
    assert_eq!(sleeps.waits(), Vec::<Duration>::new());
}

/// Catches: a manifest write failing the tree on a dead connection or a 503: its bytes are
/// held, so the same write is made again.
#[tokio::test]
async fn a_manifest_write_is_repeated_too() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let packed = rig.farm.packed("packed", "cas/f/w/p0", 0).await;
    let root = rig
        .store(&directory(vec![file("p", &packed, false)], vec![], vec![]))
        .await;
    rig.farm
        .state()
        .faults
        .any
        .extend([dead_connection(), unavailable()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(sleeps.waits(), [FIRST_WAIT]);
}

/// Catches: a second dead connection retried again: the write fails on it, with the store's
/// message exactly, after two attempts.
#[tokio::test]
async fn a_second_dead_connection_fails_the_tree() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, _) = unlocated(&rig).await;
    script(
        &rig,
        &key,
        vec![dead_connection(), dead_connection(), dead_connection()],
    );
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "request to the store failed: connection closed before message completed"
                .to_owned()
        }
    );
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 2);
    assert!(sleeps.waits().is_empty());
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

/// Catches: a 503 retried for longer than the total: the waits stop at the total (the last one
/// cut short to end there), and the write fails with the store's answer exactly.
#[tokio::test]
async fn a_503_past_the_total_fails_with_the_stores_answer() {
    let (rig, sleeps) = rig(Duration::from_secs(5)).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| unavailable()).collect());
    let failed = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        failed,
        NoTree::Put {
            key: key.clone(),
            why: "store answered 503 SlowDown: retry later".to_owned()
        }
    );
    assert_eq!(failed.why(), "store");
    let waits = sleeps.waits();
    assert_eq!(
        waits,
        [
            FIRST_WAIT,
            FIRST_WAIT * 2,
            FIRST_WAIT * 4,
            Duration::from_millis(1500)
        ]
    );
    assert_eq!(waits.iter().sum::<Duration>(), Duration::from_secs(5));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 5);
    assert_eq!(rig.farm.object(&key), None);
}

/// Catches: waits that grow without bound: past [`LONGEST_WAIT`] they stay there, and the last
/// is cut to the total.
#[tokio::test]
async fn the_waits_stop_doubling_at_the_longest() {
    let (rig, sleeps) = rig(DEFAULT_TOTAL).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, (0..50).map(|_| unavailable()).collect());
    rig.write(&root).await.expect_err("no tree");
    let waits = sleeps.waits();
    assert_eq!(waits.iter().sum::<Duration>(), DEFAULT_TOTAL);
    assert_eq!(waits.iter().max(), Some(&LONGEST_WAIT));
    assert_eq!(waits[4], LONGEST_WAIT);
    assert!(waits.last().is_some_and(|last| *last < LONGEST_WAIT));
}

/// Catches: a total of zero still waiting: a 503 is the failure on the first attempt.
#[tokio::test]
async fn a_zero_total_never_retries_a_503() {
    let (rig, sleeps) = rig(Duration::ZERO).await;
    let (root, key, _) = unlocated(&rig).await;
    script(&rig, &key, vec![unavailable()]);
    rig.write(&root).await.expect_err("no tree");
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 1);
    assert!(sleeps.waits().is_empty());
}

/// Catches: an answer that is not a connection failure or a 503 repeated: a 403, and a body
/// whose bytes are not its digest's, are the failure on the first attempt.
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

/// Catches: a writer with no retry configured not repeating at all: the default is the 60 s
/// total, which is also what the type reports.
#[test]
fn the_default_total_is_sixty_seconds() {
    assert_eq!(PutRetry::default().total(), Duration::from_secs(60));
    assert_eq!(DEFAULT_TOTAL, Duration::from_secs(60));
}

/// Catches: a retry that never waits on the real timer: a writer made with [`PutRetry::new`]
/// waits out a 503 on tokio's clock (paused here, so the wait is not taken) and stores the blob.
#[tokio::test(start_paused = true)]
async fn the_timer_of_a_running_frontend_waits_out_a_503() {
    let rig = Rig::with_memo_retry(None, Some(PutRetry::new(DEFAULT_TOTAL))).await;
    let (root, key, bytes) = unlocated(&rig).await;
    script(&rig, &key, vec![unavailable()]);
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts_of(&key["cas/l/".len()..]), 2);
}
