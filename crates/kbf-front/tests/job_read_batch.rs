//! `POST /batch` on the job-facing read: many small blobs of one Action in one request.
//!
//! The world is the one of the golden fixture (`data/job_read_batch.golden`): its blobs, its
//! Action and its tree, so that the fixture a client author replays is the very data these
//! tests check the listener against.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::digest_to_proto;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{
    BATCH_MAX_BYTES, BATCH_MAX_DIGESTS, BATCH_PATH, BlobSource, JobRead, Limits,
    MAX_BATCH_REQUEST_BYTES, Reply,
};
use kbf_proto::reapi::{Action, Directory, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use tokio::sync::Semaphore;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0011";
const NOT_AFTER: u64 = 1_000;
const FIXTURE: &str = include_str!("data/job_read_batch.golden");

/// What a fixture case expects.
enum Expect {
    Response(Vec<u8>),
    Refused(u16, String),
}

struct Case {
    name: String,
    request: String,
    expect: Expect,
}

/// A line of the fixture that says how a blob is held.
struct Held {
    name: String,
    class: String,
    bytes: Vec<u8>,
}

fn unhex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

struct Fixture {
    blobs: Vec<Held>,
    sized: Vec<(String, Digest)>,
    cases: Vec<Case>,
}

fn fixture() -> Fixture {
    let mut parsed = Fixture {
        blobs: Vec::new(),
        sized: Vec::new(),
        cases: Vec::new(),
    };
    let mut name = String::new();
    let mut request = String::new();
    for line in FIXTURE.lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "blob" => {
                let mut parts = rest.splitn(3, ' ');
                let (n, class) = (parts.next().expect("name"), parts.next().expect("class"));
                parsed.blobs.push(Held {
                    name: n.to_owned(),
                    class: class.to_owned(),
                    bytes: unhex(parts.next().unwrap_or("")),
                });
            }
            "sized" => {
                let mut parts = rest.split(' ');
                let n = parts.next().expect("name").to_owned();
                let hash = parts.next().expect("hash");
                let size = parts.next().expect("size");
                let digest = Digest::parse(DigestFunction::Sha256, &format!("{hash}/{size}"));
                parsed.sized.push((n, digest.expect("a digest")));
            }
            "case" => rest.clone_into(&mut name),
            "request" => rest.clone_into(&mut request),
            "response" => parsed.cases.push(Case {
                name: name.clone(),
                request: request.clone(),
                expect: Expect::Response(unhex(rest)),
            }),
            "refused" => {
                let (status, reason) = rest.split_once(' ').expect("a status and a reason");
                parsed.cases.push(Case {
                    name: name.clone(),
                    request: request.clone(),
                    expect: Expect::Refused(status.parse().expect("a status"), reason.to_owned()),
                });
            }
            _ => {}
        }
    }
    parsed
}

/// A source over a map of blobs that logs its reads, fails for some digests, can be broken,
/// can hold the reads of one digest until given a permit, and can give other bytes for one.
struct Source {
    blobs: HashMap<Digest, Bytes>,
    fails: HashSet<Digest>,
    reads: Mutex<Vec<Digest>>,
    broken: AtomicBool,
    gate: Option<(Digest, Semaphore)>,
    wrong: Mutex<Option<Digest>>,
}

impl Source {
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
}

impl BlobSource for Source {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            self.reads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(*digest);
            if self.broken.load(Ordering::SeqCst) || self.fails.contains(digest) {
                return Err(StorageError::Unreachable(*digest));
            }
            if let Some((held, gate)) = &self.gate
                && held == digest
            {
                gate.acquire().await.expect("the gate is open").forget();
            }
            if *self.wrong.lock().unwrap_or_else(PoisonError::into_inner) == Some(*digest) {
                return Ok(Bytes::from_static(b"not the blob"));
            }
            self.blobs
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
}

struct World {
    action: Digest,
    /// Another Action, whose tree holds `alpha` only.
    other: Digest,
    names: HashMap<String, Digest>,
    held: HashMap<Digest, Bytes>,
    fails: HashSet<Digest>,
    cases: Vec<Case>,
}

fn world() -> World {
    let parsed = fixture();
    let mut names = HashMap::new();
    let mut held = HashMap::new();
    let mut fails = HashSet::new();
    let mut in_tree = Vec::new();
    for blob in &parsed.blobs {
        let digest = kbf_segments::sha256(&blob.bytes);
        names.insert(blob.name.clone(), digest);
        let stored = blob.class != "absent";
        if stored {
            held.insert(digest, Bytes::from(blob.bytes.clone()));
        }
        if blob.class == "fails" {
            fails.insert(digest);
        }
        if blob.class != "outside" {
            in_tree.push((blob.name.clone(), digest));
        }
    }
    for (name, digest) in &parsed.sized {
        names.insert(name.clone(), *digest);
        in_tree.push((name.clone(), *digest));
    }
    let file_nodes = |only: Option<&str>| -> Vec<FileNode> {
        in_tree
            .iter()
            .filter(|(name, _)| only.is_none_or(|only| only == name))
            .map(|(name, digest)| FileNode {
                name: name.clone(),
                digest: Some(digest_to_proto(digest)),
                ..FileNode::default()
            })
            .collect()
    };
    let root = Directory {
        files: file_nodes(None),
        ..Directory::default()
    };
    let alpha_only = Directory {
        files: file_nodes(Some("alpha")),
        ..Directory::default()
    };
    let mut put = |bytes: Vec<u8>| {
        let digest = kbf_segments::sha256(&bytes);
        held.insert(digest, Bytes::from(bytes));
        digest
    };
    let root = put(root.encode_to_vec());
    let command = put(b"the command".to_vec());
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(action.encode_to_vec());
    let alpha_only = put(alpha_only.encode_to_vec());
    let other = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&alpha_only)),
        ..Action::default()
    };
    let other = put(other.encode_to_vec());
    World {
        action,
        other,
        names,
        held,
        fails,
        cases: parsed.cases,
    }
}

impl World {
    fn d(&self, name: &str) -> Digest {
        self.names[name]
    }

    /// The text of `name` as a digest in a request.
    fn t(&self, name: &str) -> String {
        self.d(name).to_string()
    }
}

struct Rig {
    source: Arc<Source>,
    read: JobRead,
    grants: Arc<Grants>,
    clock: Arc<AtomicU64>,
}

fn rig(w: &World, gate: Option<Digest>, limits: Limits) -> Rig {
    let clock = Arc::new(AtomicU64::new(100));
    let key = GrantKey::from_secret(&[1; MIN_SECRET_LEN]).expect("a long enough secret");
    let tick = Arc::clone(&clock);
    let grants = Arc::new(Grants::new(
        key,
        Arc::new(move || tick.load(Ordering::SeqCst)),
    ));
    let source = Arc::new(Source {
        blobs: w.held.clone(),
        fails: w.fails.clone(),
        reads: Mutex::new(Vec::new()),
        broken: AtomicBool::new(false),
        gate: gate.map(|digest| (digest, Semaphore::new(0))),
        wrong: Mutex::new(None),
    });
    let read = JobRead::new(source.clone(), Arc::clone(&grants), limits);
    Rig {
        source,
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

    /// `POST /batch` with `body`, under the grant `token`.
    async fn batch(&self, body: &str, token: Option<&str>) -> Reply {
        let header = token.map(|token| format!("Bearer {token}"));
        self.read
            .respond_with_body(
                &Method::POST,
                BATCH_PATH,
                header.as_deref(),
                body.as_bytes(),
            )
            .await
    }

    fn metrics(&self) -> String {
        let mut out = String::new();
        self.read.render_metrics(&mut out);
        out
    }
}

fn body_of(digests: &[String]) -> String {
    let quoted: Vec<String> = digests.iter().map(|d| format!("\"{d}\"")).collect();
    format!("{{\"digests\":[{}]}}", quoted.join(","))
}

fn frame(status: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![status];
    let length = if status == 0 { payload.len() as u64 } else { 0 };
    out.extend_from_slice(&length.to_be_bytes());
    if status == 0 {
        out.extend_from_slice(payload);
    }
    out
}

fn text(reply: &Reply) -> &str {
    std::str::from_utf8(&reply.body).expect("a reason code is text")
}

fn has_line(out: &str, line: &str) {
    assert!(out.lines().any(|l| l == line), "{line} is not in\n{out}");
}

/// Catches: a framing that is not the one agreed with the client author: every case of the
/// golden fixture (frames in request order, duplicates answered each time, each of the five
/// statuses, the sum bound exactly, the refusals of a whole request) is answered with exactly its
/// bytes or its status and reason. The fixture's digests are the digests of its blobs.
#[tokio::test]
async fn the_golden_fixture_is_what_the_listener_answers() {
    let w = world();
    assert!(w.cases.len() >= 6, "the fixture has its cases");
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    let known: HashSet<String> = w.names.values().map(ToString::to_string).collect();
    for case in &w.cases {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&case.request) {
            for digest in value["digests"].as_array().map_or(&[][..], Vec::as_slice) {
                let digest = digest.as_str().expect("a text digest");
                assert!(known.contains(digest), "{}: {digest}", case.name);
            }
        }
        let reply = rig.batch(&case.request, Some(&token)).await;
        match &case.expect {
            Expect::Response(bytes) => {
                assert_eq!(reply.status, StatusCode::OK, "{}", case.name);
                assert_eq!(&reply.body[..], &bytes[..], "{}", case.name);
            }
            Expect::Refused(status, reason) => {
                assert_eq!(reply.status.as_u16(), *status, "{}", case.name);
                assert_eq!(text(&reply), format!("{reason}\n"), "{}", case.name);
            }
        }
    }
}

/// Catches: a request that is not a token's being read for, an unauthenticated caller learning
/// anything else than why it is refused, and any refusal that has read first. Every refusal here
/// is whole, and reads nothing from the source.
#[tokio::test]
async fn a_request_with_a_bad_token_is_refused_whole_and_reads_nothing() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let body = body_of(&[w.t("alpha")]);
    let reply = rig.batch(&body, None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(text(&reply), "missing_token\n");
    let reply = rig.batch(&body, Some("not-a-token")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(text(&reply), "invalid_token\n");
    let token = rig.token(w.action);
    rig.clock.store(NOT_AFTER, Ordering::SeqCst);
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(text(&reply), "expired\n");
    rig.clock.store(100, Ordering::SeqCst);
    rig.grants.revoke(BUILD, NOT_AFTER);
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(text(&reply), "revoked\n");
    assert_eq!(rig.source.total_reads(), 0);
}

/// Catches: a malformed request being answered as anything but 400, one that is read for
/// before it is refused, a digest form other than the one a GET path has (upper case, no size,
/// a signed or non-numeric size) being accepted, and a count bound that is off by one (256 is
/// answered, 257 and 0 are not).
#[tokio::test]
async fn a_malformed_request_is_400_and_reads_nothing() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    let alpha = w.t("alpha");
    let (hash, size) = alpha.split_once('/').expect("a digest");
    let many = |n: usize| body_of(&vec![alpha.clone(); n]);
    let bad = [
        "".to_owned(),
        "not json".to_owned(),
        "[]".to_owned(),
        "{}".to_owned(),
        "{\"digests\":\"x\"}".to_owned(),
        "{\"digests\":[5]}".to_owned(),
        "{\"digests\":[null]}".to_owned(),
        "{\"digests\":[]}".to_owned(),
        body_of(&[hash.to_owned()]),
        body_of(&[format!("{}/{size}", hash.to_uppercase())]),
        body_of(&[format!("{hash}/+{size}")]),
        body_of(&[format!("{hash}/x")]),
        body_of(&[format!("{hash}/")]),
        body_of(&[format!("{}/{size}", &hash[1..])]),
        many(BATCH_MAX_DIGESTS + 1),
    ];
    for body in &bad {
        let reply = rig.batch(body, Some(&token)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body:.80}");
        assert_eq!(text(&reply), "malformed\n", "{body:.80}");
    }
    assert_eq!(rig.source.total_reads(), 0, "nothing is read to refuse");
    let reply = rig.batch(&many(BATCH_MAX_DIGESTS), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.len(), BATCH_MAX_DIGESTS * (9 + 5));
}

/// Catches: a sum bound that is off by one or that is checked after the reads: sizes of exactly
/// 4194304 together are answered, one byte more is 413 `too_large` with nothing read, and a
/// request that is too large is refused whole whatever the grant's scope would say of its digests.
#[tokio::test]
async fn sizes_over_the_sum_bound_are_413_before_any_read() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    assert_eq!(BATCH_MAX_BYTES, 4_194_304);
    let stranger = |hash: u8, size: u64| Digest::new(DigestFunction::Sha256, [hash; 32], size);
    let half = BATCH_MAX_BYTES / 2;
    let at_bound = [stranger(1, half), stranger(2, half)].map(|d| d.to_string());
    let reply = rig.batch(&body_of(&at_bound), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK, "the bound itself is served");
    let over = [stranger(1, half), stranger(2, half + 1)].map(|d| d.to_string());
    let before = rig.source.total_reads();
    let reply = rig.batch(&body_of(&over), Some(&token)).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(text(&reply), "too_large\n");
    assert_eq!(rig.source.total_reads(), before);
    let sizes_of_u64_max = [stranger(3, u64::MAX), stranger(4, u64::MAX)].map(|d| d.to_string());
    let reply = rig.batch(&body_of(&sizes_of_u64_max), Some(&token)).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "a size past u64 sums is not counted"
    );
    assert_eq!(&reply.body[..], [frame(2, b""), frame(2, b"")].concat());
}

/// Catches: scope being checked for the request and not for each digest (one digest outside the
/// grant spoiling the others or, worse, being served), and a blob outside the scope being read
/// from the store before it is refused.
#[tokio::test]
async fn scope_is_checked_for_each_digest_and_a_refused_digest_is_not_read() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    let body = body_of(&[w.t("secret"), w.t("alpha"), w.t("secret")]);
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let expected = [frame(2, b""), frame(0, b"alpha"), frame(2, b"")].concat();
    assert_eq!(&reply.body[..], &expected[..]);
    assert_eq!(rig.source.reads_of(&w.d("secret")), 0);
}

/// Catches: a blob over 4 MiB being read for a batch (it is answered `too_large` from its
/// digest alone), and a blob that is in scope but not held being answered as anything but
/// `not_found`, or spoiling the frames beside it.
#[tokio::test]
async fn an_oversize_blob_is_not_read_and_a_missing_one_is_not_found() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    let body = body_of(&[w.t("huge"), w.t("gone"), w.t("beta")]);
    let reply = rig.batch(&body, Some(&token)).await;
    let expected = [
        frame(3, b""),
        frame(1, b""),
        frame(0, &w.held[&w.d("beta")]),
    ]
    .concat();
    assert_eq!(&reply.body[..], &expected[..]);
    assert_eq!(rig.source.reads_of(&w.d("huge")), 0);
    assert_eq!(rig.source.reads_of(&w.d("gone")), 1);
}

/// Catches: a store that fails for one blob failing the request (the other blobs are answered),
/// bytes that are not the blob's being sent as an ok frame (the length of an ok frame is the
/// digest's size, and its bytes hash to it), and the empty blob being read from a store that
/// never holds it.
#[tokio::test]
async fn a_failing_or_lying_store_is_store_unavailable_for_that_blob_only() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    *rig.source
        .wrong
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(w.d("alpha"));
    let token = rig.token(w.action);
    let body = body_of(&[w.t("flaky"), w.t("alpha"), w.t("beta"), w.t("empty")]);
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let beta = &w.held[&w.d("beta")];
    let expected = [frame(4, b""), frame(4, b""), frame(0, beta), frame(0, b"")].concat();
    assert_eq!(&reply.body[..], &expected[..]);
    assert_eq!(rig.source.reads_of(&w.d("empty")), 0);
}

/// Catches: a store that cannot answer being told to the task as a refusal of its grant (403),
/// and an Action whose inputs are not in the store being served: both are refused whole, the first
/// as 503 `store_unavailable`, the second as 403 `no_scope`.
#[tokio::test]
async fn a_scope_that_cannot_be_worked_out_refuses_the_whole_request() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    rig.source.broken.store(true, Ordering::SeqCst);
    let body = body_of(&[w.t("alpha")]);
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(text(&reply), "store_unavailable\n");
    rig.source.broken.store(false, Ordering::SeqCst);
    let absent = Digest::new(DigestFunction::Sha256, [5; 32], 10);
    let reply = rig.batch(&body, Some(&rig.token(absent))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(text(&reply), "no_scope\n");
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "a failed scope is not remembered"
    );
}

/// Catches: `/batch` being answered for the wrong method, a GET of it being taken for a blob
/// path (it is 405, not 404), another path taking a POST, and a POST with no body being
/// anything but a malformed request.
#[tokio::test]
async fn only_post_reaches_batch_and_only_batch_takes_a_post() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    let header = format!("Bearer {token}");
    for method in [Method::GET, Method::PUT, Method::DELETE, Method::HEAD] {
        let reply = rig
            .read
            .respond_with_body(&method, BATCH_PATH, Some(&header), b"{}")
            .await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(text(&reply), "method_not_allowed\n", "{method}");
    }
    let alpha = format!("/{}", w.d("alpha"));
    for path in [alpha.as_str(), "/", "/batch/", "/Batch"] {
        let reply = rig
            .read
            .respond_with_body(&Method::POST, path, Some(&header), b"{}")
            .await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
    let reply = rig
        .read
        .respond(&Method::POST, BATCH_PATH, Some(&header))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(rig.source.total_reads(), 0);
}

/// The largest request body (256 digests of at most 100 bytes each) fits the bound on a body.
const _: () = assert!(MAX_BATCH_REQUEST_BYTES >= BATCH_MAX_DIGESTS * 100);

/// Catches: batches counted under the wrong outcome, blobs counted under the wrong status,
/// bytes served not counting the batch's, and GET's counters moving for a batch.
#[tokio::test]
async fn the_metrics_count_each_batch_outcome_and_each_blob_status() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.action);
    let all = body_of(&[
        w.t("alpha"),
        w.t("gone"),
        w.t("secret"),
        w.t("huge"),
        w.t("flaky"),
    ]);
    let _ = rig.batch(&all, Some(&token)).await;
    let _ = rig.batch(&all, None).await;
    let _ = rig.batch("nope", Some(&token)).await;
    let big = [w.t("big_a"), w.t("big_b")];
    let _ = rig.batch(&body_of(&big), Some(&token)).await;
    let _ = rig
        .batch(
            &all,
            Some(&rig.token(Digest::new(DigestFunction::Sha256, [5; 32], 1))),
        )
        .await;
    rig.source.broken.store(true, Ordering::SeqCst);
    let _ = rig
        .batch(
            &all,
            Some(&rig.token(Digest::new(DigestFunction::Sha256, [6; 32], 1))),
        )
        .await;
    let out = rig.metrics();
    for line in [
        "kbf_job_batch_requests_total{outcome=\"ok\"} 1",
        "kbf_job_batch_requests_total{outcome=\"unauthorized\"} 1",
        "kbf_job_batch_requests_total{outcome=\"malformed\"} 1",
        "kbf_job_batch_requests_total{outcome=\"too_large\"} 1",
        "kbf_job_batch_requests_total{outcome=\"forbidden\"} 1",
        "kbf_job_batch_requests_total{outcome=\"unavailable\"} 1",
        "kbf_job_batch_blobs_total{status=\"ok\"} 1",
        "kbf_job_batch_blobs_total{status=\"not_found\"} 1",
        "kbf_job_batch_blobs_total{status=\"out_of_scope\"} 1",
        "kbf_job_batch_blobs_total{status=\"too_large\"} 1",
        "kbf_job_batch_blobs_total{status=\"store_unavailable\"} 1",
        "kbf_job_read_bytes_total 5",
        "kbf_job_read_requests_total{outcome=\"ok\"} 0",
    ] {
        has_line(&out, line);
    }
}

/// Catches: a grant for one Action reading what only another Action's scope holds: with a token
/// for an Action whose tree has `alpha` alone, `alpha` is served and `beta` and `secret`, which
/// are in the first Action's tree or in none, are out of scope and are not read.
#[tokio::test]
async fn a_token_for_another_action_is_scoped_to_that_actions_tree() {
    let w = world();
    let rig = rig(&w, None, Limits::default());
    let token = rig.token(w.other);
    let body = body_of(&[w.t("alpha"), w.t("beta"), w.t("secret")]);
    let reply = rig.batch(&body, Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let expected = [frame(0, b"alpha"), frame(2, b""), frame(2, b"")].concat();
    assert_eq!(&reply.body[..], &expected[..]);
    assert_eq!(rig.source.reads_of(&w.d("beta")), 0);
}
