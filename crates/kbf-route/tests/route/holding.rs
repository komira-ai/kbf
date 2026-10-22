//! The hold: a call that no owner could answer is held and tried again within a bound, and
//! a call an owner could answer is not.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_map::{MemberId, MemberState};
use kbf_meta::{ActionRecord, Role};
use kbf_route::{HoldClock, Router};
use kbf_storage::StorageError;
use kbf_types::Digest;

use super::support::{
    Cluster, Node, active, blob_read_from, blob_where, blob_written_to, blobs_with_primary,
    collect, map, member, router, sha, unavailable, view,
};

/// How long the routers of these tests hold a call.
const BOUND: Duration = Duration::from_secs(60);

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// Something that happens while a hold sleeps (a node comes back, say).
type Action = Box<dyn FnOnce() + Send>;

/// A clock a test drives. Sleeping takes no time: it moves the clock on by the pause, writes
/// the pause down and then does the next of the actions it was given. A hold that runs on
/// past twice the bound is a defect, and stops the test with a panic instead of hanging it.
struct FakeClock {
    elapsed: Mutex<Duration>,
    pauses: Mutex<Vec<Duration>>,
    actions: Mutex<VecDeque<Action>>,
}

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            elapsed: Mutex::default(),
            pauses: Mutex::default(),
            actions: Mutex::default(),
        })
    }

    /// `action` is done at the first sleep that no earlier action was given to.
    fn then(&self, action: Action) {
        self.actions.lock().expect("lock").push_back(action);
    }

    /// The pauses slept, in order.
    fn pauses(&self) -> Vec<Duration> {
        self.pauses.lock().expect("lock").clone()
    }
}

impl HoldClock for FakeClock {
    fn elapsed(&self) -> Duration {
        *self.elapsed.lock().expect("lock")
    }

    fn sleep(&self, pause: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut elapsed = self.elapsed.lock().expect("lock");
            *elapsed += pause;
            assert!(*elapsed <= BOUND * 2, "the hold outran its bound");
            drop(elapsed);
            self.pauses.lock().expect("lock").push(pause);
            let next = self.actions.lock().expect("lock").pop_front();
            next.into_iter().for_each(|action| action());
        })
    }
}

/// Does nothing while the hold sleeps.
fn wait() -> Action {
    Box::new(|| {})
}

/// `node` answers from the sleep that does this on.
fn comes_back(node: Arc<Node>) -> Action {
    Box::new(move || node.set_down(false))
}

/// Nodes 1 and 2, both down, both owners of the blob, node 1 first for a read and a write,
/// and a router that holds a call for [`BOUND`] by `clock`.
fn down_farm(clock: &Arc<FakeClock>) -> (Arc<Cluster>, Router, Vec<u8>, Digest) {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let router = router.with_hold(BOUND).with_hold_clock(clock.clone());
    let (blob, digest) = blob_where(|d| {
        map.write_owners(d) == [MemberId(1), MemberId(2)]
            && map.read_order(d) == [MemberId(1), MemberId(2)]
    });
    cluster.node(1).set_down(true);
    cluster.node(2).set_down(true);
    (cluster, router, blob, digest)
}

/// Catches a read that fails while the node that holds the blob is only restarting, the
/// defect the hold is for (mutant: no hold). Both owners are down; the first try and the
/// second fail, node 2 is back before the third and serves the blob. The pauses are the
/// first two of the backoff.
#[tokio::test]
async fn a_read_waits_for_an_owner_that_comes_back() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    cluster.node(2).hold(&blob);
    clock.then(wait());
    clock.then(comes_back(cluster.node(2)));

    let bytes = router.read_blob(&digest).await.expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(clock.pauses(), [ms(250), ms(500)]);
}

/// Catches a stream that is not held before its first frame: a read of a range opens when
/// the owner is back, and passes the whole blob on.
#[tokio::test]
async fn a_read_is_held_before_its_first_frame() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    cluster.node(2).hold(&blob);
    clock.then(comes_back(cluster.node(2)));

    let stream = router.read_range(&digest, 0, 0).await.expect("open");
    let (bytes, _) = collect(stream).await.expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(clock.pauses(), [ms(250)]);
}

/// Catches a write that fails while its owners restart: every attempt is UNAVAILABLE, the
/// router waits, and the blob is stored by node 1 once it answers.
#[tokio::test]
async fn a_write_waits_for_an_owner_that_comes_back() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    clock.then(comes_back(cluster.node(1)));

    let stored = router.put(Bytes::from(blob)).await.expect("stored");
    assert_eq!(stored, digest);
    assert_eq!(cluster.node(1).put_log(), [digest]);
    assert_eq!(clock.pauses(), [ms(250)]);
}

/// Catches a FindMissing or an action-cache lookup that skips the hold: each is held until
/// an owner answers.
#[tokio::test]
async fn a_find_and_an_action_lookup_are_held_too() {
    let clock = FakeClock::new();
    let (cluster, router, _, digest) = down_farm(&clock);
    clock.then(comes_back(cluster.node(1)));
    let missing = router.find_missing(&[digest]).await.expect("answer");
    assert_eq!(missing, [digest]);
    assert_eq!(clock.pauses(), [ms(250)]);

    let clock = FakeClock::new();
    let (cluster, router, _, digest) = down_farm(&clock);
    cluster.node(2).hold_action(digest, sha(b"result"));
    clock.then(comes_back(cluster.node(2)));
    let found = router.action_result(&digest).await.expect("answer");
    assert!(found.is_some());
    assert_eq!(clock.pauses(), [ms(250)]);
}

/// Catches a hold that reads the map once: every owner is `Down` in the map (the router
/// has nobody to call), and node 1 registers again during the hold, which changes the
/// map. The next try routes by the new map and dials node 1 with its generation, and it is
/// the only node dialled.
#[tokio::test]
async fn a_hold_reads_the_map_again_on_each_try() {
    let clock = FakeClock::new();
    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let gone = map(1, 2, all_down);
    let back = map(2, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, cell) = router(&gone, &cluster);
    let router = router.with_hold(BOUND).with_hold_clock(clock.clone());
    let (blob, digest) = blob_read_from(&back, &[1, 2]);
    cluster.node(1).hold(&blob);
    let registered = Arc::clone(&back);
    clock.then(Box::new(move || cell.set(view(&registered))));

    let bytes = router.read_blob(&digest).await.expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(clock.pauses(), [ms(250)]);
    assert_eq!(cluster.dialled(), [(1, 2)]);
}

/// Catches a hold that never ends (mutant: unbounded) and one that ends late (a last
/// pause that is not cut short at the bound): nobody comes back, the pauses double up to
/// five seconds and the last is what is left of the bound, 60 s in all, the call is tried
/// one time more than it paused, and it is UNAVAILABLE as it is without a hold. The fake
/// clock panics if the hold runs past twice the bound.
#[tokio::test]
async fn a_hold_ends_at_the_bound() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    cluster.node(2).hold(&blob);

    let error = router.read_blob(&digest).await.expect_err("nobody");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(clock.elapsed(), BOUND);
    let pauses = clock.pauses();
    assert_eq!(
        pauses[..6],
        [ms(250), ms(500), ms(1000), ms(2000), ms(4000), ms(5000)]
    );
    assert_eq!(pauses.len(), 16);
    assert_eq!(pauses.last(), Some(&ms(2250)));
    assert_eq!(cluster.node(1).read_count(), 17);
}

/// Catches a hold on a call an owner could answer, or on a failure that is an answer
/// (mutant: hold on any failure): a blob every owner lacks is NOT_FOUND, a refused write
/// is `Invalid`, and with owner 1 down a read is served by owner 2 and a write goes to it.
/// None of them waits.
#[tokio::test]
async fn a_call_an_owner_answers_is_not_held() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    cluster.node(1).set_down(false);
    cluster.node(2).set_down(false);

    let error = router.read_blob(&digest).await.expect_err("absent");
    assert!(
        matches!(error, StorageError::NotFound(d) if d == digest),
        "{error:?}"
    );

    cluster
        .node(1)
        .fail_next(StorageError::Invalid("refused".to_owned()));
    let error = router
        .put(Bytes::from(blob.clone()))
        .await
        .expect_err("refused");
    assert!(matches!(error, StorageError::Invalid(_)), "{error:?}");

    cluster.node(1).set_down(true);
    cluster.node(2).hold(&blob);
    let bytes = router.read_blob(&digest).await.expect("bytes");
    assert_eq!(bytes, blob);
    let stored = router.put(Bytes::from(blob)).await.expect("stored");
    assert_eq!(stored, digest);
    assert_eq!(cluster.node(2).put_log(), [digest]);
    assert!(clock.pauses().is_empty());
}

/// Catches a hold applied to a read that is already passing bytes on (mutant: hold applied
/// mid-stream): the first chunk arrives, the owner breaks, and the read goes on through
/// the router; no owner answers it, and the stream fails at once with UNAVAILABLE instead
/// of waiting. The other owner is asked once, and nothing sleeps.
#[tokio::test]
async fn a_stream_past_its_first_frames_is_not_held() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    for id in [1, 2] {
        cluster.node(id).set_down(false);
        cluster.node(id).hold(&blob);
    }
    cluster.node(1).break_read(1, unavailable());
    cluster.node(1).fail_read(unavailable());
    cluster.node(2).fail_read(unavailable());

    let mut stream = router.read_range(&digest, 0, 0).await.expect("open");
    let first = stream.next().await.expect("chunk").expect("bytes");
    assert_eq!(first.as_ref(), &blob[..4]);
    let error = stream.next().await.expect("item").expect_err("err");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(clock.pauses().is_empty());
    assert_eq!(cluster.node(1).read_count(), 2);
    assert_eq!(cluster.node(2).read_count(), 1);
}

/// Catches a failure that arrives as a stream's first frame going without a hold (mutant:
/// `open` hands the stream over without looking at its first frame): owner 1 accepts the
/// read and breaks before any byte, owner 2 is down, so no owner answers. The read waits
/// one pause, and owner 1 then serves the whole blob; unheld, the read would be resumed at
/// once and never wait.
#[tokio::test]
async fn a_read_whose_first_frame_fails_is_held() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    cluster.node(1).set_down(false);
    cluster.node(1).hold(&blob);
    cluster.node(1).break_read(0, unavailable());

    let stream = router.read_range(&digest, 0, 0).await.expect("open");
    let (bytes, _) = collect(stream).await.expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(clock.pauses(), [ms(250)]);
    assert_eq!(cluster.node(1).read_count(), 2);
}

/// Catches a read held for a miss that an owner that is away could not confirm (the other
/// side of mutant: hold on any failure): owner 2 says it lacks the blob and owner 1 is
/// restarting, so the blob may be on owner 1 and the read waits for it, one pause, and is
/// served when it is back.
#[tokio::test]
async fn a_miss_that_an_owner_away_could_not_confirm_is_held() {
    let clock = FakeClock::new();
    let (cluster, router, blob, digest) = down_farm(&clock);
    cluster.node(1).hold(&blob);
    cluster.node(2).set_down(false);
    clock.then(comes_back(cluster.node(1)));

    let bytes = router.read_blob(&digest).await.expect("bytes");
    assert_eq!(bytes, blob);
    assert_eq!(clock.pauses(), [ms(250)]);
}

/// Catches a commit held twice over (mutant: the copy of a blob reads through the held
/// router): the action's owner is down for the first try and back for the second, and an
/// output of the entry cannot be read (owner 1 is down, owner 2 lacks it). The commit
/// waits out its own bound and no more; a read that held for itself, from the second try,
/// would end the commit a whole bound after that try began.
#[tokio::test]
async fn a_commit_is_held_for_one_bound_not_one_per_blob_it_reads() {
    let clock = FakeClock::new();
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let router = router.with_hold(BOUND).with_hold_clock(clock.clone());
    cluster.node(3).check_closure();
    let (_, action) = blob_written_to(&map, &[3, 1]);
    let output = blobs_with_primary(&map, 1, 1).remove(0);
    let result = blobs_with_primary(&map, 2, 1).remove(0);
    let record = ActionRecord {
        result: result.1,
        closure: [output.1].into_iter().collect(),
    };
    cluster.node(2).hold(&result.0);
    cluster.node(1).set_down(true);
    cluster.node(3).set_down(true);
    clock.then(comes_back(cluster.node(3)));

    let error = router
        .commit_with_closure(Role::Daemon, action, record)
        .await
        .expect_err("the output cannot be read");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert_eq!(clock.elapsed(), BOUND);
    assert!(cluster.node(3).commits.lock().expect("lock").is_empty());
}

/// Catches a default clock that is not Tokio's: with no clock given, a router holds by
/// Tokio's time, which a paused test runs at once. A node that never comes back is waited
/// for the whole bound, and no longer.
#[tokio::test(start_paused = true)]
async fn the_default_clock_is_tokios() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    cluster.node(1).set_down(true);
    let (router, _) = router(&map, &cluster);
    let router = router.with_hold(BOUND);
    let started = tokio::time::Instant::now();

    let error = router
        .put(Bytes::from_static(b"nobody home"))
        .await
        .expect_err("held");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(started.elapsed() >= BOUND);
    assert!(started.elapsed() < BOUND + Duration::from_secs(1));
}
