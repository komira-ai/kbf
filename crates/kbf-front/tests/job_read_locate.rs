//! `GET /locate` on the job read listener: the table of where the blobs of a grant's Action
//! are, over a source that has a pack index and large objects of its own to answer from.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_front::digest_to_proto;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BlobSource, JobRead, LOCATE_PATH, Limits, Reply, Span, large_key};
use kbf_proto::reapi::{Action, Directory, DirectoryNode, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use serde_json::Value;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0014";
const NOT_AFTER: u64 = 1_000;

#[derive(Default)]
struct Source {
    blobs: Mutex<HashMap<Digest, Bytes>>,
    spans: Mutex<HashMap<Digest, Span>>,
    /// The large objects tierstore holds, with their lengths.
    large: Mutex<HashMap<Digest, u64>>,
    reads: Mutex<Vec<Digest>>,
    asked: Mutex<Vec<Vec<Digest>>>,
    heads: Mutex<Vec<Digest>>,
    index_down: AtomicBool,
    blobs_down: AtomicBool,
    head_down: AtomicBool,
}

impl Source {
    fn reads_of(&self, digest: &Digest) -> usize {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.iter().filter(|read| *read == digest).count()
    }

    fn heads_of(&self, digest: &Digest) -> usize {
        let heads = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        heads.iter().filter(|head| *head == digest).count()
    }

    fn asked(&self) -> Vec<Vec<Digest>> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn down() -> StorageError {
    StorageError::Unavailable(Arc::new(tonic::Status::unavailable("down")))
}

impl BlobSource for Source {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            self.reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            tokio::task::yield_now().await;
            if self.blobs_down.load(Ordering::SeqCst) {
                return Err(down());
            }
            self.blobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))
        })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move { Err(StorageError::NotFound(*digest)) })
    }

    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        Box::pin(async move {
            self.asked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(digests.to_vec());
            if self.index_down.load(Ordering::SeqCst) {
                return Err(down());
            }
            let spans = self.spans.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(digests.iter().map(|d| spans.get(d).cloned()).collect())
        })
    }

    fn large_len<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<Option<u64>, StorageError>> {
        Box::pin(async move {
            self.heads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            if self.head_down.load(Ordering::SeqCst) {
                return Err(down());
            }
            let large = self.large.lock().unwrap_or_else(PoisonError::into_inner);
            Ok(large.get(digest).copied())
        })
    }
}

fn put(source: &Source, bytes: Vec<u8>) -> Digest {
    let digest = kbf_segments::sha256(&bytes);
    source
        .blobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(digest, Bytes::from(bytes));
    digest
}

/// An Action over a root with files of these contents and a subdirectory with one more file.
struct Built {
    action: Digest,
    command: Digest,
    root: Digest,
    sub: Digest,
    files: Vec<Digest>,
    deep: Digest,
}

impl Built {
    fn scope(&self) -> BTreeSet<Digest> {
        let mut all: BTreeSet<Digest> = self.files.iter().copied().collect();
        all.extend([self.action, self.command, self.root, self.sub, self.deep]);
        all
    }
}

fn build(source: &Source, tag: &str, contents: &[Vec<u8>]) -> Built {
    let files: Vec<Digest> = contents.iter().map(|c| put(source, c.clone())).collect();
    let deep = put(source, format!("deep file of {tag}").into_bytes());
    let sub = Directory {
        files: vec![FileNode {
            name: "deep".into(),
            digest: Some(digest_to_proto(&deep)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let sub = put(source, sub.encode_to_vec());
    let nodes = files
        .iter()
        .enumerate()
        .map(|(i, digest)| FileNode {
            name: format!("{tag}-{i}"),
            digest: Some(digest_to_proto(digest)),
            ..FileNode::default()
        })
        .collect();
    let root = Directory {
        files: nodes,
        directories: vec![DirectoryNode {
            name: "sub".into(),
            digest: Some(digest_to_proto(&sub)),
        }],
        ..Directory::default()
    };
    let root = put(source, root.encode_to_vec());
    let command = put(source, format!("command of {tag}").into_bytes());
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(source, action.encode_to_vec());
    Built {
        action,
        command,
        root,
        sub,
        files,
        deep,
    }
}

fn span(key: &str, offset: u64, length: u64) -> Span {
    Span {
        key: key.to_owned(),
        offset,
        length,
    }
}

fn pack(source: &Source, digest: &Digest, at: Span) {
    source
        .spans
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(*digest, at);
}

struct Rig {
    source: Arc<Source>,
    read: JobRead,
    grants: Arc<Grants>,
}

fn rig(source: Source, limits: Limits) -> Rig {
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::new(AtomicU64::new(100));
    let grants = Arc::new(Grants::new(
        key,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let source = Arc::new(source);
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

    async fn locate(&self, token: &str) -> Reply {
        let header = format!("Bearer {token}");
        self.read
            .respond(&Method::GET, LOCATE_PATH, Some(&header))
            .await
    }

    fn metrics(&self) -> String {
        let mut out = String::new();
        self.read.render_metrics(&mut out);
        out
    }
}

/// The lines of a 200 locate body, by digest text.
fn table(reply: &Reply) -> HashMap<String, Value> {
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let text = std::str::from_utf8(&reply.body).expect("utf-8");
    let mut lines = HashMap::new();
    for line in text.lines() {
        let value: Value = serde_json::from_str(line).expect("a JSON line");
        let digest = value["digest"].as_str().expect("a digest").to_owned();
        assert!(lines.insert(digest, value).is_none(), "a digest twice");
    }
    lines
}

fn small() -> Limits {
    Limits {
        stream_over: 1000,
        ..Limits::default()
    }
}

/// Catches: a table that names a digest outside the grant's scope (another Action's blobs
/// that the index also holds), omits one inside it, or gives a span other than the index's. The
/// index is asked once, for the scope's digests only and in one call.
#[tokio::test]
async fn the_table_is_exactly_the_scope_with_the_indexs_spans() {
    let source = Source::default();
    let a = build(
        &source,
        "a",
        &[b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()],
    );
    let b = build(&source, "b", &[b"other-1".to_vec(), b"other-2".to_vec()]);
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    pack(&source, &a.files[1], span("cas/f/w/1", 5, 4));
    pack(&source, &a.deep, span("cas/f/w/2", 77, 19));
    pack(&source, &a.command, span("cas/f/w/2", 0, 77));
    for digest in b.scope() {
        pack(&source, &digest, span("cas/f/w/9", 1, 1));
    }
    let rig = rig(source, small());
    let reply = rig.locate(&rig.token(a.action)).await;
    let table = table(&reply);

    let named: BTreeSet<String> = table.keys().cloned().collect();
    let expected: BTreeSet<String> = a.scope().iter().map(ToString::to_string).collect();
    assert_eq!(named, expected);
    for digest in b.scope() {
        assert!(!table.contains_key(&digest.to_string()));
    }
    let pack_of = |digest: &Digest| table[&digest.to_string()]["pack"].clone();
    assert_eq!(
        pack_of(&a.files[1]),
        serde_json::json!({"key": "cas/f/w/1", "offset": 5, "length": 4})
    );
    assert_eq!(
        pack_of(&a.deep),
        serde_json::json!({"key": "cas/f/w/2", "offset": 77, "length": 19})
    );
    assert_eq!(pack_of(&a.files[0])["offset"], 0);
    assert_eq!(pack_of(&a.command)["length"], 77);
    for absent in [a.files[2], a.action, a.root, a.sub] {
        assert_eq!(
            table[&absent.to_string()],
            serde_json::json!({"digest": absent.to_string(), "absent": true})
        );
    }
    let asked = rig.source.asked();
    assert_eq!(asked.len(), 1);
    let asked: BTreeSet<Digest> = asked[0].iter().copied().collect();
    assert_eq!(asked, a.scope());
}

/// Catches: a large key for a blob tierstore does not hold, or whose object is another length
/// than the digest's; a large blob also asked of the pack index; a small blob presence-checked.
#[tokio::test]
async fn a_present_large_blob_is_a_key_and_the_others_are_absent() {
    let source = Source::default();
    let a = build(
        &source,
        "a",
        &[
            vec![7; 2000],
            vec![8; 3000],
            vec![9; 4000],
            b"small".to_vec(),
        ],
    );
    source.large.lock().unwrap().insert(a.files[0], 2000);
    // An object of another length than the digest names.
    source.large.lock().unwrap().insert(a.files[2], 3999);
    pack(&source, &a.files[3], span("cas/f/w/1", 0, 5));
    let rig = rig(source, small());
    let table = table(&rig.locate(&rig.token(a.action)).await);

    assert_eq!(
        table[&a.files[0].to_string()]["large"],
        serde_json::json!({"key": large_key(&a.files[0]), "size": 2000})
    );
    assert!(large_key(&a.files[0]).starts_with("cas/l/"));
    assert_eq!(table[&a.files[1].to_string()]["absent"], true);
    assert_eq!(table[&a.files[2].to_string()]["absent"], true);
    assert!(table[&a.files[3].to_string()]["pack"].is_object());
    let asked: Vec<Digest> = rig.source.asked().concat();
    for large in &a.files[..3] {
        assert!(
            !asked.contains(large),
            "a large blob was asked of the index"
        );
        assert_eq!(rig.source.heads_of(large), 1);
    }
    assert_eq!(rig.source.heads_of(&a.files[3]), 0);
}

/// Catches: the empty blob sent to the index or answered as stored: it is `absent` and not asked
/// for.
#[tokio::test]
async fn the_empty_blob_is_absent_and_not_asked_for() {
    let source = Source::default();
    let a = build(&source, "a", &[vec![], b"x".to_vec()]);
    let rig = rig(source, small());
    let table = table(&rig.locate(&rig.token(a.action)).await);
    assert_eq!(table[&a.files[0].to_string()]["absent"], true);
    assert!(!rig.source.asked().concat().contains(&a.files[0]));
}

/// Catches: a grant that gets another Action's table: each grant is answered its own scope, and
/// an Action whose blobs are not in the store has none (403, as a GET), however its grant
/// verifies; the refusals of a GET (401, 405) and a store that cannot answer (503) are the
/// listener's.
#[tokio::test]
async fn grants_are_answered_their_own_scope_and_refused_as_a_get_is() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    let b = build(&source, "b", &[b"other".to_vec()]);
    let rig = rig(source, small());

    let for_b = table(&rig.locate(&rig.token(b.action)).await);
    let named: BTreeSet<String> = for_b.keys().cloned().collect();
    let expected: BTreeSet<String> = b.scope().iter().map(ToString::to_string).collect();
    assert_eq!(named, expected);
    assert!(!for_b.contains_key(&a.action.to_string()));

    let unknown = kbf_segments::sha256(b"an action nobody stored");
    let reply = rig.locate(&rig.token(unknown)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(&reply.body[..], b"no_scope\n");

    let none = rig.read.respond(&Method::GET, LOCATE_PATH, None).await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    assert_eq!(&none.body[..], b"missing_token\n");
    let forged = rig.locate("forged").await;
    assert_eq!(forged.status, StatusCode::UNAUTHORIZED);
    assert_eq!(&forged.body[..], b"invalid_token\n");
    let header = format!("Bearer {}", rig.token(a.action));
    let post = rig
        .read
        .respond(&Method::POST, LOCATE_PATH, Some(&header))
        .await;
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);

    rig.source.index_down.store(true, Ordering::SeqCst);
    let down = rig.locate(&rig.token(a.action)).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(&down.body[..], b"store_unavailable\n");
}

/// Catches: a failed presence check answered as absent (and remembered): a store that cannot be
/// asked about a large object is 503, and the next request asks again.
#[tokio::test]
async fn a_failed_presence_check_is_unavailable_and_not_remembered() {
    let source = Source::default();
    let a = build(&source, "a", &[vec![7; 2000]]);
    source.large.lock().unwrap().insert(a.files[0], 2000);
    source.head_down.store(true, Ordering::SeqCst);
    let rig = rig(source, small());
    let token = rig.token(a.action);
    assert_eq!(
        rig.locate(&token).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    rig.source.head_down.store(false, Ordering::SeqCst);
    let table = table(&rig.locate(&token).await);
    assert!(table[&a.files[0].to_string()]["large"].is_object());
}

/// Catches: a closure walked again for each request: the Action and every Directory are read
/// once across two locates and a GET of the same Action.
#[tokio::test]
async fn requests_of_one_action_walk_its_closure_once() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    let rig = rig(source, small());
    let token = rig.token(a.action);
    let first = rig.locate(&token).await;
    let second = rig.locate(&token).await;
    assert_eq!(first.body, second.body);
    let header = format!("Bearer {token}");
    let get = rig
        .read
        .respond(&Method::GET, &format!("/{}", a.command), Some(&header))
        .await;
    assert_eq!(get.status, StatusCode::OK);
    for walked in [a.action, a.root, a.sub] {
        assert_eq!(rig.source.reads_of(&walked), 1, "{walked}");
    }
}

/// Catches: answers remembered like the scope: the table is built for each request, so a blob
/// that moved between two requests is answered where it is now (and the old span, which a task
/// may still hold, is the task's to verify: a stale span is by design).
#[tokio::test]
async fn the_spans_are_looked_up_afresh_for_each_request() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    let rig = rig(source, small());
    let token = rig.token(a.action);
    let before = table(&rig.locate(&token).await);
    assert_eq!(before[&a.files[0].to_string()]["pack"]["key"], "cas/f/w/1");
    pack(&rig.source, &a.files[0], span("cas/f/w/compacted", 40, 5));
    let after = table(&rig.locate(&token).await);
    assert_eq!(
        after[&a.files[0].to_string()]["pack"],
        serde_json::json!({"key": "cas/f/w/compacted", "offset": 40, "length": 5})
    );
    assert_eq!(rig.source.asked().len(), 2);
}

/// Catches: a presence check that is never remembered (a HEAD for every request) or is
/// remembered for ever: a held object and a missing one are each checked once within their
/// times, and again after them; an object written after a negative answer is found once that
/// answer has run out.
#[tokio::test(start_paused = true)]
async fn presence_is_remembered_for_seconds_and_a_negative_for_less() {
    let source = Source::default();
    let a = build(&source, "a", &[vec![7; 2000], vec![8; 3000]]);
    source.large.lock().unwrap().insert(a.files[0], 2000);
    let limits = Limits {
        locate_present_for: Duration::from_secs(10),
        locate_absent_for: Duration::from_secs(2),
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let (held, missing) = (a.files[0], a.files[1]);

    rig.locate(&token).await;
    rig.locate(&token).await;
    assert_eq!(
        (rig.source.heads_of(&held), rig.source.heads_of(&missing)),
        (1, 1)
    );

    // The missing blob is written; its negative answer is still good.
    rig.source.large.lock().unwrap().insert(missing, 3000);
    tokio::time::advance(Duration::from_secs(1)).await;
    let table_now = table(&rig.locate(&token).await);
    assert_eq!(table_now[&missing.to_string()]["absent"], true);

    tokio::time::advance(Duration::from_secs(2)).await;
    let table_later = table(&rig.locate(&token).await);
    assert!(table_later[&missing.to_string()]["large"].is_object());
    assert_eq!(
        (rig.source.heads_of(&held), rig.source.heads_of(&missing)),
        (1, 2)
    );

    // The held object's answer runs out later, and the object is gone by then.
    rig.source.large.lock().unwrap().remove(&held);
    tokio::time::advance(Duration::from_secs(8)).await;
    let table_last = table(&rig.locate(&token).await);
    assert_eq!(table_last[&held.to_string()]["absent"], true);
    assert_eq!(rig.source.heads_of(&held), 2);
}

/// Catches: counts missing or wrong: requests by outcome, and the entries answered by kind.
#[tokio::test]
async fn requests_and_entries_are_counted() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec(), vec![7; 2000]]);
    source.large.lock().unwrap().insert(a.files[1], 2000);
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    let rig = rig(source, small());
    rig.locate(&rig.token(a.action)).await;
    rig.locate("forged").await;
    let metrics = rig.metrics();
    for line in [
        "kbf_job_locate_requests_total{outcome=\"ok\"} 1",
        "kbf_job_locate_requests_total{outcome=\"unauthorized\"} 1",
        "kbf_job_locate_requests_total{outcome=\"forbidden\"} 0",
        "kbf_job_locate_entries_total{kind=\"pack\"} 1",
        "kbf_job_locate_entries_total{kind=\"large\"} 1",
        // The Action, Command, root, sub and deep file.
        "kbf_job_locate_entries_total{kind=\"absent\"} 5",
    ] {
        assert!(metrics.lines().any(|l| l == line), "{line} in\n{metrics}");
    }
}

/// Catches: a body that the budget is not held for, or that the listener cannot send in pieces:
/// the reply holds the body, and its held stream carries the same bytes.
#[tokio::test]
async fn the_table_is_sent_from_a_held_stream() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    let rig = rig(source, small());
    let mut reply = rig.locate(&rig.token(a.action)).await;
    let body = reply.body.clone();
    let stream = reply.held_stream().expect("a reply that holds its bytes");
    assert_eq!(stream.size, body.len() as u64);
    let sent: Vec<u8> = stream
        .chunks
        .map(|chunk| chunk.expect("a chunk").to_vec())
        .concat()
        .await;
    assert_eq!(sent, body.to_vec());
}

/// A source that implements only the reads: its locate answers are the defaults of the trait.
struct Bare(Source);

impl BlobSource for Bare {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        self.0.read_blob(digest)
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        self.0.read_stream(digest)
    }
}

/// Catches: a source without packs or large objects that answers a table with anything but
/// `absent` for every digest of the scope, or fails the request.
#[tokio::test]
async fn a_source_that_cannot_locate_answers_every_digest_absent() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec(), vec![7; 2000]]);
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::new(AtomicU64::new(100));
    let grants = Arc::new(Grants::new(
        key,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ));
    let token = grants
        .issue(BUILD, a.action, NOT_AFTER)
        .expect("a valid build id")
        .expose()
        .to_owned();
    let read = JobRead::new(Arc::new(Bare(source)), grants, small());
    let header = format!("Bearer {token}");
    let reply = read.respond(&Method::GET, LOCATE_PATH, Some(&header)).await;
    let table = table(&reply);
    assert_eq!(table.len(), a.scope().len());
    assert!(table.values().all(|line| line["absent"] == true));
}

/// Catches: a store that cannot read the Action answered as a table or as `no_scope`: it is 503
/// `store_unavailable`, as a GET is.
#[tokio::test]
async fn a_scope_that_cannot_be_read_is_unavailable() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    source.blobs_down.store(true, Ordering::SeqCst);
    let rig = rig(source, small());
    let reply = rig.locate(&rig.token(a.action)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(&reply.body[..], b"store_unavailable\n");
}

/// Catches: a presence memory that grows without bound: a scope of more large blobs than the
/// memory holds is answered whole and correctly, and the memory is forgotten at its bound (the
/// next request asks about nearly all of them again).
#[tokio::test]
async fn a_scope_of_many_large_blobs_is_answered_whole() {
    const MANY: usize = 4100;
    let source = Source::default();
    let contents: Vec<Vec<u8>> = (0..MANY)
        .map(|i| format!("{i:01100}").into_bytes())
        .collect();
    let a = build(&source, "a", &contents);
    for file in &a.files {
        source.large.lock().unwrap().insert(*file, 1100);
    }
    let rig = rig(source, small());
    let token = rig.token(a.action);
    let table = table(&rig.locate(&token).await);
    assert!(
        a.files
            .iter()
            .all(|f| table[&f.to_string()]["large"].is_object())
    );
    // Every line is one digest of the scope, once: none missing, none extra.
    let named: BTreeSet<String> = table.keys().cloned().collect();
    let expected: BTreeSet<String> = a.scope().iter().map(ToString::to_string).collect();
    assert_eq!(named, expected);
    // The files and the root Directory (which is itself over the size) are each asked about.
    let asked = MANY + 1;
    let heads = |rig: &Rig| rig.source.heads.lock().unwrap().len();
    assert_eq!(heads(&rig), asked);
    assert_eq!(table[&a.root.to_string()]["absent"], true);
    rig.locate(&token).await;
    assert!(heads(&rig) >= asked + 4096, "{}", heads(&rig));
}
