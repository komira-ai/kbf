//! FindMissing: split by primary, merged in the order asked.

use kbf_map::MemberState;
use kbf_storage::StorageError;
use kbf_types::Digest;

use super::support::{
    Cluster, active, blob_where, blobs_with_primary, map, member, router, sha, unavailable,
};

/// The digests of `count` blobs whose primary is `id`.
fn primary_is(map: &kbf_map::Map, id: u64, count: usize) -> Vec<Digest> {
    blobs_with_primary(map, id, count)
        .into_iter()
        .map(|(_, d)| d)
        .collect()
}

/// `digests` in digest order: a node is asked each digest once, in this order.
fn sorted(mut digests: Vec<Digest>) -> Vec<Digest> {
    digests.sort();
    digests
}

/// Catches digests asked of every owner, asked in one call of everything, or answered in
/// the nodes' order: three digests each of two primaries are asked as one call to each
/// primary naming exactly its own (each once, though one was asked twice), the replicas are never asked, and the answer is the
/// missing ones in the order asked, a repeated digest answered twice.
#[tokio::test]
async fn digests_are_asked_of_their_primaries_and_answered_in_order() {
    let map = map(1, 2, active(&[1, 2, 3]));
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (of_1, of_2) = (primary_is(&map, 1, 3), primary_is(&map, 2, 3));
    // Interleaved, with one repeated; node 1 and node 2 hold the first of their own.
    let asked = vec![
        of_2[0], of_1[0], of_2[1], of_1[1], of_2[2], of_1[2], of_1[1],
    ];
    cluster.node(1).hold_as(of_1[0], b"x");
    cluster.node(2).hold_as(of_2[0], b"y");

    let missing = router.find_missing(&asked).await.expect("an answer");
    assert_eq!(missing, [of_2[1], of_1[1], of_2[2], of_1[2], of_1[1]]);
    assert_eq!(cluster.node(1).find_log(), [sorted(of_1.clone())]);
    assert_eq!(cluster.node(2).find_log(), [sorted(of_2.clone())]);
    assert!(cluster.node(3).find_log().is_empty());
    assert_eq!(cluster.dialled(), [(1, 1), (2, 1)]);
}

/// Catches an empty question calling anybody.
#[tokio::test]
async fn nothing_asked_calls_nobody() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    assert_eq!(router.find_missing(&[]).await.expect("an answer"), []);
    assert!(cluster.dialled().is_empty());
}

/// Catches a primary that is down in the map still asked (the call would fail or wait for
/// it) and a joiner that is the primary: the down primary is skipped and the next owner is
/// asked in its place.
#[tokio::test]
async fn a_down_primary_is_skipped() {
    let members = vec![
        member(1, MemberState::Down),
        member(2, MemberState::Active),
        member(3, MemberState::Active),
    ];
    let map = map(1, 2, members);
    let cluster = Cluster::of(3);
    let (router, _) = router(&map, &cluster);
    let (_, digest) =
        blob_where(|d| map.write_owners(d) == [kbf_map::MemberId(1), kbf_map::MemberId(2)]);

    assert_eq!(
        router.find_missing(&[digest]).await.expect("an answer"),
        [digest]
    );
    assert!(cluster.node(1).find_log().is_empty());
    assert_eq!(cluster.node(2).find_log(), [vec![digest]]);
}

/// Catches a primary's failure failing the whole call, or its digests left unanswered: the
/// primary of the first three digests fails, so those go to their next owner and the call
/// still answers every digest; the other primary's digests are not asked twice.
#[tokio::test]
async fn a_primary_that_fails_hands_its_digests_to_the_next_owner() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let (of_1, of_2) = (primary_is(&map, 1, 2), primary_is(&map, 2, 2));
    cluster.node(1).fail_next(unavailable());
    cluster.node(2).hold_as(of_1[0], b"replica copy");

    let asked = vec![of_1[0], of_2[0], of_1[1], of_2[1]];
    let missing = router.find_missing(&asked).await.expect("an answer");
    assert_eq!(missing, [of_2[0], of_1[1], of_2[1]]);
    let log = cluster.node(2).find_log();
    assert_eq!(log, [sorted(of_2.clone()), sorted(of_1.clone())]);
}

/// Catches a partial answer when nobody could answer for some digest: the primary and the
/// replica both fail and the call fails, naming the failure; with every owner of a digest
/// down it says so.
#[tokio::test]
async fn a_digest_nobody_can_answer_for_fails_the_call() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let digest = primary_is(&map, 1, 1)[0];
    cluster.node(1).set_down(true);
    cluster.node(2).set_down(true);
    let error = router
        .find_missing(&[digest])
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");

    let all_down = vec![member(1, MemberState::Down), member(2, MemberState::Down)];
    let map = super::support::map(1, 2, all_down);
    let (router, _) = super::support::router(&map, &cluster);
    let error = router
        .find_missing(&[sha(b"x")])
        .await
        .expect_err("all down");
    assert!(error.to_string().contains("is down"), "{error}");
}

/// Catches a long list asked of a node in one call (a node refuses a call of more than
/// `MAX_FIND_MISSING_DIGESTS` digests as invalid, and the whole call would fail): 70 000
/// digests for one primary go as a call of the most a node takes and a call of the rest, and
/// the answer is every digest that node lacks.
#[tokio::test]
async fn a_long_list_is_asked_in_calls_a_node_takes() {
    use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
    let map = map(1, 1, active(&[1]));
    let cluster = Cluster::of(1);
    let (router, _) = router(&map, &cluster);
    let digests: Vec<Digest> = (0_u64..70_000).map(|i| sha(&i.to_le_bytes())).collect();
    let held = digests[0];
    cluster.node(1).hold_as(held, b"held");

    let missing = router.find_missing(&digests).await.expect("an answer");
    assert_eq!(missing.len(), digests.len() - 1);
    assert!(!missing.contains(&held));
    let calls: Vec<usize> = cluster.node(1).find_log().iter().map(Vec::len).collect();
    assert_eq!(
        calls,
        [MAX_FIND_MISSING_DIGESTS, 70_000 - MAX_FIND_MISSING_DIGESTS]
    );
}

/// Catches a durable question that is a plain one (a node's blob held only in RAM would
/// be reported durable, so a durable record could name a blob a restart loses), and one
/// that asks the wrong owners or tells the node the wrong thing: the primary holds `a` on
/// disk and `b` in RAM only, and has not got `c`. `find_durable` answers `b` and `c`,
/// calling only `find_durable`; `find_missing` answers `c` alone, calling only
/// `find_missing`; the replica is asked neither time.
#[tokio::test]
async fn a_durable_question_counts_a_blob_held_only_in_ram_as_missing() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let owned = primary_is(&map, 1, 3);
    let (a, b, c) = (owned[0], owned[1], owned[2]);
    cluster.node(1).hold_as(a, b"on disk");
    cluster.node(1).hold_as(b, b"in ram");
    cluster.node(1).make_ram_only(&b);

    let durable = router
        .find_durable(&[a, b, c], false)
        .await
        .expect("an answer");
    assert_eq!(durable, [b, c]);
    assert_eq!(cluster.node(1).durable_log(), [sorted(vec![a, b, c])]);
    assert!(cluster.node(1).find_log().is_empty());

    let plain = router.find_missing(&[a, b, c]).await.expect("an answer");
    assert_eq!(plain, [c]);
    assert_eq!(cluster.node(1).find_log(), [sorted(vec![a, b, c])]);
    assert_eq!(cluster.node(1).durable_log().len(), 1);
    assert!(cluster.node(2).find_log().is_empty());
    assert!(cluster.node(2).durable_log().is_empty());
}

/// Catches a durable question that falls back to the plain one when the primary fails
/// (the replica would then report its RAM-held blob durable): the primary fails, so its
/// digests go to the replica, which is asked `find_durable`, answers that the blob it
/// holds in RAM is missing, and is never asked the plain question. A call that cannot be
/// answered for a digest fails, as the plain one does.
#[tokio::test]
async fn a_durable_question_fails_over_as_a_durable_question() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let owned = primary_is(&map, 1, 2);
    cluster.node(2).hold_as(owned[0], b"in ram on the replica");
    cluster.node(2).make_ram_only(&owned[0]);
    cluster.node(2).hold_as(owned[1], b"on disk on the replica");
    cluster.node(1).fail_next(unavailable());

    let missing = router.find_durable(&owned, false).await.expect("an answer");
    assert_eq!(missing, [owned[0]]);
    assert_eq!(cluster.node(2).durable_log(), [sorted(owned.clone())]);
    assert!(cluster.node(2).find_log().is_empty());

    cluster.node(1).set_down(true);
    cluster.node(2).set_down(true);
    let error = router
        .find_durable(&owned, false)
        .await
        .expect_err("nobody answers");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
}

/// Catches `Router::find_durable(.., true)` that does not carry the wait to the node
/// (`kbf-durable: wait`), or that carries it when `wait` is false: with `wait` the primary
/// is asked `find_durable_waiting` (and the RAM-only blob is still reported missing, as
/// the node's answer after its bound), without it `find_durable`; on a failover the
/// replica is asked the same waiting question. Mutant: `Ask::durable` ignores `wait`.
#[tokio::test]
async fn a_durable_question_carries_the_wait_to_the_node_it_asks() {
    let map = map(1, 2, active(&[1, 2]));
    let cluster = Cluster::of(2);
    let (router, _) = router(&map, &cluster);
    let owned = primary_is(&map, 1, 2);
    cluster.node(1).hold_as(owned[0], b"on disk");
    cluster.node(1).hold_as(owned[1], b"in ram");
    cluster.node(1).make_ram_only(&owned[1]);

    let missing = router.find_durable(&owned, true).await.expect("an answer");
    assert_eq!(missing, [owned[1]]);
    assert_eq!(cluster.node(1).waiting_log(), [sorted(owned.clone())]);
    assert!(cluster.node(1).durable_log().is_empty());

    router.find_durable(&owned, false).await.expect("an answer");
    assert_eq!(cluster.node(1).waiting_log().len(), 1, "no wait asked");
    assert_eq!(cluster.node(1).durable_log().len(), 1);

    cluster.node(1).fail_next(unavailable());
    cluster.node(2).hold_as(owned[0], b"on disk on the replica");
    cluster.node(2).hold_as(owned[1], b"on disk on the replica");
    router.find_durable(&owned, true).await.expect("an answer");
    assert_eq!(cluster.node(2).waiting_log(), [sorted(owned)]);
    assert!(cluster.node(2).durable_log().is_empty());
}
