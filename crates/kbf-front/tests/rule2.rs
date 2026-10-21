//! Rule 2 of `docs/design/storage-ram-ack.md`, on real caches: a durable record that names
//! a blob is written only once the blob is on a disk (the index) of an owner. Nothing puts
//! a blob in the Held set in a running node yet; every test puts one there through
//! `Cache::hold_for_test`, and waits for the barrier to park through
//! `Cache::parked_for_test` (a positive event), never by sleeping or polling. The time the
//! barrier waits is the paused clock's.
//!
//! The router tests use a farm of three caches, members 1 to 3, two owners per blob. The
//! action's owner is member 3, and the blobs live on members 1 (primary) and 2.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::{Cache, CacheError, CacheStorage, MemoryMetaLog, SegmentsConfig, VerifiedBlob};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::{ActionRecord, ActionWriteError, Closure, Role};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_route::{MapSource, MapView, Nodes, RefetchError, Router};
use kbf_storage::service::{Admission, Config, RemoteStorage, StorageService};
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;
use prost::Message;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Endpoint, Server};

type MemCache = Cache<MemoryMetaLog, MemoryStore>;

const WAIT: Duration = Duration::from_secs(7);

/// A blob and its bytes.
#[derive(Clone)]
struct Blob {
    verified: VerifiedBlob,
}

impl Blob {
    fn digest(&self) -> Digest {
        self.verified.digest()
    }
}

/// The blob of an `ActionResult` (one a cache can serve an entry's hit from).
fn result_blob(exit_code: i32) -> Blob {
    blob_of(
        ActionResult {
            exit_code,
            ..ActionResult::default()
        }
        .encode_to_vec(),
    )
}

fn blob_of(bytes: Vec<u8>) -> Blob {
    Blob {
        verified: VerifiedBlob::hashed(Bytes::from(bytes)),
    }
}

/// A cache that waits `WAIT` for held blobs.
fn cache() -> Arc<MemCache> {
    Arc::new(Cache::memory().with_durable_wait(WAIT))
}

/// Commits `blob` to the index of `cache`: it is durable there.
async fn write(cache: &MemCache, blob: &Blob) {
    cache
        .store_blobs(vec![blob.verified.clone()])
        .await
        .expect("stored");
}

/// The drain: the index takes the blob, then the set drops it.
async fn drain(cache: &MemCache, blob: &Blob) {
    write(cache, blob).await;
    assert!(cache.release_for_test(&blob.digest()), "it was held");
}

fn absent() -> Digest {
    kbf_segments::sha256(b"nowhere")
}

// ---- a cache alone ----

/// Catches a durable answer that waits for nothing, one that waits when nothing is held,
/// and one that answers before the blob is in the index: nothing held answers at once
/// without parking; a held blob parks the wait, and when the drain has put it in the index
/// and dropped it the answer is "durable". Mutants: `durable_waiting` is `durable` (the
/// held blob is reported missing without parking); the wait returns before the index has
/// the blob.
#[tokio::test]
async fn a_wait_for_a_held_blob_ends_when_it_is_in_the_index() {
    let cache = cache();
    let held = blob_of(b"held in ram".to_vec());
    assert_eq!(
        cache.durable_waiting(&[absent()]).await.expect("answer"),
        [absent()],
        "nothing held: answered at once"
    );
    assert!(cache.hold_for_test(&held.verified));

    let waiting = tokio::spawn({
        let cache = Arc::clone(&cache);
        let digest = held.digest();
        async move { cache.durable_waiting(&[digest]).await }
    });
    cache.parked_for_test(1).await;
    drain(&cache, &held).await;
    assert_eq!(waiting.await.expect("joined").expect("answer"), []);
}

/// Catches a wait that ends for the wrong blob, or never ends: two blobs are held; the wait
/// is for the first; the second leaving does not end it (it parks again), the first does.
/// Mutant: the wait ends when any entry leaves.
#[tokio::test]
async fn a_wait_ends_only_when_its_own_blobs_have_left() {
    let cache = cache();
    let (mine, other) = (blob_of(b"mine".to_vec()), blob_of(b"another".to_vec()));
    assert!(cache.hold_for_test(&mine.verified));
    assert!(cache.hold_for_test(&other.verified));
    let waiting = tokio::spawn({
        let cache = Arc::clone(&cache);
        let digest = mine.digest();
        async move { cache.durable_waiting(&[digest]).await }
    });
    cache.parked_for_test(1).await;
    drain(&cache, &other).await;
    cache.parked_for_test(2).await;
    drain(&cache, &mine).await;
    assert_eq!(waiting.await.expect("joined").expect("answer"), []);
}

/// Catches a wait with no bound or the wrong one: a blob that never leaves is reported
/// missing when `--durable-wait` (here 7 s, not the 10 s default) has passed on the paused
/// clock, and not before; the answer then comes at once, without more time passing.
/// Mutants: the bound ignored (the default is used: the wait is still parked at 7 s and
/// the check below times out); no bound (it never ends).
#[tokio::test(start_paused = true)]
async fn a_blob_still_held_when_the_wait_ends_is_reported_missing() {
    let cache = cache();
    let held = blob_of(b"never written".to_vec());
    assert!(cache.hold_for_test(&held.verified));
    let waiting = tokio::spawn({
        let cache = Arc::clone(&cache);
        let digest = held.digest();
        async move { cache.durable_waiting(&[digest]).await }
    });
    cache.parked_for_test(1).await;
    tokio::time::advance(WAIT - Duration::from_millis(1)).await;
    assert!(!waiting.is_finished(), "inside the bound");
    tokio::time::advance(Duration::from_millis(1)).await;
    let answer = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("answered at the bound")
        .expect("joined")
        .expect("answer");
    assert_eq!(answer, [held.digest()]);
}

// ---- barrier (b): the action's owner ----

/// An `ActionResult` whose one output file is `blob`.
fn result_of(blob: &Blob) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".into(),
            digest: Some(kbf_proto::reapi::Digest {
                hash: blob.digest().hash_hex(),
                size_bytes: blob.digest().size_bytes as i64,
            }),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    }
}

/// Catches an action-cache write on the action's owner that does not wait for a blob the
/// node holds only in RAM (barrier b: the log checks the index, so it would refuse a blob
/// that is on its way): `prepare_action_result` parks until the drain, then returns a
/// record naming the blob, and does not refuse. Mutant: `prepare_action_result` does not
/// wait (it refuses `Absent` without parking).
#[tokio::test]
async fn preparing_an_entry_waits_for_a_closure_blob_held_in_ram() {
    let cache = cache();
    let output = blob_of(b"an output".to_vec());
    assert!(cache.hold_for_test(&output.verified));
    let result = result_of(&output);

    let mut preparing = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { cache.prepare_action_result(&result).await }
    });
    tokio::select! {
        () = cache.parked_for_test(1) => {}
        done = &mut preparing => panic!("prepared without waiting for the blob: {done:?}"),
    }
    drain(&cache, &output).await;
    let record = preparing.await.expect("joined").expect("prepared");
    assert!(record.closure.iter().any(|d| *d == output.digest()));
}

/// Catches the commit of an entry on the action's owner not waiting, and a refusal that
/// is not the node's own after the bound: the blob is held in RAM and in the index of
/// another cache only; `commit_action_record` parks; when the drain has run the entry
/// commits; where nothing drains, it is refused `Absent` when `--durable-wait` has passed
/// (the unchanged check). Mutant: `commit_action_record` does not wait.
#[tokio::test(start_paused = true)]
async fn committing_an_entry_waits_for_its_blobs_to_be_written() {
    let cache = cache();
    let (result, output) = (result_blob(1), blob_of(b"output".to_vec()));
    write(&cache, &result).await;
    assert!(cache.hold_for_test(&output.verified));
    let record = ActionRecord {
        result: result.digest(),
        closure: [output.digest()].into_iter().collect::<Closure>(),
    };
    let action = kbf_segments::sha256(b"an action");

    let mut committing = tokio::spawn({
        let (cache, record) = (Arc::clone(&cache), record.clone());
        async move {
            cache
                .commit_action_record(Role::Daemon, action, record)
                .await
        }
    });
    tokio::select! {
        () = cache.parked_for_test(1) => {}
        done = &mut committing => panic!("committed without waiting for the blob: {done:?}"),
    }
    drain(&cache, &output).await;
    committing.await.expect("joined").expect("committed");
    assert!(cache.action_entry(&action).await.expect("asked").is_some());

    // Nothing drains this one.
    let stuck = blob_of(b"stuck in ram".to_vec());
    assert!(cache.hold_for_test(&stuck.verified));
    let record = ActionRecord {
        result: result.digest(),
        closure: [stuck.digest()].into_iter().collect::<Closure>(),
    };
    let other = kbf_segments::sha256(b"another action");
    let mut committing = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move {
            cache
                .commit_action_record(Role::Daemon, other, record)
                .await
        }
    });
    cache.parked_for_test(2).await;
    tokio::time::advance(WAIT).await;
    let refused = (&mut committing).await.expect("joined");
    assert!(
        matches!(&refused, Err(CacheError::ActionWrite(ActionWriteError::Absent(d))) if d == &[stuck.digest()]),
        "{refused:?}"
    );
    assert!(cache.action_entry(&other).await.expect("asked").is_none());
}

// ---- barrier (a) and the closure check: the router over three caches ----

struct Farm {
    caches: Vec<Arc<MemCache>>,
    map: Arc<Map>,
    router: Arc<Router>,
}

struct Members(BTreeMap<MemberId, Arc<dyn Storage>>);

impl Nodes for Members {
    fn node(&self, member: &Member, _generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::clone(&self.0[&member.id]))
    }
}

struct Fixed(MapView);

impl MapSource for Fixed {
    fn current(&self) -> Option<MapView> {
        Some(self.0.clone())
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "the map is fixed".into(),
            })
        })
    }
}

fn farm() -> Farm {
    let caches: Vec<Arc<MemCache>> = (0..3).map(|_| cache()).collect();
    let members: Vec<Member> = (1..=3)
        .map(|id| Member {
            id: MemberId(id),
            addr: format!("node-{id}:7000"),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
            boot: 1,
        })
        .collect();
    let map = Arc::new(Map::new(1, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("a map"));
    let nodes: BTreeMap<MemberId, Arc<dyn Storage>> = caches
        .iter()
        .enumerate()
        .map(|(i, cache)| {
            (
                MemberId(i as u64 + 1),
                Arc::new(CacheStorage::new(
                    Arc::clone(cache),
                    SegmentsConfig::DEFAULT,
                )) as Arc<dyn Storage>,
            )
        })
        .collect();
    let view = MapView {
        map: Arc::clone(&map),
        previous: None,
        earlier: Vec::new(),
    };
    let router = Arc::new(Router::new(Arc::new(Fixed(view)), Arc::new(Members(nodes))));
    Farm {
        caches,
        map,
        router,
    }
}

impl Farm {
    /// `count` blobs whose write owners are members `order`, primary first, made by
    /// `make` from 0, 1, 2 and so on.
    fn blobs_by(&self, order: &[u64], count: usize, make: impl Fn(i32) -> Blob) -> Vec<Blob> {
        let blobs: Vec<Blob> = (0_i32..100_000)
            .map(make)
            .filter(|b| {
                let owners: Vec<u64> = self
                    .map
                    .write_owners(&b.digest())
                    .iter()
                    .map(|m| m.0)
                    .collect();
                owners == order
            })
            .take(count)
            .collect();
        assert_eq!(blobs.len(), count, "blobs that fit");
        blobs
    }

    /// `count` blobs of arbitrary bytes owned by members `order`.
    fn blobs_written_to(&self, order: &[u64], count: usize) -> Vec<Blob> {
        self.blobs_by(order, count, |i| {
            blob_of(format!("rule 2 blob {i}").into_bytes())
        })
    }

    /// An action whose owner is member 3.
    fn action(&self) -> Digest {
        self.blobs_written_to(&[3, 1], 1)[0].digest()
    }

    /// The result blob and one output of an entry, both owned by members 1 and 2, the
    /// result on both disks.
    async fn entry(&self) -> (Blob, Blob) {
        let result = self.blobs_by(&[1, 2], 1, result_blob).remove(0);
        let output = self.blobs_written_to(&[1, 2], 1).remove(0);
        for cache in &self.caches[..2] {
            write(cache, &result).await;
        }
        (result, output)
    }

    fn commit(&self, action: Digest, result: &Blob, output: &Blob) -> CommitTask {
        let router = Arc::clone(&self.router);
        let record = ActionRecord {
            result: result.digest(),
            closure: [output.digest()].into_iter().collect::<Closure>(),
        };
        tokio::spawn(async move {
            router
                .commit_with_closure(Role::Daemon, action, record)
                .await
        })
    }
}

type CommitTask = tokio::task::JoinHandle<Result<(), StorageError>>;

/// Catches barrier (a) missing, or not waiting (T3's positive twin): the output of an entry
/// is held in RAM on its primary and nowhere else on a disk. The commit parks at the
/// primary (the copy of the output to the action's owner went through: it is a durable
/// non-owner copy, which is not enough), no entry exists while it is parked, and when the
/// primary has written the blob the commit completes and the entry is found. Mutants:
/// barrier (a) deleted (the commit finishes while the blob is only in RAM); the barrier
/// asks without waiting (it is refused at once).
#[tokio::test]
async fn an_entry_commits_only_after_its_blob_is_on_an_owners_disk() {
    let farm = farm();
    let action = farm.action();
    let (result, output) = farm.entry().await;
    assert!(farm.caches[0].hold_for_test(&output.verified));

    let mut committing = farm.commit(action, &result, &output);
    tokio::select! {
        () = farm.caches[0].parked_for_test(1) => {}
        done = &mut committing => panic!("committed before the blob was on an owner's disk: {done:?}"),
    }
    assert!(
        farm.caches[2]
            .action_entry(&action)
            .await
            .expect("asked")
            .is_none(),
        "no entry while the barrier waits"
    );
    drain(&farm.caches[0], &output).await;
    committing.await.expect("joined").expect("committed");
    assert!(
        farm.caches[2]
            .action_entry(&action)
            .await
            .expect("asked")
            .is_some()
    );
    assert!(
        farm.caches[0]
            .durable(&[output.digest()])
            .await
            .expect("asked")
            .is_empty()
    );
}

/// Catches an entry committed over a blob that never reaches a disk (T3: the owners lose
/// their RAM): after the bound the commit is UNAVAILABLE (not `Ok`, not `Absent`: an owner
/// holds the blob, and the caller can try again) and no entry exists. Mutant: the barrier
/// deleted; the failure reported as `Absent`.
#[tokio::test(start_paused = true)]
async fn an_entry_over_a_blob_that_stays_in_ram_is_refused_unavailable() {
    let farm = farm();
    let action = farm.action();
    let (result, output) = farm.entry().await;
    assert!(farm.caches[0].hold_for_test(&output.verified));

    let mut committing = farm.commit(action, &result, &output);
    tokio::select! {
        () = farm.caches[0].parked_for_test(1) => {}
        done = &mut committing => panic!("committed before the blob was on an owner's disk: {done:?}"),
    }
    tokio::time::advance(WAIT).await;
    let refused = committing.await.expect("joined");
    assert!(
        matches!(refused, Err(StorageError::Unavailable(_))),
        "{refused:?}"
    );
    assert!(
        farm.caches[2]
            .action_entry(&action)
            .await
            .expect("asked")
            .is_none()
    );
}

/// Catches a barrier that counts the action's owner's copy: the output is on the disk of
/// member 3 only, which does not own it (a copy no reader asks for). The commit is refused
/// `Absent` naming it, and no entry exists. Mutant: barrier (a) deleted.
#[tokio::test]
async fn a_copy_only_on_the_actions_owner_is_not_enough() {
    let farm = farm();
    let action = farm.action();
    let (result, output) = farm.entry().await;
    write(&farm.caches[2], &output).await;

    let refused = farm.commit(action, &result, &output).await.expect("joined");
    assert!(
        matches!(&refused, Err(StorageError::ActionWrite(ActionWriteError::Absent(d))) if d == &[output.digest()]),
        "{refused:?}"
    );
    assert!(
        farm.caches[2]
            .action_entry(&action)
            .await
            .expect("asked")
            .is_none()
    );
}

/// Catches a closure check that takes a RAM-held blob for present (T11): the entry's
/// output is on the disk of the non-owner action node and held in RAM by its primary. The
/// read is a miss, never a hit, and nothing about the held blob is remembered: once the
/// primary has the blob in its index the same entry is a hit with no wait for a time to
/// live. Mutant: the closure check asks `find_missing` (the first read is a hit).
#[tokio::test]
async fn a_closure_blob_held_in_ram_makes_the_entry_a_miss() {
    let farm = farm();
    let action = farm.action();
    let (result, output) = farm.entry().await;
    // The entry is on the action's owner: its blobs are there, as copies.
    write(&farm.caches[2], &result).await;
    write(&farm.caches[2], &output).await;
    farm.caches[2]
        .commit_action_record(
            Role::Daemon,
            action,
            ActionRecord {
                result: result.digest(),
                closure: [output.digest()].into_iter().collect::<Closure>(),
            },
        )
        .await
        .expect("the entry");
    assert!(farm.caches[0].hold_for_test(&output.verified));

    let missed = farm.router.action_result(&action).await.expect("answered");
    assert!(
        missed.is_none(),
        "a blob held in RAM is not one an entry rests on"
    );
    assert_eq!(farm.router.closure_stats().cache_hits, 0);

    write(&farm.caches[0], &output).await;
    let hit = farm.router.action_result(&action).await.expect("answered");
    assert!(
        hit.is_some(),
        "on a disk: a hit, nothing remembered the miss"
    );
}

// ---- over the wire ----

/// A storage node over `cache`, and a client of it.
fn remote(cache: &Arc<MemCache>) -> RemoteStorage {
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::clone(cache),
        SegmentsConfig::DEFAULT,
    ));
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight: 64 << 20,
        },
    );
    let incoming = TcpIncoming::bind(([127, 0, 0, 1], 0).into()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(service.routes())
            .serve_with_incoming(incoming),
    );
    RemoteStorage::new(
        Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect_lazy(),
        0,
    )
}

/// Catches `kbf-durable: wait` not reaching the cache's wait: over the wire the node parks
/// for a held blob and answers from the index once the drain has run; the `only` question
/// asked beside it answers at once, naming the blob missing. Mutant: `CacheStorage` does not
/// override `find_durable_waiting` (the default answers without waiting).
#[tokio::test]
async fn the_wait_header_makes_a_node_wait_for_its_held_blobs() {
    let cache = cache();
    let held = blob_of(b"held over the wire".to_vec());
    assert!(cache.hold_for_test(&held.verified));
    let remote = Arc::new(remote(&cache));

    assert_eq!(
        remote.find_durable(&[held.digest()]).await.expect("answer"),
        [held.digest()],
        "`only` does not wait"
    );
    let asking = tokio::spawn({
        let remote = Arc::clone(&remote);
        let digest = held.digest();
        async move { remote.find_durable_waiting(&[digest]).await }
    });
    cache.parked_for_test(1).await;
    drain(&cache, &held).await;
    assert_eq!(asking.await.expect("joined").expect("answer"), []);
}
