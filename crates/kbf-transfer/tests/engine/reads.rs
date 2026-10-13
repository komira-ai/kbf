//! Reads: a broken connection resumed, streamed reads into files, and what the
//! engine refuses (bytes that do not match their digest, a file it cannot write).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kbf_transfer::{Cas, CasError, TransferConfig, digest_of, label};
use kbf_types::LeaseId;

use super::fake::{Fake, until};
use super::proxy::Proxy;
use super::{engine, patterned};

fn scratch(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// T-2 (docs/design/worker-cache.md#10-test-plan). Four reads are in flight, two into
/// memory and two streamed into files, each having received its first 1,000 of 3,000
/// bytes, when every connection is cut. Each stream fails, the engine connects again,
/// and each read resumes at offset 1,000: every blob hashes right, `retries` is 4 (one
/// per stream in flight), the server saw each blob read at 0 and then at 1,000, and two
/// connections were made. Catches no retry, and a resume that repeats or skips bytes.
/// Mutants seen red: resume at offset 0 while appending (corrupt: "more than 3000
/// bytes"); no retry (UNKNOWN, "h2 protocol error: error reading a body from
/// connection").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t2_a_cut_connection_resumes_every_read_from_its_offset() {
    let fake = Arc::new(Fake::default());
    let blobs: Vec<Vec<u8>> = (0..4).map(|i| patterned(i, 3000)).collect();
    let digests: Vec<_> = blobs.iter().map(|b| fake.insert(b.clone())).collect();
    fake.message_bytes.store(1000, Ordering::SeqCst);
    fake.pause.store(true, Ordering::SeqCst);
    let (proxy, addr) = Proxy::start(fake.serve().await).await;
    let config = TransferConfig {
        memory_blob_bytes: 2000,
        ..TransferConfig::default()
    };
    let engine = engine(addr, config);
    let scope = Arc::new(engine.scope(&LeaseId::new(5, 1)));
    fake.close();
    let mut memory = Vec::new();
    for digest in &digests[..2] {
        let (scope, digest) = (Arc::clone(&scope), digest.clone());
        memory.push(tokio::spawn(async move { scope.get(&digest).await }));
    }
    let mut files = Vec::new();
    for (i, digest) in digests[2..].iter().enumerate() {
        let (scope, digest) = (Arc::clone(&scope), digest.clone());
        let path = scratch(&format!("t2-{i}"));
        files.push((
            path.clone(),
            tokio::spawn(async move {
                let mut file = tokio::fs::File::create(&path).await.expect("create");
                scope.read_to(&digest, &mut file).await
            }),
        ));
    }
    until("every read has its first 1,000 bytes", || {
        scope.stats().read_bytes == 4000
    })
    .await;
    proxy.cut();
    fake.open();
    for (read, digest) in memory.into_iter().zip(&digests) {
        let read = read.await.expect("read");
        assert_eq!(read.map(|bytes| digest_of(&bytes)).as_ref(), Ok(digest));
    }
    for ((path, read), digest) in files.into_iter().zip(&digests[2..]) {
        assert_eq!(read.await.expect("read"), Ok(()));
        assert_eq!(digest_of(&std::fs::read(&path).expect("file")), *digest);
    }
    let stats = scope.stats();
    assert_eq!(
        (stats.retries, stats.read_calls, stats.read_bytes),
        (4, 8, 12_000)
    );
    for digest in &digests {
        assert_eq!(fake.reads_of(digest), [0, 1000], "{digest:?}");
    }
    assert_eq!(proxy.accepted.load(Ordering::SeqCst), 2, "connections");
}

/// Catches bytes the server sends that do not match the digest being trusted: more
/// bytes than the digest says (stopped as soon as they pass it, so a lying server
/// cannot fill memory), fewer, and as many but other bytes; and NOT_FOUND reported as
/// anything but missing.
#[tokio::test]
async fn bytes_that_do_not_match_their_digest_are_refused() {
    let fake = Arc::new(Fake::default());
    let engine = engine(fake.serve().await, TransferConfig::default());
    let long = digest_of(b"abc");
    fake.lie(&long, b"abcdef".to_vec());
    let error = engine.get(&long).await.expect_err("too long");
    assert_eq!(
        error,
        CasError::Corrupt(label(&long), "more than 3 bytes".to_owned())
    );
    let short = digest_of(b"abcdef");
    fake.lie(&short, b"abc".to_vec());
    let error = engine.get(&short).await.expect_err("too short");
    assert_eq!(
        error,
        CasError::Corrupt(label(&short), label(&digest_of(b"abc")))
    );
    let other = digest_of(b"xyz");
    fake.lie(&other, b"abc".to_vec());
    let error = engine.get(&other).await.expect_err("other bytes");
    assert_eq!(
        error,
        CasError::Corrupt(label(&other), label(&digest_of(b"abc")))
    );
    let absent = digest_of(b"absent");
    assert_eq!(
        engine.get(&absent).await,
        Err(CasError::Missing(label(&absent)))
    );
}

/// Catches a read into a file that keeps a write failure to itself, or reports it as
/// the CAS failing: into a file opened read-only, a blob streamed in several messages,
/// one read through memory and written whole (3 MiB: tokio reports the failure of its
/// first write to the second), and a small one (reported by the flush) are each
/// [`CasError::Write`]. Also a streamed read that does not check what it wrote: a lying
/// server's blob streamed into a file is corrupt.
#[tokio::test]
async fn a_read_into_a_file_it_cannot_write_is_a_write_error() {
    let fake = Arc::new(Fake::default());
    fake.message_bytes.store(8, Ordering::SeqCst);
    let config = TransferConfig {
        memory_blob_bytes: 4 << 20,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    let path = scratch("read-only");
    std::fs::write(&path, b"").expect("create");
    let streamed = patterned(1, (4 << 20) + 20);
    for bytes in [b"small".to_vec(), patterned(2, 3 << 20), streamed] {
        let digest = fake.insert(bytes);
        let mut file = tokio::fs::File::open(&path).await.expect("open");
        let error = engine
            .read_to(&digest, &mut file)
            .await
            .expect_err("read-only");
        assert!(
            matches!(&error, CasError::Write(blob, _) if *blob == label(&digest)),
            "{error:?}"
        );
    }
    let lied = digest_of(b"twenty bytes of data");
    fake.lie(&lied, b"twenty other bytes!!".to_vec());
    let config = TransferConfig {
        memory_blob_bytes: 10,
        ..TransferConfig::default()
    };
    let engine = super::engine(fake.serve().await, config);
    let mut file = tokio::fs::File::create(scratch("lied"))
        .await
        .expect("create");
    let error = engine.read_to(&lied, &mut file).await.expect_err("corrupt");
    assert!(matches!(&error, CasError::Corrupt(..)), "{error:?}");
}

/// Catches a lease's scope that is not the engine's (a scope taken from a scope must
/// share its limits, counted for its own lease), and an in-flight limit other than
/// `--transfer-in-flight` told to the walks.
#[tokio::test]
async fn a_scope_of_a_scope_shares_the_engine() {
    let fake = Arc::new(Fake::default());
    let digest = fake.insert(b"shared".to_vec());
    let config = TransferConfig {
        in_flight: 5,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    assert_eq!(engine.in_flight(), 5);
    // The engine's own `Cas::lease`, as a runtime takes it.
    let scope = Arc::new(engine.lease(&LeaseId::new(8, 1)));
    let inner = scope.lease(&LeaseId::new(8, 2));
    assert_eq!(inner.in_flight(), 5);
    assert_eq!(inner.get(&digest).await, Ok(b"shared".to_vec()));
    assert_eq!((inner.stats().read_calls, scope.stats().read_calls), (1, 0));
}
