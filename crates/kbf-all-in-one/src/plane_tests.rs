//! What the conformance checks do not reach: a digest that does not parse, a server half
//! that is gone, and a file that changed after the walk hashed it.

use std::os::fd::OwnedFd;

use kbf_front::{Cache, MetaLog as _, SegmentsConfig};
use kbf_transfer::{FileSource, RecordHasher, Source, Stamp, digest_of, fetch};

use super::*;

/// The plane over a new in-memory cache, its calls run on `server`.
fn plane(server: &tokio::runtime::Runtime) -> LocalDataPlane {
    let _context = server.enter();
    let store = LocalStore::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    LocalDataPlane::new(store, server.handle().clone())
}

/// A file that takes any bytes and keeps none.
fn null_file() -> tokio::fs::File {
    let file = std::fs::OpenOptions::new().write(true).open("/dev/null");
    tokio::fs::File::from_std(file.expect("/dev/null"))
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
        let mut sink = null_file();
        let into = plane.read_to(&bad, &mut sink).await;
        assert_eq!(into, Err(CasError::Missing(label(&bad))));
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
        let into = plane.read_to(&digest, &mut null_file()).await;
        assert!(matches!(into, Err(CasError::Unavailable(..))), "{into:?}");
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

/// Catches a `read_to` that writes other bytes than the blob's, takes a blob whose
/// bytes fail their digest (past its first window) for read, answers a file it cannot
/// write as anything but `Write`, or a blob not held as anything but `Missing`. A held
/// blob reads into its file; one of 9 MiB with its last byte flipped in the store is
/// `Unavailable`; a file opened read-only is `Write`; an absent blob is `Missing`.
#[test]
fn read_to_writes_the_blob_or_says_why_not() {
    use kbf_meta::BlobAnswer;
    use kbf_objstore::{ByteRange, ObjectStore};
    let server = server();
    let cache = {
        let _context = server.enter();
        Arc::new(Cache::memory())
    };
    let store = {
        let _context = server.enter();
        LocalStore::new(Arc::clone(&cache), SegmentsConfig::DEFAULT)
    };
    let plane = LocalDataPlane::new(store, server.handle().clone());
    let path = std::env::temp_dir().join(format!("kbf-read-to-{}", std::process::id()));
    server.block_on(async {
        let held = plane.put(b"held bytes".to_vec()).await.expect("put");
        let mut file = tokio::fs::File::create(&path).await.expect("create");
        plane.read_to(&held, &mut file).await.expect("read");
        assert_eq!(std::fs::read(&path).expect("read back"), b"held bytes");
        let mut read_only = tokio::fs::File::open(&path).await.expect("open");
        let refused = plane.read_to(&held, &mut read_only).await;
        assert!(matches!(refused, Err(CasError::Write(..))), "{refused:?}");
        let absent = digest_of(b"absent");
        let missing = plane.read_to(&absent, &mut file).await;
        assert_eq!(missing, Err(CasError::Missing(label(&absent))));

        let large = vec![5u8; 9 << 20];
        let digest = plane.put(large.clone()).await.expect("put");
        let parsed = parse(&digest).expect("a digest");
        let answer = cache
            .meta()
            .query(move |s| s.blob(&parsed))
            .await
            .expect("query");
        let BlobAnswer::Present { location, .. } = answer else {
            panic!("{answer:?}");
        };
        let key = cache.object_key(location.object).expect("key");
        let all = ByteRange::new(0, u64::from(u32::MAX)).expect("range");
        let mut stored = cache
            .objects()
            .get_range(&key, all)
            .await
            .expect("get")
            .to_vec();
        let last = usize::try_from(location.offset).expect("offset") + large.len() - 1;
        stored[last] ^= 1;
        cache.objects().delete(&key).await.expect("delete");
        let replaced = cache.objects().put_new(&key, stored.into(), None).await;
        replaced.expect("put");
        let damaged = plane.read_to(&digest, &mut file).await;
        assert!(
            matches!(damaged, Err(CasError::Unavailable(..))),
            "{damaged:?}"
        );
    });
    let _ = std::fs::remove_file(&path);
}
