//! The question "does the farm hold this blob" as a read would answer it
//! ([`Router::find_missing_anywhere`]): the primary first, then every other owner of this
//! generation and of the previous one; and what is done about a blob found away from its
//! primary.

use std::io;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_map::MemberState;
use kbf_route::{MapView, RepairLimit, Router};
use kbf_storage::StorageError;
use kbf_types::Digest;
use tracing_subscriber::fmt::MakeWriter;

use super::support::{
    Cell, Cluster, Hook, active, blob_read_from, blob_where, map, member, router, sha, view,
};

fn limit() -> RepairLimit {
    RepairLimit {
        burst: NonZeroU32::new(100).expect("non-zero"),
        per_second: NonZeroU32::new(100).expect("non-zero"),
    }
}

/// Catches the refusal that kept a farm from running its actions after a join: the primary
/// (a joined node that is still empty) lacks a blob its replica holds. A read finds it, so
/// the question must say it is held, and the replica is asked only for what the primary
/// lacked.
#[tokio::test]
async fn a_blob_only_a_replica_holds_is_not_missing() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);

    assert_eq!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer"),
        []
    );
    assert_eq!(cluster.node(1).find_log(), [vec![digest]]);
    assert_eq!(cluster.node(2).find_log(), [vec![digest]]);
    assert!(cluster.node(3).find_log().is_empty(), "not an owner of it");
}

/// Catches a fallback that costs a call when it need not: a blob its primary holds asks
/// nobody else (a warm farm pays nothing for the fallback).
#[tokio::test]
async fn a_blob_the_primary_holds_asks_no_other_owner() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(1).hold(&blob);

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    assert!(cluster.node(2).find_log().is_empty());
}

/// Catches a fallback that stops at this generation's owners: a map change moved the blob's
/// owners (generation 2: members 1 and 2), and only the previous generation's owner (4)
/// holds it. A read finds it there, so it is not missing.
#[tokio::test]
async fn a_blob_only_the_previous_generation_holds_is_not_missing() {
    let before = map(1, 2, active(&[3, 4, 5]));
    let now = map(2, 2, active(&[1, 2]));
    let cluster = Cluster::of(5);
    let cell = Cell::with(MapView {
        map: now.clone(),
        previous: Some(before.clone()),
    });
    let router = Router::new(cell, cluster.clone());
    let (blob, digest) = blob_where(|d| {
        now.read_order(d) == [kbf_map::MemberId(1), kbf_map::MemberId(2)]
            && before.read_order(d) == [kbf_map::MemberId(3), kbf_map::MemberId(4)]
    });
    cluster.node(4).hold(&blob);

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
}

/// Catches a fallback that says present for what nobody holds, and one that loses the order
/// or the repeats of the question: of three digests, one held by the replica only and two
/// held by nobody (one asked twice), the answer is the two, in the order asked, and every
/// owner of each was asked.
#[tokio::test]
async fn a_blob_nobody_holds_is_missing_and_every_owner_was_asked() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let blobs = super::support::blobs_read_from(&map, &[1, 2], 3);
    cluster.node(2).hold(&blobs[1].0);
    let asked = [blobs[0].1, blobs[1].1, blobs[2].1, blobs[0].1];

    assert_eq!(
        router.find_missing_anywhere(&asked).await.expect("answer"),
        [blobs[0].1, blobs[2].1, blobs[0].1]
    );
    assert_eq!(
        cluster.node(2).find_log().len(),
        1,
        "one batch to the replica"
    );
}

/// Catches an owner that could not answer taken for one that lacks the blob (the call would
/// say "missing" about a blob that may be there, and a client would be told to upload it
/// again or the run refused): the primary lacks it and the replica is down, so the answer
/// is UNAVAILABLE, not "missing".
#[tokio::test]
async fn an_owner_that_cannot_answer_is_not_a_no() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (_, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).set_down(true);

    let e = router
        .find_missing_anywhere(&[digest])
        .await
        .expect_err("cannot say");
    assert!(matches!(e, StorageError::Unavailable(_)), "{e:?}");
}

/// Catches a down member asked (the call would wait for it) and a down primary treated as
/// holding or lacking: member 1 is down in the map, so the blob is asked of the owner that is
/// not down, which holds it.
#[tokio::test]
async fn a_down_member_is_not_asked() {
    let members = vec![
        member(1, MemberState::Down),
        member(2, MemberState::Active),
        member(3, MemberState::Active),
    ];
    let map = map(1, 2, members);
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) =
        blob_where(|d| map.write_owners(d) == [kbf_map::MemberId(1), kbf_map::MemberId(2)]);
    cluster.node(2).hold(&blob);

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    assert!(cluster.node(1).find_log().is_empty());
}

/// Catches the client's FindMissing following the fallback: the call a client makes asks the
/// primary alone, so a blob only a replica holds is reported missing and the client
/// uploads it (which fills the primary); the replica is never asked. (Kept apart from the
/// Execute check on purpose: a cold build asks mostly about blobs nobody holds, and a
/// replica asked on each would double its calls.)
#[tokio::test]
async fn the_clients_find_missing_asks_the_primary_alone() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);

    assert_eq!(
        router.find_missing(&[digest]).await.expect("answer"),
        [digest]
    );
    assert!(cluster.node(2).find_log().is_empty());
}

/// Catches a blob found off its primary that read repair is not told of, or told wrong: the
/// hook hears the digest, who had it, and who lacked it (the primary), once per digest; a
/// blob the primary holds is not reported, and a blob nobody holds is not reported.
#[tokio::test]
async fn a_hit_off_the_primary_is_told_to_read_repair() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let hook = Arc::new(Hook::default());
    let router = Router::new(Cell::with(view(&map)), cluster.clone())
        .with_read_repair(hook.clone(), limit());
    let blobs = super::support::blobs_read_from(&map, &[1, 2], 3);
    cluster.node(2).hold(&blobs[0].0);
    cluster.node(1).hold(&blobs[1].0);
    let asked: Vec<_> = blobs.iter().map(|b| b.1).collect();

    let missing = router.find_missing_anywhere(&asked).await.expect("answer");
    assert_eq!(missing, [blobs[2].1]);
    assert_eq!(hook.told(), [(blobs[0].1, 2, vec![1])]);
}

/// Catches a repair that never copies, or copies to the wrong place: with the router's own
/// repair installed, a blob only the replica holds is copied to the primary, from the
/// replica, after the question has been answered; a second question about it (now held by
/// both) sends nothing, and the replica is not written to.
#[tokio::test]
async fn background_repair_copies_the_blob_to_the_owner_that_lacked_it() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let router =
        Router::new(Cell::with(view(&map)), cluster.clone()).with_background_repair(limit());
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    for _ in 0..200 {
        if cluster.node(1).blob(&digest).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(cluster.node(1).blob(&digest).as_deref(), Some(&blob[..]));
    assert_eq!(cluster.node(1).put_log(), [digest]);
    assert!(cluster.node(2).put_log().is_empty());

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(cluster.node(1).put_log(), [digest], "no second copy");
}

/// Catches a repair that holds up the answer: the replica's read is gated shut, so the copy
/// cannot finish, and the question is answered all the same.
#[tokio::test]
async fn the_answer_does_not_wait_for_the_repair() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let router =
        Router::new(Cell::with(view(&map)), cluster.clone()).with_background_repair(limit());
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);
    let gate = cluster.node(2).gate();

    let answered = tokio::time::timeout(
        Duration::from_secs(5),
        router.find_missing_anywhere(&[digest]),
    )
    .await
    .expect("answered while the repair waits")
    .expect("answer");
    assert!(answered.is_empty());
    assert!(cluster.node(1).put_log().is_empty());
    gate.add_permits(100);
}

/// What the router logs, captured for the test that reads it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for Captured {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        self.clone()
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
    }
}

/// Catches a refusal that leaves no trace (the only sign was a counter): a blob nobody
/// holds is logged at warn with how many were missing, the digest, and the owners that
/// were asked about it (the primary and the replica); a blob that was found is not named.
#[tokio::test]
async fn a_blob_nobody_holds_is_logged_with_the_owners_asked() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let blobs = super::support::blobs_read_from(&map, &[1, 2], 2);
    cluster.node(2).hold(&blobs[1].0);

    let missing = router
        .find_missing_anywhere(&[blobs[0].1, blobs[1].1])
        .await
        .expect("answer");
    assert_eq!(missing, [blobs[0].1]);
    let log = captured.text();
    assert!(log.contains("WARN"), "{log}");
    assert!(log.contains("count=1"), "{log}");
    assert!(
        log.contains(&format!("{} (asked m1,m2)", blobs[0].1)),
        "{log}"
    );
    assert!(!log.contains(&blobs[1].1.to_string()), "{log}");
}

/// Catches a fallback that sends a node more digests than it takes in one call: seventy
/// thousand digests the primaries lack, one held by a replica, are asked of the replicas in
/// batches the node takes, and the answer is every digest but the held one.
#[tokio::test]
async fn a_long_list_is_asked_of_the_replicas_in_calls_a_node_takes() {
    use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let digests: Vec<Digest> = (0_u64..70_000).map(|i| sha(&i.to_le_bytes())).collect();
    let held = digests[0];
    for id in [1, 2] {
        cluster.node(id).hold_as(held, b"held");
    }
    // Held by the replica only: the digest's primary is the other node.
    let primary = map.write_owners(&digests[1])[0].0;
    cluster.node(3 - primary).hold_as(digests[1], b"replica");

    let missing = router
        .find_missing_anywhere(&digests)
        .await
        .expect("answer");
    assert_eq!(missing.len(), digests.len() - 2);
    assert!(!missing.contains(&held) && !missing.contains(&digests[1]));
    for id in [1, 2] {
        let calls = cluster.node(id).find_log();
        assert!(
            calls.iter().all(|c| c.len() <= MAX_FIND_MISSING_DIGESTS),
            "node {id}: {:?}",
            calls.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }
}

/// Catches a `STALE_MAP` from a replica taken for an answer: the replica holds a newer map,
/// the router fetches it and asks again, and the blob it holds is found.
#[tokio::test]
async fn a_stale_map_from_a_replica_runs_the_question_again() {
    let one = map(1, 2, active(&[1, 2]));
    let two = map(2, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, cell) = router(&one, &cluster);
    let (blob, digest) = blob_read_from(&one, &[1, 2]);
    cluster.node(2).hold(&blob);
    cell.answer_fetch(Ok(view(&two)));
    cluster
        .node(2)
        .fail_next(StorageError::StaleMap { current: 2 });

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    assert_eq!(cell.fetched(), [2]);
}

/// Waits until `done` holds, up to five seconds of the runtime's clock.
async fn until(done: impl Fn() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

/// Catches a repair with no bound: with the replica's reads held shut, six blobs found off
/// their primary start four copies and no more (the rest wait for a later find), and a
/// blob found again while its copy is running does not start a second one.
#[tokio::test]
async fn at_most_four_copies_run_at_once_and_a_digest_runs_once() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let router =
        Router::new(Cell::with(view(&map)), cluster.clone()).with_background_repair(limit());
    let blobs = super::support::blobs_read_from(&map, &[1, 2], 6);
    for (bytes, _) in &blobs {
        cluster.node(2).hold(bytes);
    }
    let gate = cluster.node(2).gate();
    let asked: Vec<_> = blobs.iter().map(|b| b.1).collect();

    router.find_missing_anywhere(&asked).await.expect("answer");
    until(|| cluster.node(2).read_count() == kbf_route::MAX_REPAIRS_IN_FLIGHT).await;
    router
        .find_missing_anywhere(&asked[..1])
        .await
        .expect("answer");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        cluster.node(2).read_count(),
        kbf_route::MAX_REPAIRS_IN_FLIGHT,
        "four copies, none for the digest already being copied, none for the other two"
    );

    gate.add_permits(100);
    until(|| cluster.node(1).put_log().len() == kbf_route::MAX_REPAIRS_IN_FLIGHT).await;
}

/// Catches a failed copy that is never tried again (the digest left in the set of those
/// being copied) or that takes the answer down with it: the owner refuses the first
/// copy, the question is answered all the same, and the next find of the blob copies it.
#[tokio::test]
async fn a_failed_copy_is_tried_again_by_the_next_find() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let router =
        Router::new(Cell::with(view(&map)), cluster.clone()).with_background_repair(limit());
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);
    cluster
        .node(1)
        .fail_next_put(StorageError::Internal("disk full".to_owned()));

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    until(|| cluster.node(1).find_log().len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        cluster.node(1).blob(&digest).is_none(),
        "the copy was refused"
    );

    assert!(
        router
            .find_missing_anywhere(&[digest])
            .await
            .expect("answer")
            .is_empty()
    );
    until(|| cluster.node(1).blob(&digest).is_some()).await;
}

/// Catches repair that needs a runtime the caller may not have: a find made outside one
/// is answered, and nothing is copied (and nothing panics).
#[test]
fn a_find_outside_a_runtime_is_answered_and_not_repaired() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let router =
        Router::new(Cell::with(view(&map)), cluster.clone()).with_background_repair(limit());
    let (blob, digest) = blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold(&blob);

    let missing =
        futures::executor::block_on(router.find_missing_anywhere(&[digest])).expect("answer");
    assert!(missing.is_empty());
    assert!(cluster.node(1).put_log().is_empty());
}
