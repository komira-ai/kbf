//! The job-facing read's byte budget, its single-flight scope walk, and the batch's read
//! through [`BlobSource::read_many`].
//!
//! The source yields while a read is open, so that reads that are allowed to overlap do, and
//! records the most reads, and the most bytes of them, that were ever open at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::{BoxFuture, join_all};
use futures::{StreamExt as _, stream};
use kbf_front::digest_to_proto;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BATCH_PATH, BlobSource, JobRead, Limits, Reply};
use kbf_proto::reapi::{Action, Directory, FileNode};
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
    gate: Option<(Digest, Semaphore)>,
}

impl Source {
    fn reads_of(&self, digest: &Digest) -> usize {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.iter().filter(|read| *read == digest).count()
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
            self.blobs
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))
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
            digests
                .iter()
                .map(|d| self.blobs.get(d).cloned().ok_or(StorageError::NotFound(*d)))
                .collect()
        })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move {
            let blob = self
                .blobs
                .get(digest)
                .ok_or(StorageError::NotFound(*digest))?;
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
        let replies = join_all(t.files.iter().map(|d| rig.get(d, &token))).await;
        assert!(
            replies.iter().all(|r| r.status == StatusCode::OK),
            "{budget}"
        );
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
    let replies = join_all(
        built
            .iter()
            .zip(&tokens)
            .map(|(b, token)| rig.get(&b.command, token)),
    )
    .await;
    assert!(replies.iter().all(|r| r.status == StatusCode::OK));
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
    let batch = rig.batch(&[t.files[0]], &token);
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
