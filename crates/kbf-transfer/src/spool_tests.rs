use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::stream;
use kbf_proto::reapi::Digest;

use super::{Spool, State, to_proto};
use crate::cas::Cas;

/// A file of this test binary's own, created empty.
fn scratch_file(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-transfer-spool-unit")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(name);
    File::create(&path).expect("create");
    path
}

/// A spool over `file`, as `Spool::create` makes one over a file it opened itself.
fn spool_over(file: File) -> Spool {
    Spool {
        file: Arc::new(file),
        state: Mutex::new(State::default()),
    }
}

/// A spool whose file refuses writes (opened read-only).
fn unwritable(name: &str) -> Spool {
    let path = scratch_file(name);
    spool_over(File::open(path).expect("open read-only"))
}

/// A spool whose file refuses reads (opened write-only).
fn unreadable(name: &str) -> Spool {
    let path = scratch_file(name);
    spool_over(
        OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open write-only"),
    )
}

/// Catches a failed write that keeps its reservation (mutant: `release` not called on
/// a failed `put`), which would make every later put of that blob a no-op answered
/// as spooled: a `put` whose write fails is a read error naming the spool, records
/// nothing, and leaves the digest free to be reserved again.
#[tokio::test]
async fn a_failed_write_records_nothing_and_frees_the_digest() {
    let spool = unwritable("put");
    let bytes = b"an output".to_vec();
    let digest = kbf_segments::sha256(&bytes);
    let failed = spool.put(bytes).await.expect_err("the write fails");
    let shown = format!("{failed:?}");
    assert!(shown.starts_with("Read("), "{shown}");
    assert!(shown.contains("\"spool: "), "{shown}");
    assert!(spool.records().is_empty());
    assert!(spool.reserve(digest).is_some(), "the digest is free again");
}

/// Catches a failed chunk write that keeps its reservation (mutant: `release` not
/// called when `write_chunks` fails): the chunks of a blob whose write fails are a
/// read error, nothing is recorded, and the digest is free again.
#[tokio::test]
async fn failed_chunk_writes_record_nothing_and_free_the_digest() {
    let spool = unwritable("put-chunks");
    let bytes = b"chunked output".to_vec();
    let digest = kbf_segments::sha256(&bytes);
    let chunks = Box::pin(stream::iter([Ok(bytes)]));
    let failed = spool
        .put_chunks(to_proto(&digest), chunks)
        .await
        .expect_err("the write fails");
    let shown = format!("{failed:?}");
    assert!(shown.starts_with("Read("), "{shown}");
    assert!(shown.contains("\"spool: "), "{shown}");
    assert!(spool.records().is_empty());
    assert!(spool.reserve(digest).is_some(), "the digest is free again");
}

/// Catches a spool read failure answered as a missing blob, which the caller would
/// fetch or upload elsewhere instead of failing (mutant: `get` maps the read error to
/// `Missing`): a blob put into a spool whose file cannot be read is UNAVAILABLE from
/// `get`, and the record's read fails.
#[tokio::test]
async fn a_spool_that_cannot_be_read_is_an_error_not_a_missing_blob() {
    let spool = unreadable("read");
    let digest = spool
        .put(b"written, not readable".to_vec())
        .await
        .expect("put");
    let failed = spool.get(&digest).await.expect_err("the read fails");
    assert!(
        format!("{failed:?}").starts_with("Unavailable("),
        "{failed:?}"
    );
    let record = spool.outgoing().remove(0);
    let size = record.digest.size_bytes;
    record.read(0, size).await.expect_err("the read fails");
}

/// Catches chunks accepted under a digest the spool cannot index (mutant: the parse
/// check dropped, so a blob is stored under no digest): chunks offered for a digest
/// that is not SHA-256 are refused as corrupt before anything is reserved.
#[tokio::test]
async fn chunks_for_a_digest_that_is_not_sha256_are_refused() {
    let spool = unreadable("not-sha256");
    let digest = Digest {
        hash: "not a sha-256 hash".to_owned(),
        size_bytes: 3,
    };
    let chunks = Box::pin(stream::iter([Ok(b"abc".to_vec())]));
    let refused = spool.put_chunks(digest, chunks).await.expect_err("refused");
    assert!(
        format!("{refused:?}").starts_with("Corrupt("),
        "{refused:?}"
    );
    assert!(spool.records().is_empty());
}
