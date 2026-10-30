//! The job-facing read's byte budget, its single-flight scope walk, and the batch's read
//! through [`BlobSource::read_many`].
//!
//! The source yields while a read is open, so that reads that are allowed to overlap do, and
//! records the most reads, and the most bytes of them, that were ever open at once.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::{BoxFuture, join_all};
use futures::{StreamExt as _, stream};
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{
    BATCH_PATH, BlobSource, HELD_CHUNK_BYTES, JobRead, Limits, Reply, StreamError,
};
use kbf_front::{Backend, Cache, VerifiedBlob, digest_to_proto};
use kbf_proto::reapi::{Action, Directory, DirectoryNode, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::Semaphore;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0013";
const NOT_AFTER: u64 = 1_000;

#[derive(Default)]
struct Open {
    now: usize,
    bytes: u64,
    most: usize,
    most_bytes: u64,
}

struct Source {
    blobs: HashMap<Digest, Bytes>,
    open: Mutex<Open>,
    reads: Mutex<Vec<Digest>>,
    many: Mutex<Vec<Vec<Digest>>>,
    /// The `read_many` calls open now, and the most there ever were.
    many_open: Mutex<(usize, usize)>,
    /// Answers `read_many` with its first answer only.
    short_many: AtomicBool,
    /// Digests the store does not hold, though they are in the tree.
    hidden: Mutex<HashSet<Digest>>,
    /// The store fails every read it is asked for.
    broken: AtomicBool,
    gate: Option<(Digest, Semaphore)>,
}

impl Source {
    fn reads_of(&self, digest: &Digest) -> usize {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.iter().filter(|read| *read == digest).count()
    }

    fn hide(&self, digest: Digest) {
        self.hidden
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(digest);
    }

    /// What the store answers for `digest`.
    fn answer(&self, digest: &Digest) -> Result<Bytes, StorageError> {
        let hidden = self
            .hidden
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(digest);
        if self.broken.load(Ordering::SeqCst) {
            return Err(StorageError::Unreachable(*digest));
        }
        match self.blobs.get(digest) {
            Some(bytes) if !hidden => Ok(bytes.clone()),
            _ => Err(StorageError::NotFound(*digest)),
        }
    }

    fn most(&self) -> (usize, u64) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        (open.most, open.most_bytes)
    }
}

impl BlobSource for Source {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            self.reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            {
                let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
                open.now += 1;
                open.bytes += digest.size_bytes;
                open.most = open.most.max(open.now);
                open.most_bytes = open.most_bytes.max(open.bytes);
            }
            if let Some((held, gate)) = &self.gate
                && held == digest
            {
                gate.acquire().await.expect("the gate is open").forget();
            }
            for _ in 0..30 {
                tokio::task::yield_now().await;
            }
            {
                let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
                open.now -= 1;
                open.bytes -= digest.size_bytes;
            }
            self.answer(digest)
        })
    }

    fn read_many<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Vec<Result<Bytes, StorageError>>> {
        Box::pin(async move {
            self.many
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(digests.to_vec());
            if let Some((held, gate)) = &self.gate
                && digests.contains(held)
            {
                gate.acquire().await.expect("the gate is open").forget();
            }
            {
                let mut open = self
                    .many_open
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                open.0 += 1;
                open.1 = open.1.max(open.0);
            }
            for _ in 0..30 {
                tokio::task::yield_now().await;
            }
            self.many_open
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .0 -= 1;
            let take = if self.short_many.load(Ordering::SeqCst) {
                1
            } else {
                digests.len()
            };
            digests.iter().take(take).map(|d| self.answer(d)).collect()
        })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move {
            let blob = self.answer(digest)?;
            let chunks: Vec<Result<Bytes, StorageError>> = blob
                .chunks(1024)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            Ok(stream::iter(chunks).boxed())
        })
    }
}

fn put(blobs: &mut HashMap<Digest, Bytes>, bytes: Vec<u8>) -> Digest {
    let digest = kbf_segments::sha256(&bytes);
    blobs.insert(digest, Bytes::from(bytes));
    digest
}

/// An Action over a directory of files with these contents, whose file names are padded.
struct Built {
    action: Digest,
    command: Digest,
    root: Digest,
    files: Vec<Digest>,
}

fn build(blobs: &mut HashMap<Digest, Bytes>, tag: &str, contents: &[Vec<u8>], pad: usize) -> Built {
    let files: Vec<Digest> = contents.iter().map(|c| put(blobs, c.clone())).collect();
    let nodes = files
        .iter()
        .enumerate()
        .map(|(i, digest)| FileNode {
            name: format!("{tag}-{i}-{}", "x".repeat(pad)),
            digest: Some(digest_to_proto(digest)),
            ..FileNode::default()
        })
        .collect();
    let root = Directory {
        files: nodes,
        ..Directory::default()
    };
    let root = put(blobs, root.encode_to_vec());
    let command = put(blobs, format!("command of {tag}").into_bytes());
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(blobs, action.encode_to_vec());
    Built {
        action,
        command,
        root,
        files,
    }
}

fn pattern(seed: usize, n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i + seed) * 31 % 251) as u8).collect()
}

struct Rig {
    source: Arc<Source>,
    read: JobRead,
    grants: Arc<Grants>,
}

fn rig(blobs: HashMap<Digest, Bytes>, gate: Option<Digest>, limits: Limits) -> Rig {
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::new(AtomicU64::new(100));
    let grants = Arc::new(Grants::new(
        key,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let source = Arc::new(Source {
        blobs,
        open: Mutex::new(Open::default()),
        reads: Mutex::new(Vec::new()),
        many: Mutex::new(Vec::new()),
        many_open: Mutex::new((0, 0)),
        short_many: AtomicBool::new(false),
        hidden: Mutex::new(HashSet::new()),
        broken: AtomicBool::new(false),
        gate: gate.map(|digest| (digest, Semaphore::new(0))),
    });
    let read = JobRead::new(source.clone(), Arc::clone(&grants), limits);
    Rig {
        source,
        read,
        grants,
    }
}

impl Rig {
    fn token(&self, action: Digest) -> String {
        self.grants
            .issue(BUILD, action, NOT_AFTER)
            .expect("a valid build id")
            .expose()
            .to_owned()
    }

    async fn get(&self, digest: &Digest, token: &str) -> Reply {
        let header = format!("Bearer {token}");
        self.read
            .respond(&Method::GET, &format!("/{digest}"), Some(&header))
            .await
    }

    async fn batch(&self, digests: &[Digest], token: &str) -> Reply {
        let header = format!("Bearer {token}");
        let quoted: Vec<String> = digests.iter().map(|d| format!("\"{d}\"")).collect();
        let body = format!("{{\"digests\":[{}]}}", quoted.join(","));
        self.read
            .respond_with_body(&Method::POST, BATCH_PATH, Some(&header), body.as_bytes())
            .await
    }
}

fn frame(status: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![status];
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Catches: a batch that reads its blobs one by one (so the pack layer cannot coalesce them
/// into few ranged reads): the source is asked once, for the blobs that need a read, each
/// once, in request order; a digest outside the scope, one too large for a batch and the empty
/// blob are not asked for; and every blob is answered in its place with its own bytes.
#[tokio::test]
async fn a_batch_reads_its_blobs_with_one_read_many_call() {
    let mut blobs = HashMap::new();
    let t = build(
        &mut blobs,
        "a",
        &[b"alpha".to_vec(), b"beta".to_vec(), vec![]],
        0,
    );
    let other = build(&mut blobs, "b", &[b"gamma".to_vec()], 0);
    let rig = rig(blobs, None, Limits::default());
    let token = rig.token(t.action);
    let empty = t.files[2];
    let reply = rig
        .batch(
            &[t.files[1], other.files[0], t.files[0], empty, t.files[1]],
            &token,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let expected = [
        frame(0, b"beta"),
        frame(2, b""),
        frame(0, b"alpha"),
        frame(0, b""),
        frame(0, b"beta"),
    ]
    .concat();
    assert_eq!(&reply.body[..], &expected[..]);
    let many = rig
        .source
        .many
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // The scope walk reads the Action and its Directory one by one; the blobs are one call.
    assert_eq!(many.len(), 1, "{many:?}");
    assert_eq!(many[0], [t.files[1], t.files[0]]);
    assert_eq!(rig.source.reads_of(&t.files[0]), 0);
    assert_eq!(rig.source.reads_of(&other.files[0]), 0);
}

/// Catches: reads that are not bounded by bytes: with a budget of 4000 bytes, blobs of 3000
/// bytes are read one at a time (two would be 6000), with 7000 two at once and not three, and
/// every request is answered either way (a request over the budget waits, never fails). A blob
/// larger than the whole budget is read alone, and is answered too.
#[tokio::test]
async fn whole_reads_are_bounded_by_the_byte_budget_and_wait_instead_of_failing() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..6).map(|i| pattern(i, 3000)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    for (budget, at_most, at_least) in [(4000, 3000, 3000), (7000, 6000, 6000)] {
        let limits = Limits {
            read_budget_bytes: budget,
            ..Limits::default()
        };
        let rig = rig(blobs.clone(), None, limits);
        let token = rig.token(t.action);
        // The scope is worked out first, so that only the blobs are open together.
        assert_eq!(rig.get(&t.command, &token).await.status, StatusCode::OK);
        // Each reply is dropped as soon as it has its status: a reply keeps its bytes charged
        // until it is dropped, and the reads that wait for room wait for that.
        let replies = join_all(
            t.files
                .iter()
                .map(|d| async { rig.get(d, &token).await.status }),
        )
        .await;
        assert!(replies.iter().all(|r| *r == StatusCode::OK), "{budget}");
        let (_, bytes) = rig.source.most();
        assert!(bytes <= at_most && bytes >= at_least, "{budget}: {bytes}");
    }
    let limits = Limits {
        read_budget_bytes: 1000,
        ..Limits::default()
    };
    let rig = rig(blobs.clone(), None, limits);
    let token = rig.token(t.action);
    let reply = rig.get(&t.files[0], &token).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "a blob over the budget is read alone"
    );
    assert_eq!(reply.body.len(), 3000);
}

/// Catches: a streamed blob that takes its whole size from the budget (a 20 KiB blob with a
/// budget of 8 KiB would then be one stream at a time, and a 500 MiB blob would starve every
/// other read), and one that takes nothing: with a window of 4 KiB two streams are open at once,
/// a third waits, and goes on when one is dropped.
#[tokio::test]
async fn a_streamed_blob_takes_a_bounded_window_of_the_budget() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..3).map(|i| pattern(i, 20 * 1024)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let limits = Limits {
        read_budget_bytes: 8 * 1024,
        stream_window: 4 * 1024,
        stream_over: 1024,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    let first = rig.get(&t.files[0], &token).await;
    let second = rig.get(&t.files[1], &token).await;
    assert!(first.stream.is_some() && second.stream.is_some());
    let mut third = Box::pin(rig.get(&t.files[2], &token));
    assert!(
        futures::poll!(third.as_mut()).is_pending(),
        "the budget is taken"
    );
    drop(first);
    let third = third.await;
    assert!(
        third.stream.is_some(),
        "a place is given back when a stream is dropped"
    );
}

/// Catches: the scope walk reading outside the budget (the walks of many Actions at once,
/// each reading Directories, are what a farm's first minute is): with a budget of 1000 bytes
/// and Directories of about 600, the walks of six Actions never have more than the budget open
/// at once, and every one of them is answered.
#[tokio::test]
async fn the_scope_walk_counts_against_the_byte_budget() {
    let mut blobs = HashMap::new();
    let built: Vec<Built> = (0..6)
        .map(|i| build(&mut blobs, &format!("t{i}"), &[pattern(i, 10)], 500))
        .collect();
    let dir_size = blobs[&built[0].root].len() as u64;
    assert!(dir_size > 500 && dir_size < 700, "{dir_size}");
    let limits = Limits {
        read_budget_bytes: 1000,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let tokens: Vec<String> = built.iter().map(|b| rig.token(b.action)).collect();
    let at = &rig;
    let replies = join_all(
        built
            .iter()
            .zip(&tokens)
            .map(|(b, token)| async move { at.get(&b.command, token).await.status }),
    )
    .await;
    assert!(replies.iter().all(|r| *r == StatusCode::OK));
    let (most, bytes) = rig.source.most();
    assert!(bytes <= 1000, "{bytes} bytes were open at once");
    assert!(most >= 1);
}

/// Catches: every first request of an Action walking its tree: a hundred tasks of one action
/// asking at once (the Action's own blob is held open so that they all arrive before the walk
/// ends) read the Action and each Directory once, and are all answered.
#[tokio::test]
async fn concurrent_first_requests_of_one_action_walk_its_tree_once() {
    let mut blobs = HashMap::new();
    let t = build(&mut blobs, "a", &[b"alpha".to_vec()], 0);
    let rig = rig(blobs, Some(t.action), Limits::default());
    let token = rig.token(t.action);
    let asks = join_all((0..100).map(|_| rig.get(&t.files[0], &token)));
    let driver = async {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(rig.source.reads_of(&t.action), 1, "one walk is open");
        let (_, gate) = rig.source.gate.as_ref().expect("a gate");
        gate.add_permits(1);
    };
    let (replies, ()) = tokio::join!(asks, driver);
    assert!(replies.iter().all(|r| r.status == StatusCode::OK));
    assert_eq!(rig.source.reads_of(&t.action), 1);
    assert_eq!(rig.source.reads_of(&t.root), 1);
}

/// Catches: a batch that holds no place among the reads in flight (a task sending 256-blob
/// requests in parallel opening unbounded reads of the store): with one place, a GET waits for
/// the open batch and goes on when its read ends.
#[tokio::test]
async fn a_batch_holds_a_read_place_while_its_blobs_are_read() {
    let mut blobs = HashMap::new();
    let t = build(&mut blobs, "a", &[b"alpha".to_vec(), b"beta".to_vec()], 0);
    let limits = Limits {
        reads_in_flight: 1,
        ..Limits::default()
    };
    let rig = rig(blobs, Some(t.files[0]), limits);
    let token = rig.token(t.action);
    let wanted = [t.files[0]];
    let batch = rig.batch(&wanted, &token);
    let get = async {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        rig.get(&t.files[1], &token).await
    };
    let driver = async {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            rig.source.reads_of(&t.files[1]),
            0,
            "the GET waits for the batch"
        );
        let (_, gate) = rig.source.gate.as_ref().expect("a gate");
        gate.add_permits(1);
    };
    let (batch, get, ()) = tokio::join!(batch, get, driver);
    assert_eq!(batch.status, StatusCode::OK);
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], b"beta");
}

/// Catches: a batch with nothing to read (the empty blob, a digest outside the scope, a blob
/// too large for a batch) taking places and calling the source for nothing: it is answered
/// from its digests alone, and the source is not asked.
#[tokio::test]
async fn a_batch_with_nothing_to_read_does_not_ask_the_source() {
    let mut blobs = HashMap::new();
    let t = build(&mut blobs, "a", &[vec![]], 0);
    let other = build(&mut blobs, "b", &[b"gamma".to_vec()], 0);
    let rig = rig(blobs, None, Limits::default());
    let token = rig.token(t.action);
    let reply = rig.batch(&[t.files[0], other.files[0]], &token).await;
    assert_eq!(reply.status, StatusCode::OK);
    let expected = [frame(0, b""), frame(2, b"")].concat();
    assert_eq!(&reply.body[..], &expected[..]);
    let many = rig
        .source
        .many
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    assert!(many.is_empty(), "{many:?}");
}

/// Catches: the budget being given back when the bytes are read, while the reply still holds
/// them and a client that does not read keeps them there (256 connections of 64 MiB are far
/// more than the budget): with a budget of 4000 bytes, a reply of 3000 that is not sent keeps
/// a second read of 3000 waiting, and a batch reply does the same; each goes on when the reply
/// is dropped.
#[tokio::test]
async fn a_reply_that_is_not_sent_keeps_its_bytes_charged_to_the_budget() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..2).map(|i| pattern(i, 3000)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let limits = Limits {
        read_budget_bytes: 4000,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    let held = rig.get(&t.files[0], &token).await;
    assert_eq!(held.body.len(), 3000);
    let mut second = Box::pin(rig.get(&t.files[1], &token));
    assert!(
        futures::poll!(second.as_mut()).is_pending(),
        "a GET reply is charged"
    );
    drop(held);
    assert_eq!(second.await.status, StatusCode::OK);

    let held = rig.batch(&[t.files[0]], &token).await;
    assert_eq!(held.status, StatusCode::OK);
    let mut second = Box::pin(rig.get(&t.files[1], &token));
    assert!(
        futures::poll!(second.as_mut()).is_pending(),
        "a batch reply is charged"
    );
    drop(held);
    assert_eq!(second.await.status, StatusCode::OK);
}

/// Catches: a body handed to the connection whole (a 64 MiB copy into the connection's write
/// buffer while the budget is already given back) and a budget given back before the last byte
/// is taken: `held_stream` cuts the body into chunks of at most [`HELD_CHUNK_BYTES`], gives
/// all of its bytes in order, holds the budget until the stream ends, and is none for a
/// reply that holds nothing (a refusal).
#[tokio::test]
async fn a_held_body_is_sent_in_chunks_and_the_budget_comes_back_when_it_ends() {
    let mut blobs = HashMap::new();
    let big = pattern(1, 3 * HELD_CHUNK_BYTES + 100);
    let t = build(&mut blobs, "a", &[big.clone(), pattern(2, 3000)], 0);
    let limits = Limits {
        read_budget_bytes: 4 * HELD_CHUNK_BYTES as u64,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    let mut reply = rig.get(&t.files[0], &token).await;
    let stream = reply.held_stream().expect("a reply that holds its bytes");
    assert!(reply.body.is_empty() && reply.held_stream().is_none());
    assert_eq!(stream.size, big.len() as u64);
    let mut chunks = stream.chunks;
    let first = chunks.next().await.expect("a chunk").expect("ok");
    assert_eq!(first.len(), HELD_CHUNK_BYTES);
    // The open stream holds the budget: a read of the same size waits.
    let mut second = Box::pin(rig.get(&t.files[0], &token));
    assert!(futures::poll!(second.as_mut()).is_pending());
    let mut got = first.to_vec();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.expect("ok");
        assert!(chunk.len() <= HELD_CHUNK_BYTES);
        got.extend_from_slice(&chunk);
    }
    assert_eq!(got, big);
    drop(chunks);
    assert_eq!(second.await.body.len(), big.len());
    let nowhere = Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 4);
    let mut refused = rig.get(&nowhere, &token).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.held_stream().is_none());
}

/// Catches: the scope walk taking budget, and reading, for the size a tenant wrote into a
/// Directory or an Action entry (10^12 bytes would take the whole budget and stall every
/// request): a child Directory, a root and an Action of over the cap give no scope, and none of
/// them is read.
#[tokio::test]
async fn a_directory_or_action_blob_over_the_cap_gives_no_scope_and_is_not_read() {
    let mut blobs = HashMap::new();
    let huge = Digest::new(
        kbf_types::DigestFunction::Sha256,
        [7; 32],
        1_000_000_000_000,
    );
    let child = DirectoryNode {
        name: "child".to_owned(),
        digest: Some(digest_to_proto(&huge)),
    };
    let root = Directory {
        directories: vec![child],
        ..Directory::default()
    };
    let root = put(&mut blobs, root.encode_to_vec());
    let command = put(&mut blobs, b"the command".to_vec());
    let action_of = |root: &Digest| Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(root)),
        ..Action::default()
    };
    let by_child = put(&mut blobs, action_of(&root).encode_to_vec());
    let by_root = put(&mut blobs, action_of(&huge).encode_to_vec());
    let limits = Limits {
        read_budget_bytes: 1024,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    for action in [by_child, by_root, huge] {
        let reply = rig.get(&command, &rig.token(action)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{action}");
        assert_eq!(&reply.body[..], b"no_scope\n", "{action}");
    }
    assert_eq!(rig.source.reads_of(&huge), 0);
}

/// Catches: a batch that does not take the budget for its read, which no other test notices:
/// with a budget of 4000, two batches of one 3000-byte blob each never have their reads open
/// together.
#[tokio::test]
async fn a_batch_takes_its_budget_for_the_whole_read() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..2).map(|i| pattern(i, 3000)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let limits = Limits {
        read_budget_bytes: 4000,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    assert_eq!(rig.get(&t.command, &token).await.status, StatusCode::OK);
    let (a, b) = tokio::join!(
        async { rig.batch(&t.files[..1], &token).await.status },
        async { rig.batch(&t.files[1..], &token).await.status }
    );
    assert_eq!((a, b), (StatusCode::OK, StatusCode::OK));
    let most = rig
        .source
        .many_open
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .1;
    assert_eq!(most, 1, "the second batch waits for room");
}

/// Catches: a batch larger than the whole budget waiting for ever (it takes all of it instead),
/// and a batch that is charged its blobs' sizes only, when the bytes it holds pin the gaps
/// between blobs that the pack layer reads through: two blobs of 3000 bytes are charged 3000 +
/// 3000 + 64 KiB, so with a budget of 100 KiB two such batches are never open together.
#[tokio::test]
async fn a_batch_is_charged_its_gaps_and_one_over_the_budget_is_served() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..4).map(|i| pattern(i, 3000)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let small = Limits {
        read_budget_bytes: 1024,
        ..Limits::default()
    };
    let rig_small = rig(blobs.clone(), None, small);
    let token = rig_small.token(t.action);
    let reply = rig_small.batch(&t.files[..2], &token).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "a batch over the budget takes all of it"
    );
    let limits = Limits {
        read_budget_bytes: 100 * 1024,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    assert_eq!(rig.get(&t.command, &token).await.status, StatusCode::OK);
    let (a, b) = tokio::join!(
        async { rig.batch(&t.files[..2], &token).await.status },
        async { rig.batch(&t.files[2..], &token).await.status }
    );
    assert_eq!((a, b), (StatusCode::OK, StatusCode::OK));
    let most = rig
        .source
        .many_open
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .1;
    assert_eq!(most, 1, "the gaps are charged");
}

/// Catches: a batch that takes every read place (a batch of at least `reads_in_flight` blobs
/// queued behind one slow streamed read, and every GET queued behind it): with four places a
/// batch of four blobs that is held open takes at most two, and a GET goes on meanwhile.
#[tokio::test]
async fn a_batch_takes_at_most_half_the_read_places() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..5).map(|i| pattern(i, 100)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let limits = Limits {
        reads_in_flight: 4,
        ..Limits::default()
    };
    let rig = rig(blobs, Some(t.files[0]), limits);
    let token = rig.token(t.action);
    let batch = rig.batch(&t.files[..4], &token);
    let driver = async {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        // A GET is served while the batch is held open: it is not queued behind its places.
        let got = rig.get(&t.files[4], &token).await;
        assert_eq!(got.status, StatusCode::OK);
        let (_, gate) = rig.source.gate.as_ref().expect("a gate");
        gate.add_permits(1);
    };
    let (batch, ()) = tokio::join!(batch, driver);
    assert_eq!(batch.status, StatusCode::OK);
}

/// Catches: a source that answers a `read_many` with fewer answers than blobs asked for
/// panicking the request: the blob with no answer is `store_unavailable`, the one with an
/// answer is served.
#[tokio::test]
async fn a_source_with_too_few_answers_leaves_store_unavailable_not_a_panic() {
    let mut blobs = HashMap::new();
    let t = build(&mut blobs, "a", &[b"alpha".to_vec(), b"beta".to_vec()], 0);
    let rig = rig(blobs, None, Limits::default());
    rig.source.short_many.store(true, Ordering::SeqCst);
    let token = rig.token(t.action);
    let reply = rig.batch(&t.files, &token).await;
    let expected = [frame(0, b"alpha"), vec![4, 0, 0, 0, 0, 0, 0, 0, 0]].concat();
    assert_eq!(&reply.body[..], &expected[..]);
}

/// Catches: a walk in progress being dropped when the memory of scopes is at its bound (a
/// second request for the Action would walk it again, and a farm of many Actions would walk
/// each many times): with a bound of one, the Action whose walk is open is asked for again
/// after another Action arrived, and is read once.
#[tokio::test]
async fn a_walk_in_progress_survives_the_bound_of_the_scope_memory() {
    let mut blobs = HashMap::new();
    let a = build(&mut blobs, "a", &[b"alpha".to_vec()], 0);
    let b = build(&mut blobs, "b", &[b"beta".to_vec()], 0);
    let limits = Limits {
        scopes: 1,
        ..Limits::default()
    };
    let rig = rig(blobs, Some(a.action), limits);
    let (ta, tb) = (rig.token(a.action), rig.token(b.action));
    let first = rig.get(&a.files[0], &ta);
    let other = async {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        rig.get(&b.files[0], &tb).await
    };
    let again = async {
        for _ in 0..60 {
            tokio::task::yield_now().await;
        }
        rig.get(&a.files[0], &ta).await
    };
    let driver = async {
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        assert_eq!(rig.source.reads_of(&a.action), 1, "the open walk is shared");
        let (_, gate) = rig.source.gate.as_ref().expect("a gate");
        gate.add_permits(1);
    };
    let (x, y, z, ()) = tokio::join!(first, other, again, driver);
    assert!(
        [x.status, y.status, z.status]
            .iter()
            .all(|s| *s == StatusCode::OK)
    );
    assert_eq!(rig.source.reads_of(&a.action), 1);
}

/// Catches: a client that opens a response and never reads it holding its share of the budget
/// for ever (enough of them stop every read of the listener): with an idle limit of 30
/// seconds on a paused clock, a read that waits for the room of an unread GET reply is served
/// once the limit has passed, and the stalled body, if it is read after that, ends with
/// `StreamError::Idle` and no bytes.
#[tokio::test(start_paused = true)]
async fn a_reader_that_stalls_gives_its_budget_back_after_the_idle_limit() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..2).map(|i| pattern(i, 3000)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let limits = Limits {
        read_budget_bytes: 4000,
        held_idle: Duration::from_secs(30),
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    let mut stalled = rig.get(&t.files[0], &token).await;
    let mut body = stalled.held_stream().expect("a held body").chunks;
    let mut second = Box::pin(rig.get(&t.files[1], &token));
    assert!(futures::poll!(second.as_mut()).is_pending());
    assert_eq!(
        second.await.status,
        StatusCode::OK,
        "served after the idle limit"
    );
    assert!(matches!(body.next().await, Some(Err(StreamError::Idle))));
    assert!(body.next().await.is_none());
}

/// Catches: the idle limit cutting off a client that is reading, however long its response
/// takes in all: a body read one piece every 20 seconds under a limit of 30 is delivered whole
/// and keeps the budget (a second read waits) until it ends, and then the second read is served.
#[tokio::test(start_paused = true)]
async fn a_reader_that_makes_progress_keeps_its_budget() {
    let mut blobs = HashMap::new();
    let big = pattern(1, 3 * HELD_CHUNK_BYTES + 100);
    let t = build(&mut blobs, "a", std::slice::from_ref(&big), 0);
    let limits = Limits {
        read_budget_bytes: 4 * HELD_CHUNK_BYTES as u64,
        held_idle: Duration::from_secs(30),
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    let mut reply = rig.get(&t.files[0], &token).await;
    let mut body = reply.held_stream().expect("a held body").chunks;
    let mut second = Box::pin(rig.get(&t.files[0], &token));
    let mut got = 0;
    while let Some(piece) = body.next().await {
        got += piece
            .expect("a reader that makes progress is not cut off")
            .len();
        assert!(futures::poll!(second.as_mut()).is_pending());
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
    assert_eq!(got, big.len());
    assert_eq!(second.await.body.len(), big.len());
}

/// Catches: the idle limit covering a GET reply but not a streamed blob (a stalled reader of a
/// large blob holds its window of the budget for ever): with room for two windows, two streams
/// that are never read, and a third read that waits, the third is served after the limit and a
/// stalled stream then ends with `StreamError::Idle`.
#[tokio::test(start_paused = true)]
async fn a_stalled_stream_of_a_large_blob_gives_its_window_back() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..3).map(|i| pattern(i, 20 * 1024)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let limits = Limits {
        read_budget_bytes: 8 * 1024,
        stream_window: 4 * 1024,
        stream_over: 1024,
        held_idle: Duration::from_secs(30),
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let token = rig.token(t.action);
    let first = rig.get(&t.files[0], &token).await;
    let second = rig.get(&t.files[1], &token).await;
    let mut stalled = first.stream.expect("streamed").chunks;
    let _other = second.stream.expect("streamed");
    let third = rig.get(&t.files[2], &token).await;
    assert!(third.stream.is_some(), "served after the idle limit");
    assert!(matches!(stalled.next().await, Some(Err(StreamError::Idle))));
}

/// Catches: the batch read through a [`Backend`] (the source of a running frontend, which reads
/// a batch with its own `read_many`) answering anything but each blob's own bytes in order, a
/// missing blob as anything but `not_found`, and a reply of a streamed blob being unprintable
/// (it is logged when a request fails).
#[tokio::test]
async fn a_batch_reads_through_a_backend_and_a_streamed_reply_prints() {
    let mut blobs = HashMap::new();
    let contents: Vec<Vec<u8>> = (0..2).map(|i| pattern(i, 3000)).collect();
    let t = build(&mut blobs, "a", &contents, 0);
    let shared = rig(blobs.clone(), None, Limits::default());
    let cache = Arc::new(Cache::memory());
    let held: Vec<VerifiedBlob> = blobs
        .iter()
        .filter(|(digest, _)| **digest != t.files[1])
        .map(|(digest, bytes)| VerifiedBlob::new(*digest, bytes.clone()).expect("verified"))
        .collect();
    cache.store_blobs(held).await.expect("stored");
    let limits = Limits {
        stream_over: 1024,
        ..Limits::default()
    };
    let read = JobRead::new(
        Arc::new(Backend::from(cache)),
        Arc::clone(&shared.grants),
        limits,
    );
    let header = format!("Bearer {}", shared.token(t.action));
    let quoted: Vec<String> = t.files.iter().map(|d| format!("\"{d}\"")).collect();
    let body = format!("{{\"digests\":[{}]}}", quoted.join(","));
    let reply = read
        .respond_with_body(&Method::POST, BATCH_PATH, Some(&header), body.as_bytes())
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let expected = [frame(0, &contents[0]), vec![1, 0, 0, 0, 0, 0, 0, 0, 0]].concat();
    assert_eq!(&reply.body[..], &expected[..]);

    let streamed = read
        .respond(&Method::GET, &format!("/{}", t.files[0]), Some(&header))
        .await;
    let printed = format!("{streamed:?}");
    assert!(
        printed.contains("BlobStream") && printed.contains("3000"),
        "{printed}"
    );
}

#[path = "job_read_budget/failures.rs"]
mod failures;
