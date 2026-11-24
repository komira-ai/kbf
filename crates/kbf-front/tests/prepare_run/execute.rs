//! One walk of an input tree per Execute: the Execute's input check, over the same backend as
//! the run's put-directory preparation, leaves that preparation every Directory it read (with
//! its names) and its finding that the Command and every file were held, so the preparation
//! reads no Directory and asks the store no FindMissing again. A check that finds an input
//! absent leaves no finding, and a finding older than the time a check is believed for (45
//! seconds) is checked again. The store records every blob read and every FindMissing call;
//! time is tokio's paused clock.

use std::collections::BTreeSet;

use kbf_auth::Authorizers;
use kbf_front::prepare_run::{DIRECTORY_MEMORY_BYTES, DirectoryCache, PutDirPrep, PutDirRun};
use kbf_front::{Closer, Dispatch, ExecutionService, Stage, Submission, Ticket};
use kbf_proto::reapi::ExecuteRequest;
use kbf_proto::reapi::execution_server::Execution as _;
use tokio::sync::watch;
use tonic::{Code, Status};

use super::*;

/// How long an Execute's finding is believed (`input_walk`'s `VERIFIED_TTL`).
const BELIEVED: Duration = Duration::from_secs(45);

/// A dispatch that takes every submission and runs nothing.
struct Taken;

impl Dispatch for Taken {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let key = submission.key;
        Ok(Ticket {
            name: "operations/1".to_owned(),
            instance: key.instance,
            action: key.action,
            stage: watch::channel(Stage::Queued).1,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }

    fn cancel(&self, _name: &str) -> bool {
        false
    }
}

/// An Execution service and a put-directory preparer with a cache of named Directories, over
/// one backend on `store`, as a frontend sets them up.
struct Frontend {
    service: ExecutionService<Taken>,
    prep: PutDirPrep,
    backend: Backend,
    _closer: Closer,
}

impl Frontend {
    fn over(store: &Arc<Watched>) -> Self {
        let storage: Arc<dyn Storage> = store.clone();
        let backend = Backend::remote(storage);
        let prep = PutDirPrep::new(
            backend.clone(),
            PackKeyMint::new(MINT_SECRET).expect("a mint"),
            SLACK,
            ClosureLimits::DEFAULT,
        )
        .with_directories(Arc::new(DirectoryCache::in_memory(DIRECTORY_MEMORY_BYTES)));
        let (closer, closing) = kbf_front::closing();
        let service = ExecutionService::with_authorizers(
            backend.clone(),
            Arc::new(Taken),
            closing,
            Arc::new(Authorizers::allow_all()),
        );
        Self {
            service,
            prep,
            backend,
            _closer: closer,
        }
    }

    /// Executes `action`, with no action-cache lookup: the code of a refusal, if refused.
    async fn execute(&self, action: &Digest) -> Result<(), Code> {
        let request = ExecuteRequest {
            action_digest: Some(wire(action)),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        };
        let answered = self.service.execute(tonic::Request::new(request)).await;
        answered.map(drop).map_err(|status| status.code())
    }

    /// The run of `action` prepared, as the executor asks for it once the Execute submitted it.
    async fn prepare(&self, action: &Digest) -> Result<PutDirRun, PrepareError> {
        self.prep
            .prepare_put_dir(LEASE, action, None, now(), WALL)
            .await
    }

    /// The counter `name` on the backend's metrics page.
    fn counted(&self, name: &str) -> u64 {
        let page = self.backend.render_metrics();
        let prefix = format!("{name} ");
        let line = page.lines().find_map(|line| line.strip_prefix(&prefix));
        line.unwrap_or_else(|| panic!("no {name} in {page}"))
            .parse()
            .expect("a count")
    }
}

/// How many times each Directory of the standard tree was read from the store.
fn directory_reads(parts: &Parts, store: &Watched) -> Vec<usize> {
    let reads = store.reads();
    let tree = &parts.tree;
    [tree.root, tree.lib, tree.lib2, tree.deep]
        .iter()
        .map(|dir| reads.iter().filter(|read| *read == dir).count())
        .collect()
}

/// Asserts that one Execute and the preparation of its run make one FindMissing pass over the
/// files and read each Directory once, and that the preparation answers the same closure: the
/// Execute reads the four Directories and asks once; the preparation takes them all from its
/// cache and asks nothing. Catches a preparation that walks the store again (each Directory read
/// twice) or asks FindMissing again (two calls).
#[tokio::test(start_paused = true)]
async fn an_execute_and_its_run_read_each_directory_once_and_ask_once() {
    let (parts, store, _) = standard().await;
    let frontend = Frontend::over(&store);
    frontend.execute(&parts.action).await.expect("submitted");
    assert_eq!(store.asks.load(SeqCst), 1, "the Execute's check");
    let run = frontend.prepare(&parts.action).await.expect("prepared");

    assert_eq!(directory_reads(&parts, &store), vec![1, 1, 1, 1]);
    assert_eq!(store.asks.load(SeqCst), 1, "no second FindMissing");
    let inputs: BTreeSet<Digest> = run.kept.closure().inputs().copied().collect();
    assert_eq!(inputs, parts.tree.expected);
    assert_eq!(run.kept.directory_count(), 4);
    assert_eq!(
        [
            frontend.counted("kbf_prepare_dirs_cached_total"),
            frontend.counted("kbf_prepare_dirs_read_total"),
            frontend.counted("kbf_prepare_held_vouched_total"),
            frontend.counted("kbf_prepare_held_checked_total"),
        ],
        [4, 0, 1, 0]
    );
}

/// Asserts that an input absent from the store still fails the Execute FAILED_PRECONDITION at
/// its own check, and that such a check vouches for nothing: a preparation of the same Action
/// asks the store and finds the file missing. Catches a finding left by a check that found
/// something absent.
#[tokio::test(start_paused = true)]
async fn an_execute_missing_an_input_is_refused_and_vouches_for_nothing() {
    let c = Parts::new().tree.c;
    let (parts, store, _) = missing(&[c]).await;
    let frontend = Frontend::over(&store);
    let refused = frontend.execute(&parts.action).await;
    assert_eq!(refused, Err(Code::FailedPrecondition));

    let prepared = frontend.prepare(&parts.action).await;
    assert!(
        matches!(
            &prepared,
            Err(PrepareError::Closure(ClosureError::Missing { what: "a file", digest })) if *digest == c
        ),
        "{prepared:?}"
    );
    assert_eq!(
        store.asks.load(SeqCst),
        2,
        "the Execute's and the preparation's"
    );
    assert_eq!(frontend.counted("kbf_prepare_held_checked_total"), 1);
}

/// Asserts the boundary of the time a finding is believed for: a preparation a second inside it
/// asks nothing, and one at its end asks the store again. Catches a finding believed for ever,
/// or one not believed at all.
#[tokio::test(start_paused = true)]
async fn a_finding_older_than_the_time_it_is_believed_for_is_checked_again() {
    let (parts, store, _) = standard().await;
    let frontend = Frontend::over(&store);
    frontend.execute(&parts.action).await.expect("submitted");
    tokio::time::advance(BELIEVED - Duration::from_secs(1)).await;
    frontend.prepare(&parts.action).await.expect("prepared");
    assert_eq!(store.asks.load(SeqCst), 1, "believed");

    frontend.execute(&parts.action).await.expect("submitted");
    assert_eq!(store.asks.load(SeqCst), 2);
    tokio::time::advance(BELIEVED).await;
    frontend.prepare(&parts.action).await.expect("prepared");
    assert_eq!(store.asks.load(SeqCst), 3, "checked again");
    assert_eq!(
        [
            frontend.counted("kbf_prepare_held_vouched_total"),
            frontend.counted("kbf_prepare_held_checked_total"),
        ],
        [1, 1]
    );
}

/// Asserts that over the cache in this process, whose input check is its own and keeps no
/// finding, an Execute leaves the preparation nothing: the preparation reads its four
/// Directories and asks the store itself. Catches a finding or a Directory taken from a check
/// that never made one.
#[tokio::test]
async fn over_the_in_process_cache_the_preparation_walks_and_asks_for_itself() {
    let parts = Parts::new();
    let cache = Arc::new(Cache::memory());
    let storage = CacheStorage::new(Arc::clone(&cache), SegmentsConfig::DEFAULT);
    for (_, bytes) in parts.blobs.clone() {
        storage.put(bytes).await.expect("the blob is stored");
    }
    let backend = Backend::from(cache);
    let prep = PutDirPrep::new(
        backend.clone(),
        PackKeyMint::new(MINT_SECRET).expect("a mint"),
        SLACK,
        ClosureLimits::DEFAULT,
    )
    .with_directories(Arc::new(DirectoryCache::in_memory(DIRECTORY_MEMORY_BYTES)));
    let (closer, closing) = kbf_front::closing();
    let service = ExecutionService::with_authorizers(
        backend.clone(),
        Arc::new(Taken),
        closing,
        Arc::new(Authorizers::allow_all()),
    );
    let frontend = Frontend {
        service,
        prep,
        backend,
        _closer: closer,
    };
    frontend.execute(&parts.action).await.expect("submitted");
    let run = frontend.prepare(&parts.action).await.expect("prepared");
    let inputs: BTreeSet<Digest> = run.kept.closure().inputs().copied().collect();
    assert_eq!(inputs, parts.tree.expected);
    assert_eq!(
        [
            frontend.counted("kbf_prepare_dirs_cached_total"),
            frontend.counted("kbf_prepare_dirs_read_total"),
            frontend.counted("kbf_prepare_held_vouched_total"),
            frontend.counted("kbf_prepare_held_checked_total"),
        ],
        [0, 4, 0, 1]
    );
}
