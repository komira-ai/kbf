//! `GET /locate` on the job read listener: the table of where the blobs of a grant's Action
//! are, over a source that has a pack index and large objects of its own to answer from: its
//! scope, its format and its spans. The caps, deadlines, budget and presence memory are in
//! `job_read_locate_bounds.rs`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BlobSource, JobRead, LOCATE_PATH, MAX_LOCATE_LINE_BYTES, large_key};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use tonic::codegen::http::{Method, StatusCode};

mod locate_common;

use locate_common::*;

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
    pack(&source, &a.deep, span("cas/f/w/2", 77, a.deep.size_bytes));
    pack(
        &source,
        &a.command,
        span("cas/f/w/2", 0, a.command.size_bytes),
    );
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
        serde_json::json!({"key": "cas/f/w/2", "offset": 77, "length": a.deep.size_bytes})
    );
    assert_eq!(pack_of(&a.files[0])["offset"], 0);
    assert_eq!(pack_of(&a.command)["length"], a.command.size_bytes);
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

/// Catches: a span the pack reader would not read as the blob (it reads the digest's size from
/// the offset, so a length that is not the size is the index disagreeing with the digest), and a
/// pack key longer than an object key can be, passed on; and a line longer than the most the
/// budget is charged for: those two are `absent`, and a key of the longest length is a pack
/// entry whose line fits [`MAX_LOCATE_LINE_BYTES`].
#[tokio::test]
async fn a_span_that_does_not_fit_its_digest_is_absent() {
    let source = Source::default();
    let a = build(
        &source,
        "a",
        &[b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()],
    );
    let longest = "k".repeat(1024);
    pack(&source, &a.files[0], span(&longest, u64::MAX - 5, 5));
    pack(&source, &a.files[1], span("cas/f/w/1", 0, 99));
    pack(&source, &a.files[2], span(&"k".repeat(1025), 0, 5));
    let rig = rig(source, small());
    let reply = rig.locate(&rig.token(a.action)).await;
    let text = std::str::from_utf8(&reply.body).expect("utf-8").to_owned();
    let table = table(&reply);
    assert!(table[&a.files[0].to_string()]["pack"].is_object());
    assert_eq!(table[&a.files[1].to_string()]["absent"], true);
    assert_eq!(table[&a.files[2].to_string()]["absent"], true);
    let longest_line = text.lines().map(|l| l.len() + 1).max().expect("lines");
    assert!(
        longest_line as u64 <= MAX_LOCATE_LINE_BYTES,
        "{longest_line}"
    );
}

/// Catches: a span that compares equal to one that differs in its key, offset or length (the
/// grants and the table's tests compare spans), or whose copy or `Debug` leaves a field out.
#[test]
fn a_span_is_equal_to_its_copy_and_to_nothing_that_differs_in_a_field() {
    let at = span("cas/f/w/1", 3, 5);
    assert_eq!(at, at.clone());
    assert_ne!(at, span("cas/f/w/2", 3, 5));
    assert_ne!(at, span("cas/f/w/1", 4, 5));
    assert_ne!(at, span("cas/f/w/1", 3, 6));
    let shown = format!("{at:?}");
    for part in ["cas/f/w/1", "offset: 3", "length: 5"] {
        assert!(shown.contains(part), "{shown}");
    }
}
