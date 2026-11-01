//! `prepare_run`: the key, the presigned write and the read grant of one run, and the input
//! closure the grant is for.
//!
//! The store is a real in-memory storage node behind a wrapper that records every blob read
//! and every FindMissing call and fails on request. Time is injected, the secrets are fixed,
//! and no test waits or reads the wall clock. Every test says the behaviour it asserts.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_front::grants::{ClaimsError, GrantError, GrantKey, Grants};
use kbf_front::prepare_run::{
    AccessKey, ClosureError, ClosureLimits, ConfigError, ExpiryError, MAX_EXPIRY, PrepareError,
    PrepareFault, PutTarget, RunPrep, RunPrepConfig,
};
use kbf_front::{Backend, Cache, CacheStorage, PackKeyMint, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::{self, ActionResult, Directory, DirectoryNode, FileNode};
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tierstore::server::{PresignRequest, presign_put};

#[path = "prepare_run/faults.rs"]
mod faults;
#[path = "prepare_run/keyset.rs"]
mod keyset;
#[path = "prepare_run/start.rs"]
mod start;

const LEASE: &str = "00000000000000010000000000000002";
const MINT_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const GRANT_SECRET: &[u8] = b"fedcba9876543210fedcba9876543210";
const KEY_ID: &str = "pack-presign";
const KEY_SECRET: &str = "the presign secret";
const ENDPOINT: &str = "http://127.0.0.1:9000";
const BUCKET: &str = "tiers";
const REGION: &str = "us-east-1";
const READ_URL: &str = "https://127.0.0.1:8443";
const WALL: Duration = Duration::from_secs(7_200);
const SLACK: Duration = Duration::from_secs(600);
/// A minute boundary: 29833334 minutes after the epoch.
const NOW_SECS: u64 = 1_790_000_040;

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW_SECS)
}

fn wire(digest: &Digest) -> reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

fn empty() -> Digest {
    kbf_segments::sha256(&[])
}

fn blob(text: &str) -> (Digest, Bytes) {
    let bytes = Bytes::copy_from_slice(text.as_bytes());
    (kbf_segments::sha256(&bytes), bytes)
}

/// The Directory that names `files` and `dirs`, and its digest.
fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
    let message = Directory {
        files: files
            .iter()
            .enumerate()
            .map(|(at, digest)| FileNode {
                name: format!("f{at}"),
                digest: Some(wire(digest)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .enumerate()
            .map(|(at, digest)| DirectoryNode {
                name: format!("d{at}"),
                digest: Some(wire(digest)),
            })
            .collect(),
        ..Directory::default()
    };
    let bytes = Bytes::from(message.encode_to_vec());
    (kbf_segments::sha256(&bytes), bytes)
}

/// The Action that names `command` and `root`, and its digest.
fn action_of(command: Option<&Digest>, root: Option<&Digest>) -> (Digest, Bytes) {
    let message = reapi::Action {
        command_digest: command.map(wire),
        input_root_digest: root.map(wire),
        ..reapi::Action::default()
    };
    let bytes = Bytes::from(message.encode_to_vec());
    (kbf_segments::sha256(&bytes), bytes)
}

/// The digests of the input tree of the standard Action.
///
/// The root holds the files `a` and `dup` and an empty file, and the directories `lib` and
/// `lib2`. `lib` holds `b`, `dup` (the same file as the root's) and `deep`; `lib2` holds `deep`
/// too, so `deep` (which holds `c`) is a nested directory that two parents name.
struct Tree {
    root: Digest,
    lib: Digest,
    lib2: Digest,
    deep: Digest,
    a: Digest,
    b: Digest,
    c: Digest,
    dup: Digest,
    /// Every digest of the tree, each once.
    expected: BTreeSet<Digest>,
}

/// Every blob a farm holds for the standard Action.
struct Parts {
    tree: Tree,
    command: Digest,
    action: Digest,
    /// A blob in the store that is in no Action's closure.
    stray: Digest,
    blobs: Vec<(Digest, Bytes)>,
}

impl Parts {
    fn new() -> Self {
        let (a, a_bytes) = blob("file a");
        let (b, b_bytes) = blob("file b of lib");
        let (c, c_bytes) = blob("file c, deep down");
        let (dup, dup_bytes) = blob("a file two directories hold");
        let (deep, deep_bytes) = dir(&[c], &[]);
        let (lib, lib_bytes) = dir(&[b, dup], &[deep]);
        let (lib2, lib2_bytes) = dir(&[], &[deep]);
        let (root, root_bytes) = dir(&[a, dup, empty()], &[lib, lib2]);
        let (command, command_bytes) = {
            let message = reapi::Command {
                arguments: vec!["true".to_owned()],
                ..reapi::Command::default()
            };
            let bytes = Bytes::from(message.encode_to_vec());
            (kbf_segments::sha256(&bytes), bytes)
        };
        let (action, action_bytes) = action_of(Some(&command), Some(&root));
        let (stray, stray_bytes) = blob("a blob of another tenant");
        let tree = Tree {
            root,
            lib,
            lib2,
            deep,
            a,
            b,
            c,
            dup,
            expected: BTreeSet::from([root, lib, lib2, deep, a, b, c, dup, empty()]),
        };
        let blobs = vec![
            (a, a_bytes),
            (b, b_bytes),
            (c, c_bytes),
            (dup, dup_bytes),
            (deep, deep_bytes),
            (lib, lib_bytes),
            (lib2, lib2_bytes),
            (root, root_bytes),
            (command, command_bytes),
            (action, action_bytes),
            (stray, stray_bytes),
        ];
        Self {
            tree,
            command,
            action,
            stray,
            blobs,
        }
    }

    /// The blob with this digest.
    fn blob(&self, digest: &Digest) -> (Digest, Bytes) {
        let found = self.blobs.iter().find(|(held, _)| held == digest);
        found.expect("the farm has this blob").clone()
    }

    /// The blobs except those of `omit`.
    fn without(&self, omit: &[Digest]) -> Vec<(Digest, Bytes)> {
        let kept = self
            .blobs
            .iter()
            .filter(|(digest, _)| !omit.contains(digest));
        kept.cloned().collect()
    }
}

/// A storage node that records the blobs it is asked to read and the FindMissing calls, and
/// fails them when told to.
struct Watched {
    inner: Arc<dyn Storage>,
    reads: Mutex<Vec<Digest>>,
    asks: AtomicUsize,
    reads_down: AtomicBool,
    asks_down: AtomicBool,
    reads_broken: AtomicBool,
    asks_broken: AtomicBool,
}

impl Watched {
    /// A node that holds `blobs`.
    async fn holding(blobs: Vec<(Digest, Bytes)>) -> Arc<Self> {
        let inner: Arc<dyn Storage> = Arc::new(CacheStorage::new(
            Arc::new(Cache::memory()),
            SegmentsConfig::DEFAULT,
        ));
        for (digest, bytes) in blobs {
            let stored = inner.put(bytes).await.expect("the blob is stored");
            assert_eq!(stored, digest, "the blob is stored under its digest");
        }
        Arc::new(Self {
            inner,
            reads: Mutex::new(Vec::new()),
            asks: AtomicUsize::new(0),
            reads_down: AtomicBool::new(false),
            asks_down: AtomicBool::new(false),
            reads_broken: AtomicBool::new(false),
            asks_broken: AtomicBool::new(false),
        })
    }

    /// The blobs read so far, sorted.
    fn reads(&self) -> Vec<Digest> {
        let mut reads = self
            .reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        reads.sort();
        reads
    }
}

fn down() -> StorageError {
    StorageError::Unavailable(Arc::new(std::io::Error::other("the store is down")))
}

/// What a store whose own records disagree answers: the same every time.
fn broken() -> StorageError {
    StorageError::Internal("the store's records disagree".to_owned())
}

impl Storage for Watched {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.asks.fetch_add(1, SeqCst);
        if self.asks_down.load(SeqCst) {
            return Box::pin(async { Err(down()) });
        }
        if self.asks_broken.load(SeqCst) {
            return Box::pin(async { Err(broken()) });
        }
        self.inner.find_missing(digests)
    }

    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(*digest);
        if self.reads_down.load(SeqCst) {
            return Box::pin(async { Err(down()) });
        }
        if self.reads_broken.load(SeqCst) {
            return Box::pin(async { Err(broken()) });
        }
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

fn config() -> RunPrepConfig {
    RunPrepConfig {
        mint: PackKeyMint::new(MINT_SECRET).expect("a mint"),
        grant_key: GrantKey::from_secret(GRANT_SECRET).expect("a grant key"),
        put: PutTarget {
            endpoint: ENDPOINT.to_owned(),
            region: REGION.to_owned(),
            bucket: BUCKET.to_owned(),
            key: AccessKey::new(KEY_ID, KEY_SECRET),
        },
        max_packs: 1,
        large: None,
        read_url: READ_URL.to_owned(),
        allow_plain_read_url: false,
        run_wall: WALL,
        slack: SLACK,
        limits: ClosureLimits::DEFAULT,
    }
}

fn prep_with(store: &Arc<Watched>, config: RunPrepConfig) -> RunPrep {
    let storage: Arc<dyn Storage> = store.clone();
    RunPrep::new(Backend::remote(storage), config).expect("a preparer")
}

fn prep(store: &Arc<Watched>) -> RunPrep {
    prep_with(store, config())
}

/// A preparer over a farm that holds all of the standard Action's blobs.
async fn standard() -> (Parts, Arc<Watched>, RunPrep) {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let prep = prep(&store);
    (parts, store, prep)
}

/// A preparer over a farm that holds the standard blobs except `omit`.
async fn missing(omit: &[Digest]) -> (Parts, Arc<Watched>, RunPrep) {
    let parts = Parts::new();
    let store = Watched::holding(parts.without(omit)).await;
    let prep = prep(&store);
    (parts, store, prep)
}

fn refused(result: Result<kbf_front::prepare_run::RunInputs, PrepareError>) -> PrepareError {
    result.expect_err("the run is refused")
}

#[tokio::test]
async fn a_run_gets_a_key_that_the_mint_verifies_for_that_lease_and_action() {
    let (parts, _, prep) = standard().await;
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    let mint = PackKeyMint::new(MINT_SECRET).expect("a mint");
    assert_eq!(inputs.key.build(), LEASE);
    assert!(inputs.key.key().starts_with(&format!("cas/b/{LEASE}/")));
    assert!(inputs.key.key().ends_with("-0"));
    let minted = mint.verify(&inputs.key, &parts.action).expect("verified");
    assert_eq!(minted, now(), "the key says the minute it was minted in");
    let other = blob("another action").0;
    assert!(
        mint.verify(&inputs.key, &other).is_err(),
        "the key is for this action only"
    );
}

#[tokio::test]
async fn the_put_url_is_a_presigned_put_for_exactly_the_key_and_the_signer_makes_the_same_url() {
    let (parts, _, prep) = standard().await;
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    // The signature covers the method, the host, the path and the query, so a URL equal to the
    // one the tier store's own signer makes for this request is one its verifier recomputes.
    let expected = presign_put(
        &AccessKey::new(KEY_ID, KEY_SECRET),
        &PresignRequest {
            endpoint: ENDPOINT,
            region: REGION,
            bucket: BUCKET,
            key: inputs.key.key(),
            expires_secs: 7_800,
            now: NOW_SECS * 1000,
        },
    )
    .expect("signed");
    assert_eq!(inputs.put_url, expected);
    let (path, query) = inputs.put_url.split_once('?').expect("a query");
    assert_eq!(path, format!("{ENDPOINT}/{BUCKET}/{}", inputs.key.key()));
    assert!(path.ends_with("-0"), "the one slot of the lease");
    assert!(query.split('&').any(|pair| pair == "X-Amz-Expires=7800"));
    assert!(
        query
            .split('&')
            .any(|pair| pair == "X-Amz-Date=20260921T141400Z")
    );
}

#[tokio::test]
async fn expiry_is_the_wall_plus_the_slack_and_the_grant_ends_with_the_url() {
    let (parts, _, prep) = standard().await;
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    assert!(inputs.put_url.contains("X-Amz-Expires=7800"));
    assert_eq!(inputs.read_grant.not_after, NOW_SECS + 7_800);
}

#[tokio::test]
async fn expiry_is_capped_at_four_hours() {
    let (parts, _, prep) = standard().await;
    let wall = MAX_EXPIRY - SLACK;
    let inputs = prep
        .prepare_run_for(LEASE, &parts.action, now(), wall)
        .await
        .expect("a wall that reaches the ceiling is allowed");
    assert!(inputs.put_url.contains("X-Amz-Expires=14400"));
    assert_eq!(inputs.read_grant.not_after, NOW_SECS + 14_400);
}

#[tokio::test]
async fn a_run_wall_that_would_pass_four_hours_is_refused() {
    let (parts, _, prep) = standard().await;
    let wall = MAX_EXPIRY - SLACK + Duration::from_secs(1);
    let error = refused(
        prep.prepare_run_for(LEASE, &parts.action, now(), wall)
            .await,
    );
    assert!(
        matches!(
            error,
            PrepareError::Expiry(ExpiryError::OverCeiling {
                seconds: 14_401,
                ceiling: 14_400
            })
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_fraction_of_a_second_of_wall_counts_as_a_second() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let mut short = config();
    short.run_wall = Duration::from_millis(1_500);
    short.slack = Duration::ZERO;
    let prep = prep_with(&store, short);
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    assert!(inputs.put_url.contains("X-Amz-Expires=2"));
    assert_eq!(inputs.read_grant.not_after, NOW_SECS + 2);
}

#[tokio::test]
async fn the_grant_verifies_and_its_scope_holds_exactly_the_action_the_command_and_every_input() {
    let (parts, _, prep) = standard().await;
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    let clock = Arc::new(AtomicU64::new(NOW_SECS));
    let read = Arc::clone(&clock);
    let grants = Grants::new(
        GrantKey::from_secret(GRANT_SECRET).expect("a grant key"),
        Arc::new(move || read.load(SeqCst)),
    );
    let grant = &inputs.read_grant;
    assert_eq!(grant.url, READ_URL);
    assert_eq!(grant.not_after, NOW_SECS + 7_800);
    let claims = grants.verify(grant.token.expose()).expect("a valid grant");
    assert_eq!(claims.build, LEASE);
    assert_eq!(claims.action, parts.action);
    assert_eq!(claims.not_after, grant.not_after);
    clock.store(grant.not_after, SeqCst);
    assert_eq!(
        grants.verify(grant.token.expose()),
        Err(GrantError::Expired),
        "the grant stops working at not_after"
    );

    let closure = &inputs.closure;
    assert_eq!(closure.action(), &parts.action);
    assert_eq!(closure.command(), &parts.command);
    let listed: BTreeSet<Digest> = closure.inputs().copied().collect();
    assert_eq!(listed, parts.tree.expected, "every input digest, each once");
    assert_eq!(closure.input_count(), 9);
    let bytes: u64 = parts.tree.expected.iter().map(|d| d.size_bytes).sum();
    assert_eq!(closure.input_bytes(), bytes);
    let scope = closure.scope();
    for digest in parts
        .tree
        .expected
        .iter()
        .chain([&parts.action, &parts.command])
    {
        assert!(scope.allows(digest), "{digest} is in the scope");
    }
    assert!(
        !scope.allows(&parts.stray),
        "a blob the store holds that is no input of the action is out of scope"
    );
    assert!(!scope.allows(&blob("unrelated").0));
}

#[tokio::test]
async fn each_directory_is_read_once_and_files_and_the_command_are_asked_for_not_read() {
    let (parts, store, prep) = standard().await;
    prep.prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    let tree = &parts.tree;
    let mut wanted = vec![parts.action, tree.root, tree.lib, tree.lib2, tree.deep];
    wanted.sort();
    assert_eq!(
        store.reads(),
        wanted,
        "the Action and the four Directories, `deep` once though two parents name it"
    );
    assert_eq!(
        store.asks.load(SeqCst),
        1,
        "one FindMissing for the files and the Command"
    );
}

#[tokio::test]
async fn a_missing_input_file_is_an_error() {
    let parts = Parts::new();
    let (_, _, prep) = missing(&[parts.tree.b]).await;
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Missing { what: "a file", digest })
                if *digest == parts.tree.b
        ),
        "{error}"
    );
}

#[tokio::test]
async fn of_two_missing_files_the_smallest_digest_is_named() {
    let parts = Parts::new();
    let (_, _, prep) = missing(&[parts.tree.c, parts.tree.b]).await;
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    let smallest = parts.tree.b.min(parts.tree.c);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Missing { digest, .. }) if *digest == smallest
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_directory_is_an_error() {
    let parts = Parts::new();
    let (_, _, prep) = missing(&[parts.tree.lib]).await;
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Missing { what: "a Directory", digest })
                if *digest == parts.tree.lib
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_command_is_an_error() {
    let parts = Parts::new();
    let (_, _, prep) = missing(&[parts.command]).await;
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Missing { what: "the Command", digest })
                if *digest == parts.command
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_action_is_an_error() {
    let parts = Parts::new();
    let (_, _, prep) = missing(&[parts.action]).await;
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Missing { what: "the Action", digest })
                if *digest == parts.action
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_store_that_cannot_be_read_is_unavailable_not_missing() {
    let (parts, store, prep) = standard().await;
    store.reads_down.store(true, SeqCst);
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Unavailable { digest, .. })
                if *digest == parts.action
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_store_that_cannot_say_what_it_holds_is_unavailable_not_missing() {
    let (parts, store, prep) = standard().await;
    store.asks_down.store(true, SeqCst);
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Unavailable { .. })
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_closure_over_the_digest_budget_is_an_error_and_one_at_it_is_not() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let mut at = config();
    at.limits.max_digests = 9;
    prep_with(&store, at)
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("nine digests are within a budget of nine");
    let mut over = config();
    over.limits.max_digests = 8;
    let error = refused(
        prep_with(&store, over)
            .prepare_run(LEASE, &parts.action, now())
            .await,
    );
    assert!(
        matches!(
            error,
            PrepareError::Closure(ClosureError::TooManyDigests { limit: 8 })
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_closure_over_the_byte_budget_is_an_error_and_one_at_it_is_not() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let total: u64 = parts.tree.expected.iter().map(|d| d.size_bytes).sum();
    let mut at = config();
    at.limits.max_bytes = total;
    prep_with(&store, at)
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("the tree is within a budget of its own size");
    let mut over = config();
    over.limits.max_bytes = total - 1;
    let error = refused(
        prep_with(&store, over)
            .prepare_run(LEASE, &parts.action, now())
            .await,
    );
    assert!(
        matches!(
            error,
            PrepareError::Closure(ClosureError::TooManyBytes { limit }) if limit == total - 1
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_tree_over_budget_is_cut_off_before_the_directories_under_it_are_read() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let mut tiny = config();
    tiny.limits.max_digests = 1;
    let error = refused(
        prep_with(&store, tiny)
            .prepare_run(LEASE, &parts.action, now())
            .await,
    );
    assert!(matches!(
        error,
        PrepareError::Closure(ClosureError::TooManyDigests { limit: 1 })
    ));
    let mut wanted = vec![parts.action, parts.tree.root];
    wanted.sort();
    assert_eq!(
        store.reads(),
        wanted,
        "the Action and the root, nothing under it"
    );
}

#[tokio::test]
async fn an_invalid_lease_string_is_refused_before_the_store_is_read() {
    let (parts, store, prep) = standard().await;
    let long = "x".repeat(129);
    for lease in ["", "has a space", "a/b", "nul\0byte", long.as_str()] {
        let error = refused(prep.prepare_run(lease, &parts.action, now()).await);
        assert!(
            matches!(error, PrepareError::Lease(_)),
            "{lease:?}: {error}"
        );
    }
    assert!(store.reads().is_empty(), "no blob was read");
    assert_eq!(store.asks.load(SeqCst), 0);
}

#[tokio::test]
async fn a_lease_a_key_can_name_but_a_grant_cannot_carry_is_refused() {
    let (parts, _, prep) = standard().await;
    let lease = "l".repeat(100);
    let error = refused(prep.prepare_run(&lease, &parts.action, now()).await);
    assert!(
        matches!(error, PrepareError::Grant(ClaimsError::Build(_))),
        "{error}"
    );
}

#[tokio::test]
async fn a_time_before_the_epoch_is_refused() {
    let (parts, _, prep) = standard().await;
    let before = UNIX_EPOCH - Duration::from_secs(1);
    let error = refused(prep.prepare_run(LEASE, &parts.action, before).await);
    assert!(matches!(error, PrepareError::Clock), "{error}");
}

#[tokio::test]
async fn a_time_past_the_last_minute_a_key_can_name_is_refused() {
    let (parts, _, prep) = standard().await;
    let far = UNIX_EPOCH + Duration::from_secs(1 << 40);
    let error = refused(prep.prepare_run(LEASE, &parts.action, far).await);
    assert!(matches!(error, PrepareError::Mint(_)), "{error}");
}

#[tokio::test]
async fn the_same_inputs_give_the_same_closure_and_scope() {
    let (parts, _, first) = standard().await;
    let (_, _, second) = standard().await;
    let one = first
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    let later = now() + Duration::from_secs(3_600);
    let two = second
        .prepare_run("00000000000000010000000000000003", &parts.action, later)
        .await
        .expect("prepared");
    assert_eq!(one.closure, two.closure, "another lease, time and farm");
    let again = first.closure(&parts.action).await.expect("collected");
    assert_eq!(one.closure, again);
    let probe = [
        parts.tree.root,
        parts.tree.a,
        parts.tree.dup,
        parts.stray,
        parts.action,
    ];
    for digest in &probe {
        assert_eq!(
            one.closure.scope().allows(digest),
            two.closure.scope().allows(digest)
        );
    }
    assert_ne!(one.key, two.key, "a key is made for its own lease");
}

#[tokio::test]
async fn an_empty_input_root_is_the_empty_directory_and_is_not_read() {
    let parts = Parts::new();
    let (action, bytes) = action_of(Some(&parts.command), Some(&empty()));
    let mut blobs = parts.without(&[parts.action]);
    blobs.push((action, bytes));
    let store = Watched::holding(blobs).await;
    let inputs = prep(&store)
        .prepare_run(LEASE, &action, now())
        .await
        .expect("prepared");
    let listed: Vec<Digest> = inputs.closure.inputs().copied().collect();
    assert_eq!(listed, [empty()]);
    assert_eq!(store.reads(), [action], "only the Action was read");
}

#[tokio::test]
async fn an_action_that_does_not_decode_is_an_error() {
    let bytes = Bytes::from_static(&[0xff, 0xff, 0xff]);
    let action = kbf_segments::sha256(&bytes);
    let store = Watched::holding(vec![(action, bytes)]).await;
    let error = refused(prep(&store).prepare_run(LEASE, &action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid { what: "the Action", digest, .. })
                if *digest == action
        ),
        "{error}"
    );
}

#[tokio::test]
async fn an_action_without_a_command_or_an_input_root_is_an_error() {
    let parts = Parts::new();
    let no_command = action_of(None, Some(&parts.tree.root));
    let no_root = action_of(Some(&parts.command), None);
    let store = Watched::holding(vec![no_command.clone(), no_root.clone()]).await;
    let prep = prep(&store);
    let error = refused(prep.prepare_run(LEASE, &no_command.0, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid {
                what: "the Action's command digest",
                ..
            })
        ),
        "{error}"
    );
    let error = refused(prep.prepare_run(LEASE, &no_root.0, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid {
                what: "the Action's input root",
                ..
            })
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_directory_that_does_not_decode_is_an_error() {
    let parts = Parts::new();
    let bytes = Bytes::from_static(&[0xff, 0xff, 0xff]);
    let root = kbf_segments::sha256(&bytes);
    let (action, action_bytes) = action_of(Some(&parts.command), Some(&root));
    let store = Watched::holding(vec![(root, bytes), (action, action_bytes)]).await;
    let error = refused(prep(&store).prepare_run(LEASE, &action, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid { what: "a Directory", digest, .. })
                if *digest == root
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_directory_that_names_a_malformed_digest_is_an_error() {
    let parts = Parts::new();
    let bad_file = Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: Some(reapi::Digest {
                hash: "not a hash".to_owned(),
                size_bytes: 1,
            }),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let bad_file = Bytes::from(bad_file.encode_to_vec());
    let bad_file_digest = kbf_segments::sha256(&bad_file);
    let bad_child = Directory {
        directories: vec![DirectoryNode {
            name: "d".to_owned(),
            digest: None,
        }],
        ..Directory::default()
    };
    let bad_child = Bytes::from(bad_child.encode_to_vec());
    let bad_child_digest = kbf_segments::sha256(&bad_child);
    let with_file = action_of(Some(&parts.command), Some(&bad_file_digest));
    let with_child = action_of(Some(&parts.command), Some(&bad_child_digest));
    let store = Watched::holding(vec![
        (bad_file_digest, bad_file),
        (bad_child_digest, bad_child),
        with_file.clone(),
        with_child.clone(),
        parts.blob(&parts.command),
    ])
    .await;
    let prep = prep(&store);
    let error = refused(prep.prepare_run(LEASE, &with_file.0, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid { what: "a file", digest, .. })
                if *digest == bad_file_digest
        ),
        "{error}"
    );
    let error = refused(prep.prepare_run(LEASE, &with_child.0, now()).await);
    assert!(
        matches!(
            &error,
            PrepareError::Closure(ClosureError::Invalid { what: "a subdirectory", digest, .. })
                if *digest == bad_child_digest
        ),
        "{error}"
    );
}

#[tokio::test]
async fn the_debug_of_a_run_shows_no_url_and_no_token() {
    let (parts, _, prep) = standard().await;
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    let shown = format!("{inputs:?} {prep:?}");
    assert!(!shown.contains("X-Amz-Signature"));
    assert!(!shown.contains(inputs.read_grant.token.expose()));
    assert!(!shown.contains(KEY_SECRET));
    assert!(shown.contains(LEASE));
}
