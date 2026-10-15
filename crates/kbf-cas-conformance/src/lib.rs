//! What every implementation of the daemon's data plane must do.
//!
//! The data plane is [`kbf_transfer::Cas`]: the operations a worker daemon uses against
//! the farm's blob store (find which blobs are missing, store an action's outputs as
//! records or a spool, read a blob whole, to a file or as chunks). Today there is one
//! implementation, [`kbf_transfer::Transfers`], which speaks gRPC to a server; a daemon
//! linked into the same process as the server will have another that calls the server's
//! store directly. Both must be indistinguishable to a runtime, and this crate is how
//! that is kept true: one set of checks, generic over the implementation.
//!
//! An implementation's tests give [`Subject`] and run the checks with [`tests!`], which
//! makes one `#[tokio::test]` per check, so a failure names the behaviour. The checks
//! are also public functions, for a test that needs one alone.
//!
//! The checks assume the implementation answers [`Cas::missing`] from the store: the
//! trait's default (every digest missing) is for test stores whose puts need no
//! question, and fails [`missing_is_the_absent_digests_in_order`].

use std::fs::File;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;

use futures::stream;
use kbf_proto::reapi::Digest;
use kbf_transfer::{
    Cas, CasError, Chunks, FileSource, Record, RecordHasher, Source, Spool, Stamp,
    WRITE_CHUNK_BYTES, digest_of, fetch, label, proto_digest,
};
use kbf_types::LeaseId;

/// An implementation under test.
pub trait Subject {
    /// The data plane being checked.
    type Cas: Cas;

    /// A store with nothing in it, as a daemon sees it. Called once per check.
    fn fresh(&self) -> impl Future<Output = Arc<Self::Cas>>;

    /// A directory the checks may make files in (spools, files to upload, files to read
    /// into). It need not be empty; the checks use names of their own.
    fn scratch(&self) -> &Path;
}

/// `len` bytes that differ for each `seed` and repeat only every 251 bytes, so a blob
/// that loses, repeats or reorders a chunk is not equal to the original.
#[must_use]
pub fn patterned(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed * 7) % 251) as u8)
        .collect()
}

/// `bytes` cut into `chunk`-byte pieces, as [`Cas::put_chunks`] takes them.
fn chunked(bytes: &[u8], chunk: usize) -> Chunks {
    let pieces: Vec<std::io::Result<Vec<u8>>> =
        bytes.chunks(chunk).map(|c| Ok(c.to_vec())).collect();
    Box::pin(stream::iter(pieces))
}

/// Catches a store that loses, repeats or reorders bytes, or answers another digest than
/// the bytes': an empty blob, a small one and one of several chunks go in with `put` and
/// come back equal and verified through `get`.
pub async fn blobs_round_trip<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let large = patterned(1, 2 * WRITE_CHUNK_BYTES + 12_345);
    for bytes in [Vec::new(), b"small".to_vec(), large] {
        let digest = cas.put(bytes.clone()).await.expect("put");
        assert_eq!(digest, digest_of(&bytes));
        assert_eq!(fetch(&*cas, &digest).await.expect("fetch"), bytes);
    }
}

/// Catches an absent blob read as an empty one, or reported as any other failure than
/// `Missing`, which callers treat as "upload it" and not as "retry".
pub async fn an_absent_blob_is_missing<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let absent = digest_of(b"never stored");
    assert_eq!(
        cas.get(&absent).await,
        Err(CasError::Missing(label(&absent)))
    );
}

/// Catches a FindMissing that answers a held blob as missing (the blob is sent again),
/// an absent one as held (it is never sent), the answer out of the order asked, or a
/// question about nothing answered with something.
pub async fn missing_is_the_absent_digests_in_order<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let (a, b) = (
        cas.put(patterned(1, 100)).await.expect("put"),
        cas.put(patterned(2, 10_000)).await.expect("put"),
    );
    let (x, y) = (digest_of(b"absent x"), digest_of(b"absent y"));
    let asked = [y.clone(), a.clone(), x.clone(), b.clone()];
    assert_eq!(cas.missing(&asked).await, [y, x]);
    assert!(cas.missing(&[]).await.is_empty());
    assert!(cas.missing(&[a, b]).await.is_empty());
}

/// Catches a streamed upload that loses or reorders bytes across chunk boundaries
/// (chunks smaller than, equal to and larger than the blob, and an empty blob), and one
/// whose bytes are not the digest's taken as stored: the digest it claimed stays
/// missing.
pub async fn chunks_are_stored_and_checked<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let bytes = patterned(3, 3000);
    let digest = digest_of(&bytes);
    for chunk in [1, 700, 3000, 4000] {
        let fresh = subject.fresh().await;
        let stored = fresh
            .put_chunks(digest.clone(), chunked(&bytes, chunk))
            .await;
        assert_eq!(stored, Ok(digest.clone()), "chunks of {chunk}");
        assert_eq!(fetch(&*fresh, &digest).await.expect("fetch"), bytes);
    }
    let empty = digest_of(b"");
    assert_eq!(
        cas.put_chunks(empty.clone(), chunked(b"", 1)).await,
        Ok(empty)
    );
    let claimed = digest_of(b"what the digest says");
    let refused = cas
        .put_chunks(claimed.clone(), chunked(b"what the bytes are", 5))
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(cas.missing(std::slice::from_ref(&claimed)).await, [claimed]);
}

/// Catches `read_to` writing other bytes than the blob's, writing from the start of the
/// file instead of its current offset, leaving bytes unflushed, or a blob that is
/// absent taken as read.
pub async fn read_to_writes_the_blob_at_the_offset<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let bytes = patterned(4, WRITE_CHUNK_BYTES + 321);
    let digest = cas.put(bytes.clone()).await.expect("put");
    let path = subject.scratch().join("conformance-read-to");
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    tokio::io::AsyncWriteExt::write_all(&mut file, b"head")
        .await
        .expect("head");
    cas.read_to(&digest, &mut file).await.expect("read_to");
    let written = std::fs::read(&path).expect("read back");
    assert_eq!(&written[..4], b"head");
    assert_eq!(&written[4..], &bytes[..]);
    let absent = digest_of(b"not in this store");
    assert_eq!(
        cas.read_to(&absent, &mut file).await,
        Err(CasError::Missing(label(&absent)))
    );
}

/// Catches `put_records` storing nothing for one kind of source: records of bytes in
/// memory, of files the caller hashed and of a spool all end up in the store, an empty
/// set is stored, and a set already held (all of it, and half of it with new blobs) is
/// stored again without error.
pub async fn records_are_stored<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    cas.put_records(Vec::new()).await.expect("no records");
    let memory = [patterned(5, 10), patterned(6, 100_000)];
    let mut records: Vec<Record> = memory.iter().cloned().map(Record::bytes).collect();
    let mut digests: Vec<Digest> = memory.iter().map(|b| digest_of(b)).collect();
    let files = [patterned(7, 4000), patterned(8, 2 * WRITE_CHUNK_BYTES + 5)];
    let (on_disk, on_disk_digests) = file_records(subject.scratch(), &files);
    records.extend(on_disk);
    digests.extend(on_disk_digests);
    assert_eq!(cas.missing(&digests).await, digests, "nothing is held yet");
    cas.put_records(records.clone()).await.expect("put records");
    assert!(
        cas.missing(&digests).await.is_empty(),
        "every record is held"
    );
    for (bytes, digest) in memory.iter().chain(&files).zip(&digests) {
        assert_eq!(fetch(&*cas, digest).await.expect("fetch"), *bytes);
    }
    cas.put_records(records.clone()).await.expect("put again");
    let more = Record::bytes(patterned(9, 500));
    let new = digest_of(&patterned(9, 500));
    records.push(more);
    cas.put_records(records).await.expect("put half held");
    assert!(cas.missing(&[new]).await.is_empty());
}

/// Files in `dir`, hashed the way a driver's walk hashes them, as records whose bytes
/// are read from the files when they are sent; and their digests.
fn file_records(dir: &Path, blobs: &[Vec<u8>]) -> (Vec<Record>, Vec<Digest>) {
    let root = Arc::new(OwnedFd::from(File::open(dir).expect("open the directory")));
    let dirs: Arc<[String]> = Arc::from(Vec::<String>::new());
    let (mut records, mut digests) = (Vec::new(), Vec::new());
    for (i, blob) in blobs.iter().enumerate() {
        let name = format!("conformance-file-{i}");
        std::fs::write(dir.join(&name), blob).expect("write");
        let stamp = Stamp::of(&File::open(dir.join(&name)).expect("open")).expect("stamp");
        let mut hasher = RecordHasher::new();
        hasher.update(blob);
        let (digest, crc32c) = hasher.finish();
        digests.push(proto_digest(&digest));
        records.push(Record {
            digest,
            crc32c,
            source: Source::File(FileSource::new(
                Arc::clone(&root),
                Arc::clone(&dirs),
                name,
                stamp,
            )),
        });
    }
    (records, digests)
}

/// Catches a spool whose blobs are not all stored: outputs gathered in one file go in
/// with one `put_spool`, and a blob put in the spool twice is stored once without error.
pub async fn a_spool_is_stored<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let spool = Arc::new(Spool::create(subject.scratch()).expect("spool"));
    let blobs: Vec<Vec<u8>> = (0..40).map(|i| patterned(i, 300 + 77 * i)).collect();
    let mut digests = Vec::new();
    for blob in &blobs {
        digests.push(spool.put(blob.clone()).await.expect("spool put"));
    }
    spool.put(blobs[0].clone()).await.expect("spool put twice");
    cas.put_spool(spool).await.expect("put spool");
    assert!(cas.missing(&digests).await.is_empty());
    for (blob, digest) in blobs.iter().zip(&digests) {
        assert_eq!(fetch(&*cas, digest).await.expect("fetch"), *blob);
    }
}

/// Catches a lease's view of the store that is not the store: what a lease puts is
/// readable from the store and from another lease, and what the store holds is
/// readable from a lease.
pub async fn a_lease_sees_the_same_store<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let (one, two) = (
        cas.lease(&LeaseId::new(1, 1)),
        cas.lease(&LeaseId::new(2, 1)),
    );
    let from_lease = one.put(b"from a lease".to_vec()).await.expect("put");
    assert_eq!(
        fetch(&*cas, &from_lease).await.expect("store"),
        b"from a lease"
    );
    assert_eq!(
        fetch(&two, &from_lease).await.expect("other"),
        b"from a lease"
    );
    let from_store = cas.put(b"from the store".to_vec()).await.expect("put");
    assert_eq!(
        fetch(&one, &from_store).await.expect("lease"),
        b"from the store"
    );
    assert!(cas.in_flight() >= 1);
}

/// Catches two uploads of one blob at once failing or storing it damaged: the engine
/// shares the work, an in-process store races its writes.
pub async fn concurrent_puts_of_one_blob_agree<S: Subject>(subject: &S) {
    let cas = subject.fresh().await;
    let bytes = patterned(10, 3 * WRITE_CHUNK_BYTES);
    let digest = digest_of(&bytes);
    let puts = (0..4).map(|_| cas.put(bytes.clone()));
    for stored in futures::future::join_all(puts).await {
        assert_eq!(stored, Ok(digest.clone()));
    }
    assert_eq!(fetch(&*cas, &digest).await.expect("fetch"), bytes);
}

/// An implementation whose server can refuse its caller: the calls a daemon makes are
/// admitted by the server's deny list, whatever carries them.
pub trait Refusable: Subject {
    /// Puts the caller of every store this subject made on the server's deny list
    /// (by whatever the subject was made to name: the node, or its certificate),
    /// from the next call on.
    fn deny(&self) -> impl Future<Output = ()>;
}

/// Catches a store that serves a caller the server's deny list refuses, on one
/// implementation and not another: a security check must not depend on how the calls
/// travel. Before the denial, a write and a read work. After it, a read, a write and an
/// upload of records all fail (a read is not `Missing`, which a caller would answer by
/// uploading), and FindMissing answers every digest missing, so nothing is taken as
/// held.
pub async fn a_denied_caller_is_refused<S: Refusable>(subject: &S) {
    let cas = subject.fresh().await;
    let bytes = patterned(11, 1000);
    let held = cas.put(bytes.clone()).await.expect("put before the denial");
    assert_eq!(
        fetch(&*cas, &held).await.expect("read before the denial"),
        bytes
    );
    subject.deny().await;
    let read = cas.get(&held).await;
    assert!(
        read.is_err() && !matches!(read, Err(CasError::Missing(_))),
        "a denied read: {:?}",
        read.as_ref().map(Vec::len)
    );
    let put = cas.put(patterned(12, 1000)).await;
    assert!(put.is_err(), "a denied put: {put:?}");
    let records = cas
        .put_records(vec![Record::bytes(patterned(13, 1000))])
        .await;
    assert!(records.is_err(), "a denied upload: {records:?}");
    assert_eq!(cas.missing(std::slice::from_ref(&held)).await, [held]);
}

/// Makes one `#[tokio::test]` per check, each against the [`Subject`] the expression
/// `$subject` builds (called once per test). The calling crate needs `tokio` with the
/// `macros` and `rt-multi-thread` features.
///
/// ```ignore
/// kbf_cas_conformance::tests!(RemoteSubject::new());
/// ```
#[macro_export]
macro_rules! tests {
    (@each $subject:expr; $($check:ident)*) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $check() {
                $crate::$check(&$subject).await;
            }
        )*
    };
    ($subject:expr) => {
        $crate::tests!(@each $subject;
            blobs_round_trip
            an_absent_blob_is_missing
            missing_is_the_absent_digests_in_order
            chunks_are_stored_and_checked
            read_to_writes_the_blob_at_the_offset
            records_are_stored
            a_spool_is_stored
            a_lease_sees_the_same_store
            concurrent_puts_of_one_blob_agree
        );
    };
}
