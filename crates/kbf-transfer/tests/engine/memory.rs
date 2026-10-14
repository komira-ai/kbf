//! The [`Cas`] trait's own methods, over a store in memory: the defaults a store
//! without streaming gets, and a store behind an `Arc`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, DEFAULT_IN_FLIGHT, digest_of, fetch, label};
use kbf_types::LeaseId;

use super::fake::lock;
use super::front::{chunked, failing};

/// A CAS in memory that returns whatever bytes are stored under a digest.
#[derive(Debug, Default)]
struct MemoryCas {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryCas {
    fn blob(&self, digest: &Digest) -> Option<Vec<u8>> {
        lock(&self.blobs).get(&label(digest)).cloned()
    }
}

impl Cas for MemoryCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.blob(digest)
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        lock(&self.blobs).insert(label(&digest), bytes);
        Ok(digest)
    }
}

/// Catches bytes that do not match their digest being trusted, and a missing blob
/// reported as anything else.
#[tokio::test]
async fn fetch_refuses_bytes_that_do_not_match() {
    let cas = MemoryCas::default();
    let digest = cas.put(b"good".to_vec()).await.expect("put");
    assert_eq!(fetch(&cas, &digest).await, Ok(b"good".to_vec()));
    lock(&cas.blobs).insert(label(&digest), b"evil".to_vec());
    let error = fetch(&cas, &digest).await.expect_err("refused");
    assert_eq!(
        error,
        CasError::Corrupt(label(&digest), label(&digest_of(b"evil")))
    );
    let absent = digest_of(b"absent");
    assert_eq!(
        fetch(&cas, &absent).await,
        Err(CasError::Missing(label(&absent)))
    );
}

/// Catches the trait's default `put_chunks` storing other bytes than the chunks',
/// taking a digest that does not match as stored, or a failed chunk as the end.
#[tokio::test]
async fn the_default_put_chunks_gathers_and_checks_its_digest() {
    let cas = MemoryCas::default();
    let (bytes, digest, chunks) = chunked(3000, 700);
    assert_eq!(
        cas.put_chunks(digest.clone(), chunks).await,
        Ok(digest.clone())
    );
    assert_eq!(cas.blob(&digest), Some(bytes));
    let (_, _, chunks) = chunked(10, 3);
    let wrong = digest_of(b"something else");
    let error = cas
        .put_chunks(wrong.clone(), chunks)
        .await
        .expect_err("mismatch");
    assert!(
        matches!(&error, CasError::Corrupt(blob, _) if *blob == label(&wrong)),
        "{error:?}"
    );
    let error = cas
        .put_chunks(wrong, failing(b"x"))
        .await
        .expect_err("unreadable");
    assert!(matches!(&error, CasError::Read(..)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("could not be read for storing: disk gone")
    );
}

/// Catches the default `read_to` writing other bytes than the blob's, leaving them
/// unflushed, or a file it cannot write taken as written; the default in-flight
/// limit drifting from the engine's; and the default `missing` leaving out a digest
/// (a blob never stored).
#[tokio::test]
async fn the_default_read_to_writes_the_checked_blob() {
    let cas = MemoryCas::default();
    let digest = cas.put(b"to a file".to_vec()).await.expect("put");
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("default-read-to");
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    cas.read_to(&digest, &mut file).await.expect("read_to");
    assert_eq!(std::fs::read(&path).expect("read"), b"to a file");
    let mut read_only = tokio::fs::File::open(&path).await.expect("open");
    let error = cas
        .read_to(&digest, &mut read_only)
        .await
        .expect_err("read-only");
    assert!(
        matches!(&error, CasError::Write(blob, _) if *blob == label(&digest)),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .starts_with(&format!("blob {} could not be written: ", label(&digest)))
    );
    assert_eq!(cas.in_flight(), DEFAULT_IN_FLIGHT);
    // A store that does not ask answers every digest missing, in the order asked, the
    // one it holds too: its puts store them.
    let absent = digest_of(b"absent");
    assert_eq!(
        cas.missing(&[absent.clone(), digest.clone()]).await,
        [absent, digest]
    );
}

/// Catches a store behind an `Arc` that is not the same store: every method reaches
/// the one inside.
#[tokio::test]
async fn a_shared_store_is_the_same_store() {
    let cas = Arc::new(Arc::new(MemoryCas::default()));
    let lease = cas.lease(&LeaseId::new(3, 4));
    let digest = lease.put(b"shared".to_vec()).await.expect("put");
    assert_eq!(cas.get(&digest).await, Ok(b"shared".to_vec()));
    let (bytes, chunked_digest, chunks) = chunked(100, 30);
    assert_eq!(
        cas.put_chunks(chunked_digest.clone(), chunks).await,
        Ok(chunked_digest.clone())
    );
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("shared-read-to");
    let mut file = tokio::fs::File::create(&path).await.expect("create");
    cas.read_to(&chunked_digest, &mut file)
        .await
        .expect("read_to");
    assert_eq!(std::fs::read(&path).expect("read"), bytes);
    assert_eq!(cas.in_flight(), DEFAULT_IN_FLIGHT);
    assert_eq!(cas.missing(std::slice::from_ref(&digest)).await, [digest]);
    assert!(Arc::ptr_eq(&lease, &*cas));
}
