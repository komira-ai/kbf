//! Where the time of each write of a tree goes, apart from the store's request
//! (`WriteTimed::Write`), and how many manifests a tree writes (`WriteTimed::Manifests`): every
//! write a tree makes of its manifests, its root or its list is told once, with its wait for a
//! manifest slot, its 503 waits and its request apart; a manifest the memo had, or that another
//! tree had in flight, is neither told nor counted; a failed write is told, and a failed
//! manifests stage counts nothing.
//!
//! The times are read on tokio's paused clock, which moves only by the fake store's fixed delay
//! of each write and the retry's waits, so every part is exact. (The wait for a slot of the pack
//! store is the pack store's, and is tested with it, in `store_writes_tests`, and through a write
//! in flight, in `action_tree/flight_tests`.)

use std::pin::pin;

use futures::poll;
use kbf_front::action_tree::{FIRST_WAIT, WriteStage, WriteTimed, WriteWaits};
use kbf_front::start_retry::RetryClock;
use kbf_front::store_retry::RetryPolicy;

use super::listing::listed_base;
use super::memo::{holding, link_root};
use super::*;

/// The time each write of the store takes when the test sets [`Faults::delay`].
const WRITE: Duration = Duration::from_millis(10);

/// What a tree's writes told their timer, in order.
type Told = Arc<Mutex<Vec<WriteTimed>>>;

/// A timer of everything a tree's writes tell into `told`.
fn timer(told: &Told) -> impl Fn(WriteTimed) + Send + Sync + 'static {
    let told = Arc::clone(told);
    move |event| {
        told.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event)
    }
}

/// What was told so far, which is taken out.
fn taken(told: &Told) -> Vec<WriteTimed> {
    std::mem::take(&mut *told.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The waits of a write that took one store write and waited for nothing.
const fn one_write() -> WriteWaits {
    request(WRITE)
}

/// The waits of a write that waited for nothing and took `took` at the store.
const fn request(took: Duration) -> WriteWaits {
    WriteWaits {
        manifest_slot: Duration::ZERO,
        store_slot: Duration::ZERO,
        store_503: Duration::ZERO,
        request: took,
    }
}

/// An input root of two directories, the root holding a subdirectory: with `.kbf`, three
/// manifests in the manifests stage.
async fn two_deep(rig: &Rig) -> KeptClosure {
    let leaf = link_root(rig, "a").await;
    let root = holding(rig, "z", &leaf).await;
    rig.kept(&root).await
}

/// A [`RetryClock`] on tokio's clock: on a paused clock, each wait is exactly the nominal one
/// (the policy of [`retry`] has no jitter).
struct OnTokio(tokio::time::Instant);

impl RetryClock for OnTokio {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(wait))
    }
}

/// A retry with a budget of 60 s and no jitter, waiting on tokio's clock.
fn retry() -> PutRetry {
    let policy = RetryPolicy {
        jitter_percent: 0,
        ..RetryPolicy::DEFAULT
    }
    .with_budget(Duration::from_secs(60));
    PutRetry::with_clock(policy, Arc::new(OnTokio(tokio::time::Instant::now())))
}

/// Catches a write of a tree not told, told twice or under another stage, a manifests stage
/// that counts the writes the memo saved, or a count told before the stage ended: a tree of two
/// directories tells each of its three manifest writes (the two directories' and `.kbf`'s), each
/// one store write long and waiting for nothing, then the stage, then a count of 3, then the
/// root's write and its stage. The same tree again finds both directories in the memo: only
/// `.kbf` is written again (the store finds it there, which is still a write), and the count is 1.
#[tokio::test(start_paused = true)]
async fn each_write_is_told_once_and_the_manifests_stage_counts_what_it_wrote() {
    let rig = Rig::new().await;
    rig.farm.state().faults.delay = Some(WRITE);
    let kept = two_deep(&rig).await;
    let told = Told::default();
    for count in [3, 1] {
        (rig.writer)
            .write_named(&kept, &rig.base.tree, |_| (), timer(&told))
            .await
            .expect("a tree");
        let mut want = vec![WriteTimed::Write(WriteStage::Manifests, one_write()); count];
        want.extend([
            WriteTimed::Stage(WriteStage::Manifests, WRITE),
            WriteTimed::Manifests(count as u64),
            WriteTimed::Write(WriteStage::Root, one_write()),
            WriteTimed::Stage(WriteStage::Root, WRITE),
        ]);
        assert_eq!(taken(&told), want, "a count of {count}");
    }
}

/// Catches the wait for a manifest slot left in the request, or told to the wrong write: with
/// one slot, the three manifests of the stage queue for it, and wait 0, 10 and 20 ms for it,
/// each with a request of one write; the stage takes three writes, and the root, alone, waits
/// for none.
#[tokio::test(start_paused = true)]
async fn the_wait_for_a_manifest_slot_is_told_apart_from_the_request() {
    let rig = Rig::new().await.with_write_slots(1);
    rig.farm.state().faults.delay = Some(WRITE);
    let kept = two_deep(&rig).await;
    let told = Told::default();
    (rig.writer)
        .write_named(&kept, &rig.base.tree, |_| (), timer(&told))
        .await
        .expect("a tree");
    let told = taken(&told);
    let mut slots: Vec<Duration> = (told.iter())
        .filter_map(|event| match event {
            WriteTimed::Write(WriteStage::Manifests, waits) => {
                assert_eq!(waits.request, WRITE, "{event:?}");
                Some(waits.manifest_slot)
            }
            _ => None,
        })
        .collect();
    slots.sort();
    assert_eq!(slots, [0, 10, 20].map(Duration::from_millis));
    let rest: Vec<WriteTimed> = (told.into_iter())
        .filter(|event| !matches!(event, WriteTimed::Write(WriteStage::Manifests, _)))
        .collect();
    let want = [
        WriteTimed::Stage(WriteStage::Manifests, 3 * WRITE),
        WriteTimed::Manifests(3),
        WriteTimed::Write(WriteStage::Root, one_write()),
        WriteTimed::Stage(WriteStage::Root, WRITE),
    ];
    assert_eq!(rest, want);
}

/// Catches a 503's wait counted as the request, or a write's attempts not all in its request:
/// the root answered `SlowDown` and then a plain 503 waits 0.5 s and then 1 s (the retry's first
/// waits, doubling), and is stored by its third attempt: its 503 waits are 1.5 s and its request
/// three writes, and its stage is the sum of both.
#[tokio::test(start_paused = true)]
async fn the_503_waits_of_a_write_are_told_apart_from_its_request() {
    let rig = Rig::with_memo_retry(None, Some(retry())).await;
    rig.farm.state().faults.delay = Some(WRITE);
    let kept = rig.kept(&link_root(&rig, "a").await).await;
    let farm = Arc::clone(&rig.farm);
    let refuse = move |tree: &ActionTree| {
        let busy = |code: &str| ObjectStoreError::Service {
            status: 503,
            code: code.to_owned(),
            message: "retry later".to_owned(),
        };
        let answers = VecDeque::from([busy("SlowDown"), busy("ServiceUnavailable")]);
        let key = whole_key(&tree.digest().to_string());
        farm.state().faults.script.insert(key, answers);
    };
    let told = Told::default();
    (rig.writer)
        .write_named(&kept, &rig.base.tree, refuse, timer(&told))
        .await
        .expect("a tree");
    let waited = FIRST_WAIT + 2 * FIRST_WAIT;
    let root = WriteWaits {
        store_503: waited,
        ..request(3 * WRITE)
    };
    let told = taken(&told);
    assert_eq!(
        told[told.len() - 2..],
        [
            WriteTimed::Write(WriteStage::Root, root),
            WriteTimed::Stage(WriteStage::Root, waited + 3 * WRITE),
        ]
    );
}

/// Catches a failed write left untold, or a count told for a stage that did not store its
/// manifests: with the stage's first write refused, that write is told, then the stage (its
/// refusal ends it before `.kbf`'s write is made), and no count and nothing of the root follow.
/// With the root refused instead, the manifests are counted, and the root's failed write is told
/// before its stage.
#[tokio::test(start_paused = true)]
async fn a_failed_write_is_told_and_a_failed_manifests_stage_counts_nothing() {
    let rig = Rig::new().await;
    let kept = rig.kept(&link_root(&rig, "a").await).await;
    rig.farm.state().faults.any = VecDeque::from([ObjectStoreError::Unsupported("no writes")]);
    let told = Told::default();
    (rig.writer)
        .write_named(&kept, &rig.base.tree, |_| (), timer(&told))
        .await
        .expect_err("no tree");
    let stages = |told: &[WriteTimed]| -> Vec<(WriteStage, bool)> {
        (told.iter())
            .filter_map(|event| match event {
                WriteTimed::Write(stage, _) => Some((*stage, true)),
                WriteTimed::Stage(stage, _) => Some((*stage, false)),
                WriteTimed::Manifests(_) => None,
            })
            .collect()
    };
    let told_now = taken(&told);
    assert_eq!(
        stages(&told_now),
        [
            (WriteStage::Manifests, true),
            (WriteStage::Manifests, false)
        ]
    );
    assert!(
        !(told_now.iter()).any(|event| matches!(event, WriteTimed::Manifests(_))),
        "{told_now:?}"
    );
    let farm = Arc::clone(&rig.farm);
    let refuse_root = move |tree: &ActionTree| {
        farm.state().faults.put = Some(whole_key(&tree.digest().to_string()));
    };
    let other = rig.kept(&link_root(&rig, "b").await).await;
    (rig.writer)
        .write_named(&other, &rig.base.tree, refuse_root, timer(&told))
        .await
        .expect_err("no tree");
    let told = taken(&told);
    assert_eq!(told[3], WriteTimed::Manifests(2), "{told:?}");
    assert_eq!(
        stages(&told)[3..],
        [(WriteStage::Root, true), (WriteStage::Root, false)]
    );
}

/// Catches a manifest written by one tree told, or counted, for another tree that waited for it:
/// two trees of one input root written at once, the second asking while the first's writes are
/// held at the store, share the manifests stage's writes. The first tree is told its three writes
/// and counts 3; the second, which made none of them, is told none of them and counts 0.
#[tokio::test(start_paused = true)]
async fn a_manifest_another_tree_had_in_flight_is_told_only_to_that_tree() {
    let rig = Rig::new().await;
    let kept = two_deep(&rig).await;
    rig.farm.state().faults.hold_all = true;
    let (first, second) = (Told::default(), Told::default());
    let mut one = pin!(
        rig.writer
            .write_named(&kept, &rig.base.tree, |_| (), timer(&first))
    );
    let mut two = pin!(
        rig.writer
            .write_named(&kept, &rig.base.tree, |_| (), timer(&second))
    );
    assert!(poll!(one.as_mut()).is_pending());
    assert!(poll!(two.as_mut()).is_pending());
    assert_eq!(
        rig.farm.puts().len(),
        3,
        "the second tree made no write of its own"
    );
    rig.farm.gate.add_permits(16);
    let (one, two) = futures::join!(one, two);
    let (one, two) = (one.expect("a tree"), two.expect("a tree"));
    assert_eq!(one, two);
    let manifests = |told: &[WriteTimed]| -> (usize, Vec<u64>) {
        let writes = (told.iter())
            .filter(|event| matches!(event, WriteTimed::Write(WriteStage::Manifests, _)))
            .count();
        let counts = (told.iter())
            .filter_map(|event| match event {
                WriteTimed::Manifests(count) => Some(*count),
                _ => None,
            })
            .collect();
        (writes, counts)
    };
    assert_eq!(manifests(&taken(&first)), (3, vec![3]));
    assert_eq!(manifests(&taken(&second)), (0, vec![0]));
}

/// Catches a tree list's write not told, told with a manifest slot it does not take, or told
/// after its stage: the list's one write, one store write long, is told with no waits, just
/// before the list's stage, and the list counts no manifests.
#[tokio::test(start_paused = true)]
async fn a_tree_lists_write_is_told_with_no_manifest_slot() {
    let rig = Rig::new().await;
    rig.farm.state().faults.delay = Some(WRITE);
    let kept = rig.kept(&link_root(&rig, "a").await).await;
    let told = Told::default();
    let how = kbf_front::action_tree::ListingWrite::default();
    (rig.writer)
        .write_listing_with(&kept, &listed_base(&rig), how, |_| (), timer(&told))
        .await
        .expect("a list");
    assert_eq!(
        taken(&told),
        [
            WriteTimed::Write(WriteStage::List, one_write()),
            WriteTimed::Stage(WriteStage::List, WRITE),
        ]
    );
}

/// Catches a refused list's write left untold, or told after its stage: with the list's write
/// refused, the write is still told, then the list's stage, and the list is not stored.
#[tokio::test(start_paused = true)]
async fn a_refused_tree_lists_write_is_told_before_its_stage() {
    let rig = Rig::new().await;
    rig.farm.state().faults.delay = Some(WRITE);
    rig.farm.state().faults.any = VecDeque::from([ObjectStoreError::Unsupported("no writes")]);
    let kept = rig.kept(&link_root(&rig, "a").await).await;
    let told = Told::default();
    let how = kbf_front::action_tree::ListingWrite::default();
    (rig.writer)
        .write_listing_with(&kept, &listed_base(&rig), how, |_| (), timer(&told))
        .await
        .expect_err("no list");
    assert_eq!(
        taken(&told),
        [
            WriteTimed::Write(WriteStage::List, one_write()),
            WriteTimed::Stage(WriteStage::List, WRITE),
        ]
    );
}

/// Catches a manifest's 503 wait lost inside the write in flight or counted as its request:
/// with the stage's first write answered `SlowDown` once, exactly one of its two manifest writes
/// is told 0.5 s of 503 wait and two writes of request, the other one write and no wait, and the
/// stage takes the longer of the two.
#[tokio::test(start_paused = true)]
async fn a_manifests_503_wait_is_told_apart_from_its_request() {
    let rig = Rig::with_memo_retry(None, Some(retry())).await;
    rig.farm.state().faults.delay = Some(WRITE);
    rig.farm.state().faults.any = VecDeque::from([ObjectStoreError::Service {
        status: 503,
        code: "SlowDown".to_owned(),
        message: "retry later".to_owned(),
    }]);
    let kept = rig.kept(&link_root(&rig, "a").await).await;
    let told = Told::default();
    (rig.writer)
        .write_named(&kept, &rig.base.tree, |_| (), timer(&told))
        .await
        .expect("a tree");
    let told = taken(&told);
    let mut writes: Vec<WriteWaits> = (told.iter())
        .filter_map(|event| match event {
            WriteTimed::Write(WriteStage::Manifests, waits) => Some(*waits),
            _ => None,
        })
        .collect();
    writes.sort_by_key(|waits| waits.store_503);
    let waited = WriteWaits {
        store_503: FIRST_WAIT,
        ..request(2 * WRITE)
    };
    assert_eq!(writes, [one_write(), waited]);
    let stage = WriteTimed::Stage(WriteStage::Manifests, FIRST_WAIT + 2 * WRITE);
    assert!(told.contains(&stage), "{told:?}");
}
