//! What the conformance checks do not reach: a digest that does not parse, a server half
//! that is gone, and a file that changed after the walk hashed it.

use std::os::fd::OwnedFd;

use kbf_front::{Cache, SegmentsConfig};
use kbf_transfer::{FileSource, RecordHasher, Source, Stamp, digest_of, fetch};

use super::*;

/// The plane over a new in-memory cache, its calls run on `server`.
fn plane(server: &tokio::runtime::Runtime) -> LocalDataPlane {
    let _context = server.enter();
    let store = LocalStore::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    LocalDataPlane::new(store, server.handle().clone())
}

/// A runtime standing in for the server half.
fn server() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime")
}

/// Catches a digest that is not one (a hash that is not hex) read as an empty blob or
/// as anything but `Missing`, and a FindMissing that holds one answered as anything but
/// every digest missing (what the transfer engine answers when the server refuses such
/// a call). Beside them, in the same test binary as the failures (each test binary has
/// its own copy of the plane), a held blob reads back and an absent one is `Missing`.
#[test]
fn a_digest_that_does_not_parse_is_missing() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let bad = Digest {
            hash: "not hex".to_owned(),
            size_bytes: 3,
        };
        assert_eq!(plane.get(&bad).await, Err(CasError::Missing(label(&bad))));
        let held = plane.put(b"held".to_vec()).await.expect("put");
        assert_eq!(
            plane.missing(&[held.clone(), bad.clone()]).await,
            [held.clone(), bad]
        );
        assert!(plane.missing(std::slice::from_ref(&held)).await.is_empty());
        assert_eq!(plane.get(&held).await, Ok(b"held".to_vec()));
        let absent = digest_of(b"absent");
        assert_eq!(
            plane.get(&absent).await,
            Err(CasError::Missing(label(&absent)))
        );
    });
}

/// Catches a call into a server half whose runtime is gone panicking, hanging, or
/// taken for an answer: every read and write fails UNAVAILABLE (a read is not
/// `Missing`, which callers would answer by uploading), and FindMissing answers every
/// digest missing.
#[test]
fn a_gone_server_half_fails_every_call() {
    let gone = server();
    let plane = plane(&gone);
    drop(gone);
    let caller = server();
    caller.block_on(async {
        let digest = digest_of(b"anything");
        let read = plane.get(&digest).await;
        assert!(matches!(read, Err(CasError::Unavailable(..))), "{read:?}");
        let put = plane.put(b"anything".to_vec()).await;
        assert!(matches!(put, Err(CasError::Unavailable(..))), "{put:?}");
        assert_eq!(plane.missing(std::slice::from_ref(&digest)).await, [digest]);
        let records = plane
            .put_records(vec![Record::bytes(b"output".to_vec())])
            .await;
        assert!(
            matches!(records, Err(CasError::Unavailable(..))),
            "{records:?}"
        );
    });
}

/// Catches a file stored with other bytes than the walk hashed: one rewritten after its
/// stamp was taken fails the upload with `Read`, and its digest stays missing.
#[test]
fn a_file_changed_after_the_walk_is_not_stored() {
    // Under the test binary's directory: unit tests get no `CARGO_TARGET_TMPDIR`.
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-all-in-one-unit")
        .join(format!("changed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    let walked = b"what the walk hashed";
    std::fs::write(dir.join("out"), walked).expect("write");
    let stamp = Stamp::of(&std::fs::File::open(dir.join("out")).expect("open")).expect("stamp");
    let mut hasher = RecordHasher::new();
    hasher.update(walked);
    let (digest, crc32c) = hasher.finish();
    // Longer than before, so the stamp's size differs whatever the clock's grain.
    std::fs::write(dir.join("out"), b"what the action wrote later").expect("rewrite");
    let root = OwnedFd::from(std::fs::File::open(&dir).expect("open the directory"));
    let source = FileSource::new(
        Arc::new(root),
        Arc::from(Vec::new()),
        "out".to_owned(),
        stamp,
    );
    let record = Record {
        digest,
        crc32c,
        source: Source::File(source),
    };
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let stored = plane.put_records(vec![record]).await;
        assert!(matches!(stored, Err(CasError::Read(..))), "{stored:?}");
        let asked = [proto_digest(&digest)];
        assert_eq!(plane.missing(&asked).await, asked);
        assert!(fetch(&plane, &asked[0]).await.is_err());
    });
}

/// Catches a blob too large to hold gathered whole in memory without a write tier
/// (mutant: the size check dropped, the whole-body put kept for it): `put_chunks` of a
/// blob one byte over `MAX_UNSTAGED_BLOB_BYTES` is refused before a chunk is read, with
/// the reason; one of a few bytes is gathered and stored, and one whose bytes are not
/// its digest's is refused as corrupt.
#[test]
fn put_chunks_refuses_a_large_blob_and_stores_a_small_one() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let large = Digest {
            hash: "00".repeat(32),
            size_bytes: i64::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size"),
        };
        let never: Chunks = Box::pin(futures::stream::once(async {
            panic!("a chunk of a refused blob was read")
        }));
        let why = plane.put_chunks(large, never).await.expect_err("refused");
        assert!(why.to_string().contains("no write tier has room"), "{why}");
        let small = digest_of(b"small");
        let chunks: Chunks = Box::pin(futures::stream::iter([
            Ok(b"sma".to_vec()),
            Ok(b"ll".to_vec()),
        ]));
        assert_eq!(plane.put_chunks(small.clone(), chunks).await, Ok(small));
        let chunks: Chunks = Box::pin(futures::stream::iter([Ok(b"other".to_vec())]));
        let wrong = plane.put_chunks(digest_of(b"small!"), chunks).await;
        assert!(matches!(wrong, Err(CasError::Corrupt(..))), "{wrong:?}");
        let failing: Chunks = Box::pin(futures::stream::iter([Err(std::io::Error::other("gone"))]));
        let read = plane.put_chunks(digest_of(b"x"), failing).await;
        assert!(matches!(read, Err(CasError::Read(..))), "{read:?}");
    });
}
