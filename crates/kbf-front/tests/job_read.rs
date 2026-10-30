//! The job-facing read: a task fetches the inputs of its Action with a read grant.
//!
//! Every test runs on an injected clock and a fake blob source that records what it was
//! asked to read, so none waits or reads the wall, and "reads nothing" is checked on the
//! source's own log.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BlobSource, JobRead, Limits, Reply, STREAM_OVER_BYTES};
use kbf_front::{Backend, Cache, VerifiedBlob, digest_to_proto};
use kbf_proto::reapi::{Action, Directory, DirectoryNode, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use tokio::sync::Semaphore;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0007";
const NOT_AFTER: u64 = 1_000;

/// A source over a map of blobs: it logs every read, can be broken, and can hold the reads
/// of one digest until it is given a permit.
struct Fake {
    blobs: Mutex<HashMap<Digest, Bytes>>,
    reads: Mutex<Vec<Digest>>,
    broken: AtomicBool,
    gate: Option<(Digest, Semaphore)>,
}

impl Fake {
    fn new(blobs: HashMap<Digest, Bytes>, gate: Option<Digest>) -> Self {
        Self {
            blobs: Mutex::new(blobs),
            reads: Mutex::new(Vec::new()),
            broken: AtomicBool::new(false),
            gate: gate.map(|digest| (digest, Semaphore::new(0))),
        }
    }

    fn reads_of(&self, digest: &Digest) -> usize {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.iter().filter(|read| *read == digest).count()
    }

    fn total_reads(&self) -> usize {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn forget(&self, digest: &Digest) {
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.remove(digest);
    }
}

impl BlobSource for Fake {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            self.reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            if self.broken.load(Ordering::SeqCst) {
                return Err(StorageError::Unreachable(*digest));
            }
            if let Some((held, gate)) = &self.gate
                && held == digest
            {
                gate.acquire().await.expect("the gate is open").forget();
            }
            let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
            blobs
                .get(digest)
                .cloned()
                .ok_or(StorageError::NotFound(*digest))
        })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move {
            let blob = {
                let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
                blobs.get(digest).cloned()
            };
            let blob = blob.ok_or(StorageError::NotFound(*digest))?;
            Ok(futures::stream::once(async move { Ok(blob) }).boxed())
        })
    }
}

fn put(blobs: &mut HashMap<Digest, Bytes>, bytes: &[u8]) -> Digest {
    let digest = kbf_segments::sha256(bytes);
    blobs.insert(digest, Bytes::copy_from_slice(bytes));
    digest
}

fn file(name: &str, digest: &Digest) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest_to_proto(digest)),
        ..FileNode::default()
    }
}

fn subdirectory(name: &str, digest: &Digest) -> DirectoryNode {
    DirectoryNode {
        name: name.to_owned(),
        digest: Some(digest_to_proto(digest)),
    }
}

fn path(digest: &Digest) -> String {
    format!("/{digest}")
}

/// The tree every test but the malformed ones reads: a root with a file, an empty file, a file
/// too large to serve, a file exactly at the cap and a subdirectory with one file.
struct Tree {
    action: Digest,
    command: Digest,
    root: Digest,
    lib: Digest,
    a: Digest,
    b: Digest,
    empty: Digest,
    big: Digest,
    at_cap: Digest,
    /// In the store, and in no scope.
    secret: Digest,
    blobs: HashMap<Digest, Bytes>,
}

fn tree() -> Tree {
    let mut blobs = HashMap::new();
    let a = put(&mut blobs, b"alpha");
    let b = put(&mut blobs, b"beta, in a subdirectory");
    let empty = kbf_segments::sha256(b"");
    // Held by no one: their size alone decides.
    let big = Digest::new(DigestFunction::Sha256, [9; 32], STREAM_OVER_BYTES + 1);
    let at_cap = Digest::new(DigestFunction::Sha256, [8; 32], STREAM_OVER_BYTES);
    let lib = Directory {
        files: vec![file("b", &b)],
        ..Directory::default()
    };
    let lib = put(&mut blobs, &lib.encode_to_vec());
    let root = Directory {
        files: vec![
            file("a", &a),
            file("empty", &empty),
            file("big", &big),
            file("at_cap", &at_cap),
        ],
        // Named twice: the walk reads a Directory once however often it is named.
        directories: vec![subdirectory("lib", &lib), subdirectory("again", &lib)],
        ..Directory::default()
    };
    let root = put(&mut blobs, &root.encode_to_vec());
    let command = put(&mut blobs, b"the command");
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(&mut blobs, &action.encode_to_vec());
    let secret = put(&mut blobs, b"another tenant's bytes");
    Tree {
        action,
        command,
        root,
        lib,
        a,
        b,
        empty,
        big,
        at_cap,
        secret,
        blobs,
    }
}

/// A job-facing read over a [`Fake`] and grants on a clock set to 100.
struct Rig {
    fake: Arc<Fake>,
    read: JobRead,
    grants: Arc<Grants>,
    clock: Arc<AtomicU64>,
}

fn grants_under(fill: u8, clock: &Arc<AtomicU64>) -> Arc<Grants> {
    let key = GrantKey::from_secret(&[fill; MIN_SECRET_LEN]).expect("a long enough secret");
    let clock = Arc::clone(clock);
    Arc::new(Grants::new(
        key,
        Arc::new(move || clock.load(Ordering::SeqCst)),
    ))
}

fn rig(blobs: HashMap<Digest, Bytes>, gate: Option<Digest>, limits: Limits) -> Rig {
    let clock = Arc::new(AtomicU64::new(100));
    let grants = grants_under(1, &clock);
    let fake = Arc::new(Fake::new(blobs, gate));
    let read = JobRead::new(fake.clone(), Arc::clone(&grants), limits);
    Rig {
        fake,
        read,
        grants,
        clock,
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

    async fn get(&self, path: &str, token: Option<&str>) -> Reply {
        let header = token.map(|token| format!("Bearer {token}"));
        self.read
            .respond(&Method::GET, path, header.as_deref())
            .await
    }
}

/// The bounds under which a blob over [`STREAM_OVER_BYTES`] is too large, as the listener's cap
/// was before large blobs were streamed.
fn capped() -> Limits {
    Limits {
        max_blob_bytes: STREAM_OVER_BYTES,
        ..Limits::default()
    }
}

fn text(reply: &Reply) -> &str {
    std::str::from_utf8(&reply.body).expect("a reason code is text")
}

/// Catches: a listener that serves the wrong bytes or a wrong length, and a scope that
/// leaves out the Action, its Command, the Directory blobs or a file of a subdirectory.
#[tokio::test]
async fn a_valid_grant_serves_the_exact_bytes_of_everything_in_its_scope() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    for digest in [t.a, t.b, t.lib, t.root, t.command, t.action] {
        let reply = rig.get(&path(&digest), Some(&token)).await;
        assert_eq!(reply.status, StatusCode::OK, "{digest}: {}", text(&reply));
        assert_eq!(reply.body, t.blobs[&digest], "{digest}");
        assert_eq!(reply.body.len() as u64, digest.size_bytes, "{digest}");
    }
}

/// Catches: a grant that reads a blob outside its Action's scope (another tenant's bytes), and
/// a refusal that reads the blob first and then withholds it.
#[tokio::test]
async fn a_digest_outside_the_grant_scope_is_forbidden_and_reads_nothing() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    let reply = rig.get(&path(&t.secret), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(text(&reply), "out_of_scope\n");
    assert_eq!(rig.fake.reads_of(&t.secret), 0);
}

/// Catches: an expiry that is not enforced, one enforced a second late (a grant is refused at
/// its `not_after`), and a refusal that carries more than its code.
#[tokio::test]
async fn an_expired_grant_is_unauthorized_at_its_second() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    rig.clock.store(NOT_AFTER - 1, Ordering::SeqCst);
    assert_eq!(
        rig.get(&path(&t.a), Some(&token)).await.status,
        StatusCode::OK
    );
    rig.clock.store(NOT_AFTER, Ordering::SeqCst);
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(text(&reply), "expired\n");
}

/// Catches: a token of another farm's key, a changed token and a garbled one being served.
#[tokio::test]
async fn a_forged_or_garbled_token_is_unauthorized() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let clock = Arc::new(AtomicU64::new(100));
    let foreign = grants_under(2, &clock)
        .issue(BUILD, t.action, NOT_AFTER)
        .expect("a token")
        .expose()
        .to_owned();
    let mut flipped = rig.token(t.action);
    let last = flipped.pop().expect("a token has a last character");
    flipped.push(if last == '0' { '1' } else { '0' });
    for token in [foreign, flipped, "not-a-token".to_owned()] {
        let reply = rig.get(&path(&t.a), Some(&token)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{token}");
        assert_eq!(text(&reply), "invalid_token\n", "{token}");
    }
    assert_eq!(rig.fake.total_reads(), 0, "nothing is read for a bad token");
}

/// Catches: a request without a grant being served, a scheme other than Bearer taken for one,
/// and a Bearer scheme in lower case refused.
#[tokio::test]
async fn a_request_without_a_bearer_grant_is_unauthorized() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    let uri = path(&t.a);
    let headers = [
        None,
        Some("Basic dXNlcjpwYXNz".to_owned()),
        Some("Bearer".to_owned()),
    ];
    for header in headers {
        let reply = rig
            .read
            .respond(&Method::GET, &uri, header.as_deref())
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{header:?}");
        assert_eq!(text(&reply), "missing_token\n", "{header:?}");
    }
    let lower = format!("bearer {token}");
    let reply = rig.read.respond(&Method::GET, &uri, Some(&lower)).await;
    assert_eq!(reply.status, StatusCode::OK);
}

/// Catches: a grant that works after its build was revoked.
#[tokio::test]
async fn a_revoked_build_is_unauthorized() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    rig.grants.revoke(BUILD, NOT_AFTER);
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(text(&reply), "revoked\n");
}

/// Catches: a route other than `GET /<hex>/<size>` answering (the listener serves nothing
/// else), and a write method taken for a read. Nothing is read for any of them.
#[tokio::test]
async fn an_unknown_path_is_not_found_and_another_method_is_not_allowed() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    let header = format!("Bearer {token}");
    let hex = t.a.hash_hex();
    let odd = [
        "/".to_owned(),
        "/metrics".to_owned(),
        "/v1/frontend/drain".to_owned(),
        "/zz/1".to_owned(),
        format!("/{hex}"),
        format!("{hex}/5"),
        format!("/{}/{}", hex.to_uppercase(), t.a.size_bytes),
        format!("/{hex}/5/extra"),
    ];
    for odd in &odd {
        let reply = rig.read.respond(&Method::GET, odd, Some(&header)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{odd}");
        assert_eq!(text(&reply), "not_found\n", "{odd}");
    }
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::HEAD] {
        let reply = rig.read.respond(&method, &path(&t.a), Some(&header)).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(text(&reply), "method_not_allowed\n", "{method}");
    }
    assert_eq!(rig.fake.total_reads(), 0);
}

/// Catches: a blob in the scope but not in the store answered as anything but 404, and a
/// missing blob spoiling the blobs beside it.
#[tokio::test]
async fn a_blob_not_in_the_store_is_not_found() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    rig.fake.forget(&t.b);
    let reply = rig.get(&path(&t.b), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(text(&reply), "not_found\n");
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
}

/// Catches: a response over the cap (here the old 64 MiB) being read and sent, a cap that is
/// off by one (a blob of exactly the cap is read), and the check made after the read.
#[tokio::test]
async fn a_blob_over_the_cap_is_too_large_and_is_not_read() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, capped());
    let token = rig.token(t.action);
    let reply = rig.get(&path(&t.big), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(text(&reply), "too_large\n");
    assert_eq!(rig.fake.reads_of(&t.big), 0);
    let reply = rig.get(&path(&t.at_cap), Some(&token)).await;
    assert_eq!(
        reply.status,
        StatusCode::NOT_FOUND,
        "the cap itself is read"
    );
    assert_eq!(rig.fake.reads_of(&t.at_cap), 1);
}

/// Catches: the empty blob being read from a store that never holds it, or refused: an input
/// that is an empty file is served as an empty body.
#[tokio::test]
async fn an_empty_file_is_served_empty_without_a_read() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    let reply = rig.get(&path(&t.empty), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
    assert_eq!(rig.fake.reads_of(&t.empty), 0);
}

/// Catches: an Action with no inputs (its input root is the empty Directory, which the store
/// never holds) having no scope, so its own Command could not be read.
#[tokio::test]
async fn an_action_with_no_inputs_reads_its_command() {
    let mut blobs = HashMap::new();
    let command = put(&mut blobs, b"a command");
    let empty = kbf_segments::sha256(b"");
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&empty)),
        ..Action::default()
    };
    let action = put(&mut blobs, &action.encode_to_vec());
    let rig = rig(blobs, None, Limits::default());
    let token = rig.token(action);
    let reply = rig.get(&path(&command), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(rig.fake.reads_of(&empty), 0);
}

/// Catches: the Action and its tree being read again for every request of a task that fetches
/// thousands of inputs.
#[tokio::test]
async fn the_scope_of_an_action_is_worked_out_once() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    for digest in [t.a, t.b, t.a] {
        assert_eq!(
            rig.get(&path(&digest), Some(&token)).await.status,
            StatusCode::OK
        );
    }
    for walked in [t.action, t.root, t.lib] {
        assert_eq!(rig.fake.reads_of(&walked), 1, "{walked}");
    }
}

/// Catches: a memory of scopes that grows without a bound, and one that forgets an Action it
/// should keep: at a bound of one Action, the second Action's scope replaces the first's.
#[tokio::test]
async fn the_memory_of_scopes_is_bounded() {
    let t = tree();
    let mut blobs = t.blobs.clone();
    let other = Action {
        command_digest: Some(digest_to_proto(&t.command)),
        input_root_digest: Some(digest_to_proto(&t.lib)),
        ..Action::default()
    };
    let other = put(&mut blobs, &other.encode_to_vec());
    let limits = Limits {
        scopes: 1,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let (first, second) = (rig.token(t.action), rig.token(other));
    for (digest, token) in [(t.a, &first), (t.b, &second), (t.a, &first)] {
        let reply = rig.get(&path(&digest), Some(token)).await;
        assert_eq!(reply.status, StatusCode::OK, "{digest}");
    }
    assert_eq!(
        rig.fake.reads_of(&t.action),
        2,
        "worked out again after the other"
    );
    assert_eq!(rig.fake.reads_of(&other), 1);
}

/// Catches: an Action that is not in the store being given a scope, and a grant for it being
/// told anything but that it has none.
#[tokio::test]
async fn an_action_that_is_not_in_the_store_has_no_scope() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let absent = Digest::new(DigestFunction::Sha256, [5; 32], 10);
    let token = rig.token(absent);
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(text(&reply), "no_scope\n");
}

/// Catches: an Action or a Directory that is not what it must be (not a message, no Command, no
/// input root, a file or a subdirectory with no digest) giving a scope anyway.
#[tokio::test]
async fn an_action_whose_blobs_are_malformed_has_no_scope() {
    let garbage: &[u8] = &[0xff, 0xff, 0xff];
    let command = kbf_segments::sha256(b"c");
    let action_of = |command: Option<&Digest>, root: Option<&Digest>| {
        Action {
            command_digest: command.map(digest_to_proto),
            input_root_digest: root.map(digest_to_proto),
            ..Action::default()
        }
        .encode_to_vec()
    };
    let garbage_root = kbf_segments::sha256(garbage);
    let nameless_file = Directory {
        files: vec![FileNode::default()],
        ..Directory::default()
    }
    .encode_to_vec();
    let nameless_dir = Directory {
        directories: vec![DirectoryNode::default()],
        ..Directory::default()
    }
    .encode_to_vec();
    let (file_root, dir_root) = (
        kbf_segments::sha256(&nameless_file),
        kbf_segments::sha256(&nameless_dir),
    );
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        ("garbage action", vec![garbage.to_vec()]),
        ("no command", vec![action_of(None, Some(&command))]),
        ("no root", vec![action_of(Some(&command), None)]),
        (
            "garbage root",
            vec![
                action_of(Some(&command), Some(&garbage_root)),
                garbage.to_vec(),
            ],
        ),
        (
            "a file with no digest",
            vec![action_of(Some(&command), Some(&file_root)), nameless_file],
        ),
        (
            "a subdirectory with no digest",
            vec![action_of(Some(&command), Some(&dir_root)), nameless_dir],
        ),
    ];
    for (what, stored) in cases {
        let mut blobs = HashMap::new();
        let digests: Vec<Digest> = stored.iter().map(|bytes| put(&mut blobs, bytes)).collect();
        let rig = rig(blobs, None, Limits::default());
        let token = rig.token(digests[0]);
        let reply = rig.get(&path(&digests[0]), Some(&token)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{what}");
        assert_eq!(text(&reply), "no_scope\n", "{what}");
    }
}

/// Catches: an input tree of any size being walked into memory: past the bound the Action has
/// no scope.
#[tokio::test]
async fn an_input_tree_past_the_bound_gives_no_scope() {
    let t = tree();
    let limits = Limits {
        scope_digests: 3,
        ..Limits::default()
    };
    let rig = rig(t.blobs.clone(), None, limits);
    let token = rig.token(t.action);
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(text(&reply), "no_scope\n");
}

/// Catches: a store that fails being answered as a refusal of the grant (403 or 404 would
/// tell the task its inputs are gone), and a failed scope being remembered, so that the task
/// is refused for ever after one bad moment.
#[tokio::test]
async fn a_store_that_fails_is_unavailable_and_is_tried_again() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, Limits::default());
    let token = rig.token(t.action);
    rig.fake.broken.store(true, Ordering::SeqCst);
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(text(&reply), "store_unavailable\n");
    rig.fake.broken.store(false, Ordering::SeqCst);
    let reply = rig.get(&path(&t.a), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    // The scope is there now: a store that fails at the blob is unavailable too.
    rig.fake.broken.store(true, Ordering::SeqCst);
    let reply = rig.get(&path(&t.b), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
}

/// Catches: no bound on the reads open at once (a task fetching thousands of inputs, or many
/// tasks, holding as many blobs in memory): with one read allowed, a second waits for the
/// first, and both are then answered.
#[tokio::test]
async fn the_bound_on_reads_holds_a_second_read_until_the_first_ends() {
    let t = tree();
    let limits = Limits {
        reads_in_flight: 1,
        ..Limits::default()
    };
    let rig = rig(t.blobs.clone(), Some(t.a), limits);
    let token = rig.token(t.action);
    let (path_a, path_b) = (path(&t.a), path(&t.b));
    let first = rig.get(&path_a, Some(&token));
    let second = rig.get(&path_b, Some(&token));
    let driver = async {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(rig.fake.reads_of(&t.a), 1, "the first read is open");
        assert_eq!(rig.fake.reads_of(&t.b), 0, "the second waits for it");
        let (_, gate) = rig.fake.gate.as_ref().expect("a gate");
        gate.add_permits(1);
    };
    let (first, second, ()) = tokio::join!(first, second, driver);
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(second.body, t.blobs[&t.b]);
}

/// Catches: requests counted under the wrong outcome, bytes counted for a refusal, and a scrape
/// of a frontend with no job-facing read that is not empty.
#[tokio::test]
async fn the_metrics_count_each_outcome_and_the_bytes_served() {
    let t = tree();
    let rig = rig(t.blobs.clone(), None, capped());
    let token = rig.token(t.action);
    let uri = path(&t.a);
    let header = format!("Bearer {token}");
    let _ = rig.get(&uri, Some(&token)).await;
    let _ = rig.get(&path(&t.b), Some(&token)).await;
    let _ = rig.get(&uri, None).await;
    let _ = rig.get(&path(&t.secret), Some(&token)).await;
    let _ = rig.get(&path(&t.at_cap), Some(&token)).await;
    let _ = rig.get(&path(&t.big), Some(&token)).await;
    let _ = rig.read.respond(&Method::POST, &uri, Some(&header)).await;
    rig.fake.broken.store(true, Ordering::SeqCst);
    let _ = rig.get(&path(&t.root), Some(&token)).await;
    let mut out = "# before\n".to_owned();
    rig.read.render_metrics(&mut out);
    let served = (t.a.size_bytes + t.b.size_bytes).to_string();
    let bytes_line = format!("kbf_job_read_bytes_total {served}");
    for line in [
        "# before",
        "kbf_job_read_requests_total{outcome=\"ok\"} 2",
        "kbf_job_read_requests_total{outcome=\"unauthorized\"} 1",
        "kbf_job_read_requests_total{outcome=\"forbidden\"} 1",
        "kbf_job_read_requests_total{outcome=\"not_found\"} 1",
        "kbf_job_read_requests_total{outcome=\"too_large\"} 1",
        "kbf_job_read_requests_total{outcome=\"method_not_allowed\"} 1",
        "kbf_job_read_requests_total{outcome=\"unavailable\"} 1",
        bytes_line.as_str(),
    ] {
        assert!(out.lines().any(|l| l == line), "{line} is not in\n{out}");
    }
    let mut none = "# nothing\n".to_owned();
    JobRead::render_metrics_of(None, &mut none);
    assert_eq!(none, "# nothing\n");
    JobRead::render_metrics_of(Some(&rig.read), &mut none);
    assert!(none.contains("kbf_job_read_bytes_total"), "{none}");
}

/// Catches: the listener reading from anywhere but the frontend's own blob path: over a
/// [`Backend`] on a cache, a stored blob is served and a blob in scope that is not stored is
/// 404.
#[tokio::test]
async fn blobs_are_read_through_the_backend() {
    let t = tree();
    let cache = Arc::new(Cache::memory());
    let held: Vec<VerifiedBlob> = t
        .blobs
        .iter()
        .map(|(digest, bytes)| VerifiedBlob::new(*digest, bytes.clone()).expect("verified"))
        .collect();
    cache.store_blobs(held).await.expect("stored");
    let clock = Arc::new(AtomicU64::new(100));
    let grants = grants_under(1, &clock);
    let token = grants
        .issue(BUILD, t.action, NOT_AFTER)
        .expect("a token")
        .expose()
        .to_owned();
    let read = JobRead::new(Arc::new(Backend::from(cache)), grants, Limits::default());
    let header = format!("Bearer {token}");
    let reply = read.respond(&Method::GET, &path(&t.b), Some(&header)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, t.blobs[&t.b]);
    let reply = read
        .respond(&Method::GET, &path(&t.at_cap), Some(&header))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}
