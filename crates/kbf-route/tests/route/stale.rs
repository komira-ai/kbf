//! STALE_MAP: a node holds a newer map; the router fetches it and runs the call again.

use std::sync::Arc;

use bytes::Bytes;
use kbf_map::{Map, MemberId};
use kbf_meta::{ActionRecord, Closure, Role};
use kbf_route::{RefetchError, Router};
use kbf_storage::StorageError;
use kbf_types::Digest;

use super::support::{Cell, Cluster, active, blob_where, collect, map, router, sha, view};

fn stale() -> StorageError {
    StorageError::StaleMap { current: 2 }
}

/// Generation 1 has members 1 and 2; generation 2 has 3 and 4.
struct Farm {
    one: Arc<Map>,
    cluster: Arc<Cluster>,
    router: Router,
    cell: Arc<Cell>,
    /// A blob owned by 1 (then 2) in generation 1 and by 3 (then 4) in generation 2.
    blob: Vec<u8>,
    digest: Digest,
}

fn farm() -> Farm {
    let one = map(1, 2, active(&[1, 2]));
    let two = map(2, 2, active(&[3, 4]));
    let cluster = Cluster::of(4);
    let (router, cell) = router(&one, &cluster);
    let (blob, digest) = blob_where(|d| {
        one.write_owners(d) == [MemberId(1), MemberId(2)]
            && two.write_owners(d) == [MemberId(3), MemberId(4)]
            && one.read_order(d) == [MemberId(1), MemberId(2)]
            && two.read_order(d) == [MemberId(3), MemberId(4)]
    });
    cell.answer_fetch(Ok(view(&two)));
    Farm {
        one,
        cluster,
        router,
        cell,
        blob,
        digest,
    }
}

/// Catches a stale answer taken for a failure of the owner (the walk would go on to the
/// second owner of the old map, which is no better), a retry against the old map, and a
/// retry that is not tagged with the new generation: the first owner says its map is newer
/// mid-read, the router fetches generation 2 (once) and reads from 3, the new first owner;
/// nobody else of the old map is asked.
#[tokio::test]
async fn a_stale_answer_mid_fetch_refetches_and_reads_from_the_new_owner() {
    let f = farm();
    f.cluster.node(3).hold(&f.blob);
    f.cluster.node(1).fail_read(stale());

    let (bytes, _) = collect(f.router.read_range(&f.digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, f.blob);
    assert_eq!(f.cell.fetched(), [2]);
    assert_eq!(f.cluster.dialled(), [(1, 1), (3, 2)]);
    assert_eq!(f.cluster.node(2).read_count(), 0);
}

/// Catches the same through the shared whole-blob read.
#[tokio::test]
async fn a_whole_blob_read_survives_a_stale_answer() {
    let f = farm();
    f.cluster.node(3).hold(&f.blob);
    f.cluster.node(1).fail_read(stale());
    assert_eq!(
        f.router.read_blob(&f.digest).await.expect("the blob"),
        f.blob
    );
    assert_eq!(f.cell.fetched(), [2]);
}

/// Catches a router that gives up after one refetch (with promotions each bumping the
/// generation, a call spanning two of them would fail) and one that chases a map nobody
/// has: the node says stale again after the fetch, the router fetches again, and when no
/// newer map can be had the call is UNAVAILABLE with the fetch's reason.
#[tokio::test]
async fn a_second_stale_answer_fetches_again() {
    let f = farm();
    f.cluster.node(3).hold(&f.blob);
    f.cluster.node(1).fail_read(stale());
    f.cluster
        .node(3)
        .fail_read(StorageError::StaleMap { current: 3 });

    let error = f
        .router
        .read_range(&f.digest, 0, 0)
        .await
        .err()
        .expect("generation 3 cannot be had");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(
        error.to_string().contains("no frontend answered"),
        "{error}"
    );
    assert_eq!(f.cell.fetched(), [2, 3]);
}

/// Catches a write that fails when the map moves several times during it (each member's
/// promotion is a new generation): three promotions, each answered STALE_MAP by the owner,
/// three fetches, and the write lands against the newest map. Mutant: one refetch only.
#[tokio::test]
async fn a_write_across_three_promotions_lands() {
    let maps: Vec<Arc<Map>> = (1..=4).map(|g| map(g, 1, active(&[1]))).collect();
    let cluster = Cluster::of(1);
    let (router, cell) = router(&maps[0], &cluster);
    for (stale_at, next) in [(2, &maps[1]), (3, &maps[2]), (4, &maps[3])] {
        cluster
            .node(1)
            .fail_next(StorageError::StaleMap { current: stale_at });
        cell.answer_fetch(Ok(view(next)));
    }
    let digest = router
        .put(Bytes::from_static(b"across promotions"))
        .await
        .expect("written against generation 4");
    assert_eq!(digest, sha(b"across promotions"));
    assert_eq!(cell.fetched(), [2, 3, 4]);
    assert_eq!(cluster.dialled(), [(1, 1), (1, 2), (1, 3), (1, 4)]);
}

/// Catches a router that chases a map that never settles: a node that answers STALE_MAP
/// with a newer generation every time is followed for `MAX_MAP_REFETCHES` fetches, then
/// the call is UNAVAILABLE naming how often the map moved.
#[tokio::test]
async fn a_map_that_never_settles_is_followed_only_so_far() {
    let max = kbf_route::MAX_MAP_REFETCHES;
    let maps: Vec<Arc<Map>> = (1..=(max as u64 + 2))
        .map(|g| map(g, 1, active(&[1])))
        .collect();
    let cluster = Cluster::of(1);
    let (router, cell) = router(&maps[0], &cluster);
    for g in 2..=(max as u64 + 1) {
        cluster
            .node(1)
            .fail_next(StorageError::StaleMap { current: g });
        cell.answer_fetch(Ok(view(&maps[g as usize - 1])));
    }
    cluster.node(1).fail_next(StorageError::StaleMap {
        current: max as u64 + 2,
    });
    let error = router
        .put(Bytes::from_static(b"never settles"))
        .await
        .expect_err("the map kept moving");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(
        error.to_string().contains(&format!("moved {max} times")),
        "{error}"
    );
    assert_eq!(cell.fetched().len(), max);
}

/// Catches a retry that runs against the map it already used: the fetch brings the same
/// generation, which cannot help, and the router stops without asking again.
#[tokio::test]
async fn a_fetch_that_brings_no_newer_map_is_unavailable() {
    let f = farm();
    // The queued answer is generation 2; replace it with generation 1 again.
    let cell = Cell::with(view(&f.one));
    cell.answer_fetch(Ok(view(&f.one)));
    let router = Router::new(cell.clone(), f.cluster.clone());
    f.cluster.node(1).hold(&f.blob);
    f.cluster.node(1).fail_read(stale());

    let error = router
        .read_range(&f.digest, 0, 0)
        .await
        .err()
        .expect("no newer map");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(error.to_string().contains("the newest map is 1"), "{error}");
    assert_eq!(f.cluster.node(1).read_count(), 1);
}

/// Catches a failed fetch turned into something else: the reason reaches the caller as an
/// outage.
#[tokio::test]
async fn a_failed_fetch_is_unavailable_with_its_reason() {
    let f = farm();
    let cell = Cell::with(view(&f.one));
    cell.answer_fetch(Err(RefetchError {
        at_least: 2,
        reason: "no frontend answered".to_owned(),
    }));
    let router = Router::new(cell, f.cluster.clone());
    f.cluster.node(1).fail_read(stale());

    let error = router
        .read_range(&f.digest, 0, 0)
        .await
        .err()
        .expect("no fetch");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(
        error.to_string().contains("no frontend answered"),
        "{error}"
    );
}

/// Catches a read that breaks on a stale answer after its first byte and does not go on
/// at the new owner from the bytes passed: the old owner passes a chunk, then says its
/// map is newer, and the read goes on at 3 from byte 4.
#[tokio::test]
async fn a_stale_answer_after_the_first_byte_resumes_at_the_new_owner() {
    let f = farm();
    f.cluster.node(1).hold(&f.blob);
    f.cluster.node(3).hold(&f.blob);
    f.cluster.node(1).break_read(1, stale());
    f.cluster.node(1).fail_read(stale());

    let (bytes, _) = collect(f.router.read_range(&f.digest, 0, 0).await.expect("a read"))
        .await
        .expect("bytes");
    assert_eq!(bytes, f.blob);
    assert_eq!(f.cluster.node(3).read_log(), [(4, 0)]);
}

/// Catches an operation that is not run through the staleness handling: each of the
/// others gets one stale answer from the first owner of generation 1 and succeeds at the
/// first owner of generation 2 with one fetch.
#[tokio::test]
async fn every_call_handles_a_stale_answer() {
    // FindMissing.
    let f = farm();
    f.cluster.node(1).fail_next(stale());
    let missing = f.router.find_missing(&[f.digest]).await.expect("an answer");
    assert_eq!(missing, [f.digest]);
    assert_eq!(f.cluster.node(3).find_log(), [vec![f.digest]]);
    assert_eq!(f.cell.fetched(), [2]);

    // A write.
    let f = farm();
    f.cluster.node(1).fail_next(stale());
    f.router
        .put(Bytes::from(f.blob.clone()))
        .await
        .expect("stored");
    assert_eq!(f.cluster.node(3).put_log(), [f.digest]);
    assert_eq!(f.cell.fetched(), [2]);

    // An action-cache commit.
    let f = farm();
    f.cluster.node(1).fail_next(stale());
    let record = ActionRecord {
        result: sha(b"result"),
        closure: Closure::new(),
    };
    f.router
        .commit_action_record(Role::Daemon, f.digest, record)
        .await
        .expect("committed");
    assert_eq!(
        f.cluster.node(3).commits.lock().expect("lock").clone(),
        [f.digest]
    );
    assert_eq!(f.cell.fetched(), [2]);

    // An action-cache read.
    let f = farm();
    f.cluster.node(3).hold_action(f.digest, sha(b"result"));
    f.cluster.node(1).fail_next(stale());
    let found = f.router.action_result(&f.digest).await.expect("an answer");
    assert!(found.is_some());
    assert_eq!(f.cell.fetched(), [2]);
}
