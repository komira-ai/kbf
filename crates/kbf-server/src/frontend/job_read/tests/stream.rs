//! Large blobs over the job listener's real TLS: a blob over the whole-blob bound is streamed
//! with its content length, one whose bytes are not its digest's is cut short, one over the cap
//! is refused before a byte is read, and the listener's size and read flags reach its limits.
//!
//! The bounds are scaled down (the bound at 1 KiB, the cap at 16 KiB, a source that streams in
//! chunks of 1 KiB) so that a blob is kilobytes; the code that runs is the one a large blob runs.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_front::grants::{GrantKey, Grants};
use kbf_front::job_read::{BlobSource, Limits};
use kbf_proto::reapi::{Action, Directory, FileNode};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;

use super::{
    BUILD, Blobs, Files, JobReadFlags, SECRET, Serving, Timeouts, ca, files, flags, ip, path_of,
    prepare, proto, request,
};
use crate::frontend::{command, parse};
use crate::test_dir::scratch_dir;

const CAP: u64 = 16 * 1024;

pub(super) struct Large {
    pub(super) action: Digest,
    pub(super) big: Digest,
    pub(super) over_cap: Digest,
    pub(super) bytes: Bytes,
    pub(super) blobs: Vec<(Digest, Bytes)>,
}

/// An Action whose input root has a file of 10 KiB, and one a byte over the cap that is held by
/// no one.
pub(super) fn large() -> Large {
    let mut blobs = Vec::new();
    let mut keep = |bytes: Vec<u8>| {
        let bytes = Bytes::from(bytes);
        let digest = kbf_segments::sha256(&bytes);
        blobs.push((digest, bytes));
        digest
    };
    let bytes: Vec<u8> = (0..10 * 1024).map(|i| (i * 31 % 251) as u8).collect();
    let big = keep(bytes);
    let over_cap = Digest::new(DigestFunction::Sha256, [9; 32], CAP + 1);
    let file = |name: &str, digest: &Digest| FileNode {
        name: name.to_owned(),
        digest: Some(proto(digest)),
        ..FileNode::default()
    };
    let root = Directory {
        files: vec![file("big", &big), file("over", &over_cap)],
        ..Directory::default()
    };
    let root = keep(root.encode_to_vec());
    let command = keep(b"the command".to_vec());
    let action = Action {
        command_digest: Some(proto(&command)),
        input_root_digest: Some(proto(&root)),
        ..Action::default()
    };
    let action = keep(action.encode_to_vec());
    let held = blobs[0].1.clone();
    Large {
        action,
        big,
        over_cap,
        bytes: held,
        blobs,
    }
}

impl Large {
    pub(super) fn token(&self) -> String {
        let key = GrantKey::from_secret(SECRET).expect("the secret is long enough");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs();
        Grants::system(key)
            .issue(BUILD, self.action, now + 3_600)
            .expect("a valid build id")
            .expose()
            .to_owned()
    }

    /// The store of every blob; with `corrupt`, the large one is stored with one byte changed,
    /// under the digest of the real bytes.
    pub(super) fn source(&self, corrupt: bool) -> Arc<dyn BlobSource> {
        let mut map: std::collections::HashMap<Digest, Bytes> =
            self.blobs.iter().cloned().collect();
        if corrupt {
            let mut wrong = self.bytes.to_vec();
            wrong[6 * 1024] ^= 0xff;
            map.insert(self.big, Bytes::from(wrong));
        }
        Arc::new(Blobs(map))
    }
}

/// A listener over `source` whose bounds are the scaled-down ones.
fn listening(
    files: &Files,
    source: Arc<dyn BlobSource>,
) -> (std::net::SocketAddr, crate::listen::Latch) {
    let mut flags = flags(files, "127.0.0.1:0");
    flags.max_blob_bytes = CAP;
    let mut prepared = prepare(&flags).expect("prepared");
    assert_eq!(prepared.limits.max_blob_bytes, CAP, "the flag is the cap");
    prepared.limits.stream_over = 1024;
    let addr = prepared.local();
    let Serving { latch, task, .. } = prepared
        .start(source, Timeouts::default())
        .expect("started");
    tokio::spawn(task);
    (addr, latch)
}

/// Catches: a streamed blob sent without its length (a client could not tell a cut-short
/// response from a whole one), with wrong bytes, or in a form a client that reads to the end of
/// the connection cannot read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_over_the_bound_is_streamed_with_its_content_length() {
    let dir = scratch_dir("job-read-stream-ok");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let large = large();
    let (addr, _latch) = listening(&files, large.source(false));
    let got = request(
        addr,
        &farm.pem,
        "GET",
        &path_of(&large.big),
        Some(&large.token()),
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.body, large.bytes);
    let length = large.bytes.len().to_string();
    assert_eq!(got.header("content-length"), Some(length.as_str()));
    assert_eq!(got.header("content-type"), Some("application/octet-stream"));
}

/// Catches: a blob whose stored bytes are not its digest's being sent to the end over the wire.
/// The response is cut short: the client either gets no response at all (the server aborts a
/// response it has not yet flushed) or one that ends before its content length, and never the
/// whole of the wrong blob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupt_large_blob_is_cut_short_over_tls() {
    let dir = scratch_dir("job-read-stream-corrupt");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let large = large();
    let (addr, _latch) = listening(&files, large.source(true));
    let got = request(
        addr,
        &farm.pem,
        "GET",
        &path_of(&large.big),
        Some(&large.token()),
    )
    .await;
    match got {
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof),
        Ok(got) => {
            let length: usize = got
                .header("content-length")
                .and_then(|v| v.parse().ok())
                .expect("a content length");
            assert_eq!(length, large.bytes.len());
            assert!(
                got.body.len() < length,
                "{} of {length} bytes",
                got.body.len()
            );
        }
    }
}

/// Catches: a blob over the cap being opened (it is 413 with its reason code before a byte is
/// read), and the cap flag not being the one in force.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_over_the_cap_is_413_over_tls() {
    let dir = scratch_dir("job-read-stream-cap");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let large = large();
    let (addr, _latch) = listening(&files, large.source(false));
    let got = request(
        addr,
        &farm.pem,
        "GET",
        &path_of(&large.over_cap),
        Some(&large.token()),
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 413);
    assert_eq!(got.text(), "too_large\n");
}

/// Catches: the cap and the reads in flight not being flags, a default that is not the design's
/// (512 MiB, 32), a default in the flag text that drifted from the one the code uses, a zero
/// accepted (a cap of 0 refuses every blob, 0 reads wait for ever), and flags that do not reach
/// the limits the listener runs under.
#[test]
fn the_cap_and_the_reads_in_flight_are_flags_with_the_design_defaults() {
    let base = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--job-listen=127.0.0.1:8990",
        "--job-tls-cert=/etc/kbf/job.pem",
        "--job-tls-key=/etc/kbf/job.key",
        "--job-grant-secret-file=/etc/kbf/grant.secret",
    ];
    let with = |flags: &[&str]| {
        let argv = base.iter().chain(flags.iter()).copied();
        command().try_get_matches_from(argv).map(|m| parse(&m))
    };
    let job = |flags: &[&str]| -> JobReadFlags {
        with(flags)
            .expect("matches")
            .expect("flags")
            .job_read
            .expect("job flags")
    };
    let default = job(&[]);
    assert_eq!(default.max_blob_bytes, 536_870_912);
    assert_eq!(default.reads_in_flight, 32);
    assert_eq!(default.read_budget_bytes, 1_073_741_824);
    assert_eq!(default.read_idle_secs, 60);
    assert_eq!(
        default.read_idle_secs,
        Limits::default().held_idle.as_secs()
    );
    assert_eq!(
        default.read_budget_bytes,
        Limits::default().read_budget_bytes
    );
    assert_eq!(default.max_blob_bytes, Limits::default().max_blob_bytes);
    assert_eq!(
        default.reads_in_flight as usize,
        Limits::default().reads_in_flight
    );
    let set = job(&[
        "--job-max-blob-bytes=1048576",
        "--job-reads-in-flight=4",
        "--job-read-budget-bytes=4096",
        "--job-read-idle-secs=5",
    ]);
    assert_eq!(set.read_idle_secs, 5);
    assert_eq!(set.read_budget_bytes, 4096);
    assert_eq!(set.max_blob_bytes, 1_048_576);
    assert_eq!(set.reads_in_flight, 4);
    for zero in [
        "--job-max-blob-bytes=0",
        "--job-reads-in-flight=0",
        "--job-read-budget-bytes=0",
        "--job-read-idle-secs=0",
    ] {
        assert!(with(&[zero]).is_err(), "{zero}");
    }
    // Alone, without --job-listen, they have no listener to bound.
    let alone = base[..5].iter().copied().chain(["--job-reads-in-flight=4"]);
    assert!(command().try_get_matches_from(alone).is_err());
}

/// Catches: flags that parse and are then not the limits the listener is built with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flags_are_the_limits_of_the_listener() {
    let dir = scratch_dir("job-read-stream-flags");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let mut flags = flags(&files, "127.0.0.1:0");
    flags.max_blob_bytes = 1_048_576;
    flags.reads_in_flight = 4;
    flags.read_budget_bytes = 4096;
    flags.read_idle_secs = 5;
    let prepared = prepare(&flags).expect("prepared");
    assert_eq!(prepared.limits.max_blob_bytes, 1_048_576);
    assert_eq!(prepared.limits.reads_in_flight, 4);
    assert_eq!(prepared.limits.read_budget_bytes, 4096);
    assert_eq!(prepared.limits.held_idle, std::time::Duration::from_secs(5));
    assert_eq!(prepared.limits.stream_over, Limits::default().stream_over);
}
