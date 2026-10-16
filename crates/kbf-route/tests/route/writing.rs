//! Writes and action-cache calls.

use bytes::Bytes;
use kbf_map::MemberState;
use kbf_meta::{ActionRecord, Closure, Role};
use kbf_storage::StorageError;

use super::support::{Cluster, active, blob_written_to, map, member, router, sha, unavailable};

/// Catches a blob sent to every owner (a 1 GbE writer would send it R times) or to the
/// replica: it goes once to the primary, and the answer is its digest.
#[tokio::test]
async fn a_write_goes_to_the_primary_only() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = blob_written_to(&map, &[2, 3]);

    assert_eq!(router.put(Bytes::from(blob)).await.expect("stored"), digest);
    assert_eq!(cluster.node(2).put_log(), [digest]);
    assert!(cluster.node(1).put_log().is_empty());
    assert!(cluster.node(3).put_log().is_empty());
    assert_eq!(cluster.dialled(), [(2, 1)]);
}

/// Catches a write sent to a primary the map says is down, and a joiner made the primary:
/// the write goes to the next owner, which is not down; a joiner is never first.
#[tokio::test]
async fn a_write_skips_an_owner_that_is_down() {
    let members = vec![
        member(1, MemberState::Down),
        member(2, MemberState::Active),
        member(3, MemberState::Joining),
    ];
    let map = map(1, 2, members);
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (blob, digest) = super::support::blob_where(|d| {
        map.write_owners(d) == [kbf_map::MemberId(1), kbf_map::MemberId(2)]
    });

    router.put(Bytes::from(blob)).await.expect("stored");
    assert!(cluster.node(1).put_log().is_empty());
    assert_eq!(cluster.node(2).put_log(), [digest]);
}

/// Catches a write that goes nowhere without saying so, and a node called for it: every
/// owner down is UNAVAILABLE and nobody is called.
#[tokio::test]
async fn a_write_with_every_owner_down_is_unavailable() {
    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let map = map(1, 2, all_down);
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let error = router
        .put(Bytes::from_static(b"x"))
        .await
        .expect_err("down");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(cluster.dialled().is_empty());
}

/// Catches a node's failure swallowed, or an answer for another blob passed on as success:
/// the primary's error is the caller's, and a node that says it stored a different digest
/// is an internal error.
#[tokio::test]
async fn a_write_returns_the_nodes_failure_and_checks_its_answer() {
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    cluster.node(1).fail_next(unavailable());
    let error = router
        .put(Bytes::from_static(b"x"))
        .await
        .expect_err("fails");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");

    cluster.node(1).answer_puts_with(sha(b"other"));
    let error = router
        .put(Bytes::from_static(b"x"))
        .await
        .expect_err("wrong digest");
    assert!(matches!(error, StorageError::Internal(_)), "{error:?}");
}

/// Catches a commit sent anywhere but the action's primary, or the entry then not read
/// back from the owners: a commit lands on the primary of the action digest, and a read of
/// the action finds it there.
#[tokio::test]
async fn an_action_entry_is_committed_at_the_primary_and_read_from_the_owners() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (_, action) = blob_written_to(&map, &[3, 1]);
    let result = sha(b"the result blob");
    let record = ActionRecord {
        result,
        closure: Closure::new(),
    };

    router
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("committed");
    assert_eq!(
        cluster.node(3).commits.lock().expect("lock").clone(),
        [action]
    );
    assert!(cluster.node(1).commits.lock().expect("lock").is_empty());

    let (found, _) = router
        .action_result(&action)
        .await
        .expect("an answer")
        .expect("an entry");
    assert_eq!(found, result);
}

/// Catches an entry lost behind an owner that lacks it or cannot answer: the first owner
/// has none and the second has it (found, and the first was asked); an action nobody has
/// is a miss, and so is one where some owner could not answer and the rest have none (a
/// cache is only ever missed); every owner failing is the failure.
#[tokio::test]
async fn an_action_is_looked_up_at_each_owner_until_one_has_it() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (_, action) = super::support::blob_read_from(&map, &[1, 2]);
    cluster.node(2).hold_action(action, sha(b"r"));

    let found = router.action_result(&action).await.expect("an answer");
    assert!(found.is_some());
    assert_eq!(
        cluster
            .node(1)
            .action_gets
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        cluster
            .node(2)
            .action_gets
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    let other = super::support::blobs_read_from(&map, &[1, 2], 2)
        .remove(1)
        .1;
    assert!(
        router
            .action_result(&other)
            .await
            .expect("an answer")
            .is_none()
    );

    cluster.node(1).set_down(true);
    assert!(
        router
            .action_result(&other)
            .await
            .expect("a miss")
            .is_none()
    );

    cluster.node(2).set_down(true);
    let error = router
        .action_result(&other)
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
}
