//! The latency of the job-facing reads (`kbf_job_read_seconds`): one observation per answered
//! `GET /<hex>/<size>` and per answered `POST /batch`, placed in its bucket by the time the
//! injected stopwatch says the read took. No test reads the wall: the source moves the
//! stopwatch by a set amount while it reads a blob.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::digest_to_proto;
use kbf_front::grants::{GrantKey, Grants, MIN_SECRET_LEN};
use kbf_front::job_read::{BATCH_PATH, BlobSource, JobRead, LOCATE_PATH, Limits, Reply};
use kbf_front::store_metrics::Stopwatch;
use kbf_proto::reapi::{Action, Directory, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tonic::codegen::http::{Method, StatusCode};

const BUILD: &str = "build-0021";
const ZERO: &str = include_str!("golden/job_read_seconds_zero.txt");

/// The injected stopwatch, in nanoseconds.
#[derive(Clone, Default)]
struct Ticks(Arc<AtomicU64>);

impl Ticks {
    fn stopwatch(&self) -> Stopwatch {
        let ticks = Arc::clone(&self.0);
        Arc::new(move || Duration::from_nanos(ticks.load(Ordering::SeqCst)))
    }

    fn pass(&self, by: Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
    }
}

/// A source of blobs that takes `cost` of the stopwatch to read the blob it is set for, and
/// no time for any other.
struct Timed {
    blobs: HashMap<Digest, Bytes>,
    ticks: Ticks,
    cost: std::sync::Mutex<(Option<Digest>, Duration)>,
}

impl BlobSource for Timed {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move {
            let (slow, cost) = *self.cost.lock().expect("lock");
            if slow == Some(*digest) {
                self.ticks.pass(cost);
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

struct Rig {
    read: JobRead,
    source: Arc<Timed>,
    file: Digest,
    token: String,
}

fn rig() -> Rig {
    let mut blobs = HashMap::new();
    let mut put = |bytes: &[u8]| {
        let digest = kbf_segments::sha256(bytes);
        blobs.insert(digest, Bytes::copy_from_slice(bytes));
        digest
    };
    let file = put(b"the input");
    let root = Directory {
        files: vec![FileNode {
            name: "in".to_owned(),
            digest: Some(digest_to_proto(&file)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let root = put(&root.encode_to_vec());
    let command = put(b"the command");
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(&action.encode_to_vec());
    let ticks = Ticks::default();
    let key = GrantKey::from_secret(&[3; MIN_SECRET_LEN]).expect("a long enough secret");
    let grants = Arc::new(Grants::new(key, Arc::new(|| 100)));
    let token = grants
        .issue(BUILD, action, 1_000)
        .expect("a valid build id")
        .expose()
        .to_owned();
    let source = Arc::new(Timed {
        blobs,
        ticks: ticks.clone(),
        cost: std::sync::Mutex::new((None, Duration::ZERO)),
    });
    let read =
        JobRead::new(source.clone(), grants, Limits::default()).with_stopwatch(ticks.stopwatch());
    Rig {
        read,
        source,
        file,
        token,
    }
}

impl Rig {
    /// The next read of the input file takes `cost`.
    fn costs(&self, cost: Duration) {
        *self.source.cost.lock().expect("lock") = (Some(self.file), cost);
    }

    async fn get(&self, token: Option<&str>) -> Reply {
        let header = token.map(|token| format!("Bearer {token}"));
        let path = format!("/{}", self.file);
        self.read
            .respond(&Method::GET, &path, header.as_deref())
            .await
    }

    fn scrape(&self) -> String {
        let mut out = String::new();
        self.read.render_metrics(&mut out);
        out
    }
}

fn has(out: &str, line: &str) {
    assert!(out.lines().any(|l| l == line), "{line} is not in\n{out}");
}

/// Catches: a family that is absent before the first read (a dashboard has nothing to
/// scrape), and one whose lines drift from the pinned text.
#[test]
fn with_no_reads_the_latency_family_renders_zero_lines() {
    let out = rig().scrape();
    assert!(out.contains(ZERO), "{ZERO} is not in\n{out}");
}

/// Catches: a read that is observed twice or not at all, an observation in the wrong bucket,
/// an answered refusal that is not observed, and a path or method that is no blob read being
/// observed: three answered GETs (3 ms, 30 ms, and an unauthorized one at no time) give
/// count 3 and sum 0.033, in the buckets of 5 ms, 50 ms and 1 ms.
#[tokio::test]
async fn job_read_latency_has_one_observation_per_answered_get() {
    let rig = rig();
    rig.costs(Duration::from_millis(3));
    assert_eq!(rig.get(Some(&rig.token)).await.status, StatusCode::OK);
    rig.costs(Duration::from_millis(30));
    assert_eq!(rig.get(Some(&rig.token)).await.status, StatusCode::OK);
    assert_eq!(rig.get(None).await.status, StatusCode::UNAUTHORIZED);
    let out = rig.scrape();
    for (le, count) in [
        ("0.001", 1),
        ("0.0025", 1),
        ("0.005", 2),
        ("0.01", 2),
        ("0.025", 2),
        ("0.05", 3),
        ("0.1", 3),
        ("60", 3),
        ("+Inf", 3),
    ] {
        has(
            &out,
            &format!("kbf_job_read_seconds_bucket{{op=\"get\",le=\"{le}\"}} {count}"),
        );
    }
    has(&out, "kbf_job_read_seconds_sum{op=\"get\"} 0.033");
    has(&out, "kbf_job_read_seconds_count{op=\"get\"} 3");
    has(&out, "kbf_job_read_seconds_count{op=\"batch\"} 0");
}

/// Catches: requests that are not blob reads (`/locate`, a method other than GET on a blob's
/// path, a path that is no digest) adding observations.
#[tokio::test]
async fn requests_that_are_not_blob_reads_are_not_observed() {
    let rig = rig();
    rig.costs(Duration::from_millis(3));
    let header = format!("Bearer {}", rig.token);
    let blob = format!("/{}", rig.file);
    let _ = rig
        .read
        .respond(&Method::GET, LOCATE_PATH, Some(&header))
        .await;
    let _ = rig.read.respond(&Method::POST, &blob, Some(&header)).await;
    let _ = rig
        .read
        .respond(&Method::GET, "/not-a-digest", Some(&header))
        .await;
    let _ = rig
        .read
        .respond_with_body(&Method::GET, BATCH_PATH, Some(&header), b"{}")
        .await;
    let out = rig.scrape();
    has(&out, "kbf_job_read_seconds_count{op=\"get\"} 0");
    has(&out, "kbf_job_read_seconds_count{op=\"batch\"} 0");
}

/// Catches: a batch that is not observed, one observed under `get`, and a batch whose
/// bucket is not the time its one blob took (7 ms is the 10 ms bucket).
#[tokio::test]
async fn a_batch_is_one_observation_under_its_own_op() {
    let rig = rig();
    rig.costs(Duration::from_millis(7));
    let header = format!("Bearer {}", rig.token);
    let body = format!("{{\"digests\":[\"{}\"]}}", rig.file);
    let reply = rig
        .read
        .respond_with_body(&Method::POST, BATCH_PATH, Some(&header), body.as_bytes())
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let out = rig.scrape();
    has(
        &out,
        "kbf_job_read_seconds_bucket{op=\"batch\",le=\"0.005\"} 0",
    );
    has(
        &out,
        "kbf_job_read_seconds_bucket{op=\"batch\",le=\"0.01\"} 1",
    );
    has(&out, "kbf_job_read_seconds_count{op=\"batch\"} 1");
    has(&out, "kbf_job_read_seconds_sum{op=\"batch\"} 0.007");
    has(&out, "kbf_job_read_seconds_count{op=\"get\"} 0");
}
