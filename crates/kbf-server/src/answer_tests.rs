//! What an OK result is answered while its outputs cannot be checked because the store
//! cannot answer, as when every owner of an output is down ([`crate::answer::outcome`]).
//! The clocks are tokio's, paused: a wait is virtual time that passes when nothing else
//! can run, and no test reads a clock.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{Backend, Cache, CacheStorage, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::{google::rpc, worker};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::{Digest, Failure, LeaseId, Outcome, WorkerId};
use tonic::Code;

use crate::answer::{Detail, Pause, outcome_pausing};

/// The pause between two asks, spelled out so that a change to the constant is a change
/// to this test: twelve pauses of five seconds a minute, ten minutes.
const FIVE_SECONDS: Duration = Duration::from_secs(5);
const TEN_MINUTES_OF_PAUSES: usize = 120;
use crate::stamp::Stamp;

/// The error `kbf-route` gives while every owner of an output is down.
#[derive(Debug, thiserror::Error)]
#[error("every owner of {0} is down")]
struct AllDown(&'static str);

/// A store over an empty in-memory cache that cannot answer an OK result's check for its
/// first `failures` asks, as a storage plane does while every owner is down, and answers
/// as the cache does afterwards. `asks` counts the checks asked.
pub(crate) struct Flaky {
    inner: CacheStorage<kbf_front::MemoryMetaLog, kbf_objstore::MemoryStore>,
    failures: AtomicU32,
    asks: AtomicU32,
    error: fn() -> StorageError,
}

fn unavailable() -> StorageError {
    StorageError::Unavailable(Arc::new(AllDown("a1b2")))
}

fn not_found() -> StorageError {
    StorageError::Internal("an output that was never stored".to_owned())
}

impl Flaky {
    /// A store that cannot answer `failures` times with an unavailable error.
    pub(crate) fn down_for(failures: u32) -> Arc<Self> {
        Self::failing_with(failures, unavailable)
    }

    fn failing_with(failures: u32, error: fn() -> StorageError) -> Arc<Self> {
        Arc::new(Self {
            inner: CacheStorage::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT),
            failures: AtomicU32::new(failures),
            asks: AtomicU32::new(0),
            error,
        })
    }

    /// How many times the check was asked.
    pub(crate) fn asks(&self) -> u32 {
        self.asks.load(Ordering::SeqCst)
    }

    /// Whether the store holds an action-cache entry for `action`.
    pub(crate) async fn holds(&self, action: &Digest) -> bool {
        self.inner
            .action_result(action)
            .await
            .expect("the cache answers")
            .is_some()
    }
}

impl Storage for Flaky {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_missing(digests)
    }
    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable(digests)
    }
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }
    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.inner.read_range(digest, offset, limit)
    }
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }
    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }
    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(action)
    }
    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.asks.fetch_add(1, Ordering::SeqCst);
        let down = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if down {
            let error = (self.error)();
            return Box::pin(async move { Err(error) });
        }
        self.inner.prepare_action_result(result)
    }
    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// An OK result of `lease` with no outputs.
pub(crate) fn ok_result(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(rpc::Status {
            code: Code::Ok as i32,
            message: String::new(),
            details: Vec::new(),
        }),
        action_result: Some(ActionResult::default()),
        action_digest: None,
        memory_kill: 0,
        lost: 0,
        job_outcome: None,
    }
}

/// A pause that returns at once and keeps each duration it was asked for.
#[derive(Default)]
struct Recorded(std::sync::Mutex<Vec<Duration>>);

impl Pause for Recorded {
    fn pause(&self, for_how_long: Duration) -> impl Future<Output = ()> + Send {
        self.0.lock().expect("lock").push(for_how_long);
        std::future::ready(())
    }
}

/// The OK result of a lease checked against `store`, and the pauses it waited with.
async fn checked(store: &Arc<Flaky>) -> (Outcome, Option<Detail>, Vec<Duration>) {
    let cache = Backend::remote(Arc::clone(store) as Arc<dyn Storage>);
    let node = WorkerId::new("node-a");
    let lease = LeaseId::new(7, 1);
    let pauses = Recorded::default();
    let (outcome, detail) = outcome_pausing(
        &cache,
        &pauses,
        std::time::SystemTime::now,
        lease,
        (
            &node,
            Stamp {
                queued: std::time::UNIX_EPOCH,
                started: std::time::UNIX_EPOCH,
            },
        ),
        ok_result(lease),
    )
    .await;
    let waited = pauses.0.lock().expect("lock").clone();
    (outcome, detail, waited)
}

/// Catches an OK result whose check meets a store that cannot answer being failed as an
/// infrastructure failure at once, which the farm charges to the operation's reruns: the
/// store is down for more asks than the farm has reruns, and the result is still
/// accepted, with its record, once the store answers.
#[tokio::test(start_paused = true)]
async fn an_ok_result_waits_for_a_store_that_cannot_answer() {
    let down = 3 * (1 + kbf_sched::FARM_RERUNS);
    let store = Flaky::down_for(down);

    let (outcome, detail, pauses) = checked(&store).await;

    assert!(matches!(outcome, Outcome::Completed { .. }), "{outcome:?}");
    assert!(matches!(detail, Some(Detail::Ran(_))), "{detail:?}");
    assert_eq!(store.asks(), down + 1, "asked until the store answered");
    assert_eq!(
        pauses,
        vec![FIVE_SECONDS; down as usize],
        "one pause per refusal"
    );
}

/// Catches a wait with no end: a store that never answers ends the result as the
/// infrastructure failure it was before, with the store's words, after the asks the wait
/// allows and no more.
#[tokio::test(start_paused = true)]
async fn a_store_that_never_answers_ends_the_wait_as_an_infra_failure() {
    let store = Flaky::down_for(u32::MAX);

    let (outcome, detail, pauses) = checked(&store).await;

    assert_eq!(outcome, Outcome::Failed(Failure::Infra));
    assert_eq!(
        pauses,
        vec![FIVE_SECONDS; TEN_MINUTES_OF_PAUSES],
        "ten minutes of five-second pauses"
    );
    let Some(Detail::Infra(why)) = detail else {
        panic!("no infrastructure detail: {detail:?}");
    };
    assert!(why.contains("every owner of a1b2 is down"), "{why}");
    assert_eq!(store.asks(), 1 + TEN_MINUTES_OF_PAUSES as u32);
}

/// Catches an error that is not an outage being waited for: a store that fails the check
/// in any other way answers once, and the result fails at once as before.
#[tokio::test(start_paused = true)]
async fn an_error_that_is_no_outage_is_not_waited_for() {
    let store = Flaky::failing_with(u32::MAX, not_found);

    let (outcome, detail, pauses) = checked(&store).await;

    assert_eq!(outcome, Outcome::Failed(Failure::Infra));
    assert!(matches!(detail, Some(Detail::Infra(_))), "{detail:?}");
    assert_eq!(store.asks(), 1);
    assert!(pauses.is_empty(), "{pauses:?}");
}

/// Catches a wait that accepts the result but hands on a record the store cannot commit:
/// the record of a result that waited is committed, and the action-cache entry is there.
#[tokio::test(start_paused = true)]
async fn the_record_of_a_result_that_waited_is_committed() {
    let store = Flaky::down_for(2);
    let (_, detail, _) = checked(&store).await;
    let Some(Detail::Ran(pending)) = detail else {
        panic!("not accepted: {detail:?}");
    };
    let action = Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 9);

    store
        .commit_action_record(Role::Daemon, action, pending.record)
        .await
        .expect("committed");

    assert!(store.holds(&action).await);
}
