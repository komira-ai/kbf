//! The question "does the farm hold this blob" as a read would answer it
//! ([`Router::find_missing_anywhere`]): the primary first, then every other owner of this
//! generation and of the previous one; and what is done about a blob found away from its
//! primary.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use kbf_map::MemberState;
use kbf_route::{MapView, RepairLimit, Router};
use kbf_storage::StorageError;

use super::support::{
    Cell, Cluster, Hook, active, blob_read_from, blob_where, map, member, router, view,
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
/// holding or lacking: member 1 is down in the map, so the blob's owners are 2 and 3; 3
/// holds it.
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
