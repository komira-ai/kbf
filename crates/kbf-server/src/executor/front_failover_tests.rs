//! The executor's preparation and ingest over a store that fails over its metadata: while the
//! store answers every call 503 for 30 s, a run's action tree is written and its pack ingested
//! as if the store had answered at once, so the run's attempt is not spent; a store down past
//! the bound of the waits makes the run unavailable (the farm's, retried), saying so. The clock
//! is the runtime's, paused: a window of 30 s passes while nothing else can run.

use kbf_front::store_metrics::Op;
use kbf_front::store_wait::STORE_WAIT;

use super::put_dir::{WALL, action_with_input, rig};
use super::*;

/// A failover of the store's metadata: the longest the store answers 503 for.
const FAILOVER: Duration = Duration::from_secs(30);

/// The waits taken so far, of every op.
fn waits_taken(rig: &Rig) -> u64 {
    Op::ALL.iter().map(|op| rig.waits.waits(*op).count()).sum()
}

/// Catches a run whose preparation or ingest fails while the store fails over, which would spend
/// one of the operation's attempts: with the store answering every call 503 for 30 s, the run's
/// put directory and action tree are made, and with it failing over again for 30 s, the pack the
/// run committed is ingested into the action's result; the store refused calls in both windows,
/// and the waits were counted with no call running out of them.
#[tokio::test(start_paused = true)]
async fn a_run_is_prepared_and_ingested_through_a_failover_of_30_seconds() {
    let rig = rig();
    let action = action_with_input(&rig, "input").await;
    rig.store.fail_over_for(FAILOVER);

    let key = rig
        .front
        .mint(lease(), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    let tree = rig.front.tree(lease()).await.expect("a tree");

    let manifest = ObjectKey::new(format!("cas/l/{tree}")).expect("a key");
    assert!(
        rig.store
            .inner
            .len(&manifest)
            .await
            .expect("asked")
            .is_some()
    );
    let refused_preparing = rig.store.refused.load(Ordering::SeqCst);
    assert!(
        refused_preparing > 0,
        "the store never refused the preparation"
    );
    let waited_preparing = waits_taken(&rig);
    let pack = write_pack(&manifest_of("7-3", &action), &[]).expect("a pack");
    let object = ObjectKey::new(format!("{key}/pack")).expect("an object key");
    rig.store
        .inner
        .put_new(&object, Bytes::from(pack), None)
        .await
        .expect("written");
    rig.store.fail_over_for(FAILOVER);

    let done = rig.front.ingest(lease(), &key, &action, &limits()).await;

    assert!(
        matches!(done, Ingested::Ran { cached: true, .. }),
        "{done:?}"
    );
    assert!(rig.store.refused.load(Ordering::SeqCst) > refused_preparing);
    assert!(
        waits_taken(&rig) > waited_preparing,
        "the ingest never waited"
    );
    for op in Op::ALL {
        assert_eq!(rig.waits.exhausted(op), 0, "{op:?} ran out of its waits");
    }
}

/// Catches a store down for good that hangs a run, or fails it as the caller's: with the store
/// answering 503 for an hour, the run's preparation is unavailable (the farm's, which the
/// executor runs again), its reason naming the store's 503 and saying it was still answered
/// after the waits; and the ingest of a committed pack after it is unavailable, not a refusal,
/// at once, the store being taken as down: the two waited 60 s in all, once.
#[tokio::test(start_paused = true)]
async fn a_store_down_past_the_bound_makes_the_run_unavailable_saying_so() {
    let rig = rig();
    let action = action_with_input(&rig, "input").await;
    let key = rig
        .front
        .mint(lease(), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    rig.store.fail_over_for(Duration::from_secs(3_600));

    let other = action_with_input(&rig, "other").await;
    let down = rig
        .front
        .mint(
            RunId::new(OperationId(8), 0),
            &other,
            WALL,
            InputTree::Manifests,
        )
        .await;
    let ingested = rig.front.ingest(lease(), &key, &action, &limits()).await;

    assert!(
        matches!(&down, Err(MintError::Unavailable(why))
            if why.contains("store answered 503 ServiceUnavailable: failing over (still so after")),
        "{down:?}"
    );
    assert!(
        matches!(&ingested, Ingested::Unavailable(why) if why.contains("(not waited for")),
        "{ingested:?}"
    );
    let exhausted: u64 = Op::ALL.iter().map(|op| rig.waits.exhausted(*op)).sum();
    assert!(
        exhausted >= 2,
        "the preparation and the ingest each ran out: {exhausted}"
    );
    let waited: u64 = Op::ALL.iter().map(|op| rig.waits.waits(*op).sum()).sum();
    assert_eq!(
        waited,
        STORE_WAIT.as_nanos() as u64,
        "the bound was waited more than once"
    );
}
