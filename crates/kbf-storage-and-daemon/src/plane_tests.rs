//! What the conformance checks do not reach: a digest that does not parse, a server half
//! that is gone, a file that changed after the walk hashed it, and blobs too large to
//! gather streamed to the store.

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
    let beside = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-all-in-one-unit");
    let dir = kbf_testkit::dir::TestDir::under(beside, "changed");
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

/// `len` bytes that differ for each `seed`.
fn patterned(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 253) as u8))
        .collect()
}

/// `bytes` as chunks of a megabyte.
fn chunked(bytes: &[u8]) -> Chunks {
    let chunks: Vec<std::io::Result<Vec<u8>>> =
        bytes.chunks(1 << 20).map(|c| Ok(c.to_vec())).collect();
    Box::pin(futures::stream::iter(chunks))
}

/// Catches a blob too large to gather refused, or gathered whole (defects: the refusal
/// restored, the size check dropped): without a write tier, `put_chunks` of a blob one
/// byte over `MAX_UNSTAGED_BLOB_BYTES` is streamed to the store and held after; again,
/// once held, it is not read at all; one whose bytes are not its digest's is `Corrupt`
/// and not held; one whose chunk fails to be read is `Read`; one with more bytes than
/// its digest names, which the store refuses, is `Unavailable`; a digest that does not
/// parse is refused. A small blob is gathered and stored as before.
#[test]
fn put_chunks_streams_a_large_blob_and_stores_a_small_one() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
        let bytes = patterned(9, size);
        let large = digest_of(&bytes);
        let stored = plane.put_chunks(large.clone(), chunked(&bytes)).await;
        assert_eq!(stored, Ok(large.clone()));
        assert!(plane.missing(std::slice::from_ref(&large)).await.is_empty());
        assert_eq!(plane.get(&large).await, Ok(bytes.clone()));
        let never: Chunks = Box::pin(futures::stream::once(async {
            panic!("a chunk of a held blob was read")
        }));
        assert_eq!(plane.put_chunks(large.clone(), never).await, Ok(large));

        let other = patterned(10, size);
        let claimed = digest_of(&patterned(11, size));
        let wrong = plane.put_chunks(claimed.clone(), chunked(&other)).await;
        assert!(
            matches!(&wrong, Err(CasError::Corrupt(blob, actual))
                if *blob == label(&claimed) && *actual == label(&digest_of(&other))),
            "{wrong:?}"
        );
        assert_eq!(
            plane.missing(std::slice::from_ref(&claimed)).await,
            [claimed]
        );

        let mut failing: Vec<std::io::Result<Vec<u8>>> = vec![Ok(vec![0; 1 << 20])];
        failing.push(Err(std::io::Error::other("gone")));
        let failing: Chunks = Box::pin(futures::stream::iter(failing));
        let unread = digest_of(&patterned(12, size));
        let read = plane.put_chunks(unread, failing).await;
        assert!(
            matches!(&read, Err(CasError::Read(_, why)) if why.contains("gone")),
            "{read:?}"
        );
        let mut longer = patterned(15, size);
        let claimed = digest_of(&longer);
        longer.extend_from_slice(&[0; 2 << 20]);
        let long = plane.put_chunks(claimed, chunked(&longer)).await;
        assert!(matches!(long, Err(CasError::Unavailable(..))), "{long:?}");
        let not_a_digest = Digest {
            hash: "not hex".to_owned(),
            size_bytes: i64::try_from(size).expect("size"),
        };
        let refused = plane.put_chunks(not_a_digest, chunked(&other)).await;
        assert!(
            matches!(refused, Err(CasError::Unavailable(..))),
            "{refused:?}"
        );

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

/// Catches an upload with a large record refused without a write tier, or failed
/// (defects: FAILED_PRECONDITION at `Begin` taken for a failure): `put_records` of a
/// record one byte over `MAX_UNSTAGED_BLOB_BYTES` and a small one, with no tier, stores
/// both, each by itself.
#[test]
fn without_a_tier_an_upload_with_a_large_record_is_stored_record_by_record() {
    let server = server();
    let plane = plane(&server);
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let large = patterned(13, size);
    let record = |bytes: &[u8]| {
        let mut hasher = RecordHasher::new();
        hasher.update(bytes);
        let (digest, crc32c) = hasher.finish();
        kbf_transfer::Record {
            digest,
            crc32c,
            source: Source::Bytes(bytes::Bytes::copy_from_slice(bytes)),
        }
    };
    let records = vec![record(&large), record(b"small")];
    let digests: Vec<Digest> = records.iter().map(|r| proto_digest(&r.digest)).collect();
    server.block_on(async {
        plane.put_records(records).await.expect("stored");
        assert!(plane.missing(&digests).await.is_empty());
        assert_eq!(plane.get(&digests[0]).await, Ok(large));
    });
}

/// Catches a streamed blob reported stored when the server half is gone: `put_chunks`
/// of a large blob after the server half's runtime shut down is `Unavailable`.
#[test]
fn put_chunks_of_a_large_blob_without_a_server_half_is_unavailable() {
    let server = server();
    let plane = plane(&server);
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let bytes = patterned(14, size);
    let digest = digest_of(&bytes);
    drop(server);
    let here = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let gone = here.block_on(plane.put_chunks(digest, chunked(&bytes)));
    assert!(
        matches!(&gone, Err(CasError::Unavailable(_, why)) if why.contains("did not answer")),
        "{gone:?}"
    );
}

/// `parts` as the chunks of a blob.
fn parts(parts: &[&[u8]]) -> Chunks {
    let chunks: Vec<std::io::Result<Vec<u8>>> = parts.iter().map(|p| Ok(p.to_vec())).collect();
    Box::pin(futures::stream::iter(chunks))
}

/// Catches a streamed store that drops a blob, re-reads one the cache holds, or says
/// stored for bytes it did not keep (defects: the held-blob return removed, so the
/// chunks are read again; the result taken for `Ok` whatever the store answered): a
/// blob streamed in two chunks is `Ok`, is held after and reads back; the same blob
/// again is `Ok` without one chunk read. `put_streamed` is called directly with small
/// blobs: through `put_chunks` it takes only blobs over `MAX_UNSTAGED_BLOB_BYTES`, and
/// the test of those is too slow for the fast run.
#[test]
fn a_streamed_blob_is_stored_and_not_read_again_once_held() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let digest = digest_of(b"streamed blob");
        let stored = plane
            .put_streamed(digest.clone(), parts(&[b"streamed", b" blob"]))
            .await;
        assert_eq!(stored, Ok(digest.clone()));
        assert!(
            plane
                .missing(std::slice::from_ref(&digest))
                .await
                .is_empty()
        );
        assert_eq!(plane.get(&digest).await, Ok(b"streamed blob".to_vec()));
        let never: Chunks = Box::pin(futures::stream::once(async {
            panic!("a chunk of a held blob was read")
        }));
        assert_eq!(plane.put_streamed(digest.clone(), never).await, Ok(digest));
    });
}

/// Catches bytes stored under a digest that is not theirs, or too many bytes taken for
/// the blob (defects: the `DigestMismatch` arm answering `Unavailable`, or the other
/// store errors answering `Corrupt`): a blob whose bytes hash to another digest is
/// `Corrupt`, naming the claimed digest and the one its bytes have, and is not held; one
/// with more bytes than its digest's size, which the store refuses, is `Unavailable`
/// and not held.
#[test]
fn a_streamed_blob_with_other_bytes_than_its_digest_is_refused() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let claimed = digest_of(b"the bytes claimed");
        let wrong = plane
            .put_streamed(claimed.clone(), parts(&[b"the bytes sent!!"]))
            .await;
        assert!(
            matches!(&wrong, Err(CasError::Corrupt(blob, actual))
                if *blob == label(&claimed) && *actual == label(&digest_of(b"the bytes sent!!"))),
            "{wrong:?}"
        );
        assert_eq!(
            plane.missing(std::slice::from_ref(&claimed)).await,
            [claimed]
        );

        // Far more than the digest names: the store refuses the body before it ends,
        // while a few bytes too many show only as another digest at the end (above).
        let named = digest_of(b"short");
        let flood = vec![b'x'; 4096];
        let long = plane
            .put_streamed(named.clone(), parts(&[b"short", &flood]))
            .await;
        assert!(matches!(long, Err(CasError::Unavailable(..))), "{long:?}");
        assert_eq!(plane.missing(std::slice::from_ref(&named)).await, [named]);
    });
}

/// Catches a chunk that could not be read reported as the store's failure, or the
/// upload taken for stored (defects: the note of the failed chunk dropped): a stream
/// whose second chunk fails is `Read` with the chunk's error, and the blob is not held.
#[test]
fn a_streamed_blob_whose_chunk_cannot_be_read_is_a_read_failure() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let digest = digest_of(b"unreadable blob");
        let chunks: Vec<std::io::Result<Vec<u8>>> = vec![
            Ok(b"unreadable".to_vec()),
            Err(std::io::Error::other("the file went away")),
        ];
        let read = plane
            .put_streamed(digest.clone(), Box::pin(futures::stream::iter(chunks)))
            .await;
        assert!(
            matches!(&read, Err(CasError::Read(blob, why))
                if *blob == label(&digest) && why.contains("the file went away")),
            "{read:?}"
        );
        assert_eq!(plane.missing(std::slice::from_ref(&digest)).await, [digest]);
    });
}

/// Catches a digest that is not a SHA-256 one handed to the store, or a gone server half
/// taken for stored: `put_streamed` of a hash that is not hex is `Unavailable` and says
/// so, and with the server half's runtime shut down is `Unavailable` and says the server
/// half did not answer, not `Read` or `Corrupt`.
#[test]
fn a_streamed_blob_is_unavailable_for_a_bad_digest_or_a_gone_server_half() {
    let server = server();
    let plane = plane(&server);
    let bad = Digest {
        hash: "not hex".to_owned(),
        size_bytes: 3,
    };
    let refused = server.block_on(plane.put_streamed(bad, parts(&[b"abc"])));
    assert!(
        matches!(&refused, Err(CasError::Unavailable(_, why)) if why == "not a SHA-256 digest"),
        "{refused:?}"
    );
    drop(server);
    let here = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let digest = digest_of(b"abc");
    let gone = here.block_on(plane.put_streamed(digest, parts(&[b"abc"])));
    assert!(
        matches!(&gone, Err(CasError::Unavailable(_, why)) if why.contains("did not answer")),
        "{gone:?}"
    );
}

/// Catches a small blob stored with other bytes than its digest's, or a failed read taken
/// for stored (defects: the digest compare dropped; a chunk's error answered as the
/// store's): `put_chunks` of a small blob in two chunks is `Ok` and held; one whose
/// bytes hash to another digest is `Corrupt` naming the claimed digest and the actual
/// one, and is not held; one whose chunk fails is `Read` with the chunk's error. (The
/// same arms for a blob over `MAX_UNSTAGED_BLOB_BYTES` are in the slow test.)
#[test]
fn put_chunks_gathers_a_small_blob_and_refuses_one_that_is_not_its_digest() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let small = digest_of(b"small blob");
        let stored = plane
            .put_chunks(small.clone(), parts(&[b"small", b" blob"]))
            .await;
        assert_eq!(stored, Ok(small.clone()));
        assert_eq!(plane.get(&small).await, Ok(b"small blob".to_vec()));

        let claimed = digest_of(b"claimed bytes");
        let wrong = plane
            .put_chunks(claimed.clone(), parts(&[b"other bytes!!"]))
            .await;
        assert!(
            matches!(&wrong, Err(CasError::Corrupt(blob, actual))
                if *blob == label(&claimed) && *actual == label(&digest_of(b"other bytes!!"))),
            "{wrong:?}"
        );
        assert_eq!(
            plane.missing(std::slice::from_ref(&claimed)).await,
            [claimed]
        );

        let chunks: Vec<std::io::Result<Vec<u8>>> = vec![
            Ok(b"unread".to_vec()),
            Err(std::io::Error::other("the file went away")),
        ];
        let unread = digest_of(b"unreadable");
        let read = plane
            .put_chunks(unread.clone(), Box::pin(futures::stream::iter(chunks)))
            .await;
        assert!(
            matches!(&read, Err(CasError::Read(blob, why))
                if *blob == label(&unread) && why.contains("the file went away")),
            "{read:?}"
        );
        assert_eq!(plane.missing(std::slice::from_ref(&unread)).await, [unread]);
    });
}

/// Catches a blob over `MAX_UNSTAGED_BLOB_BYTES` gathered in memory, and a small blob
/// reported stored when the server half is gone (defects: the size test dropped, so every
/// blob is gathered; the store's error dropped): a digest of one byte over the limit is
/// handed to the streamed store (its answer for a hash that is not hex, with no chunk
/// read, where a gathered blob would be `Corrupt`); a small blob with the server half's
/// runtime gone is `Unavailable`.
#[test]
fn put_chunks_streams_a_digest_over_the_limit_and_a_small_blob_needs_the_server_half() {
    let server = server();
    let plane = plane(&server);
    let over = Digest {
        hash: "not hex".to_owned(),
        size_bytes: i64::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size"),
    };
    let never: Chunks = Box::pin(futures::stream::once(async {
        panic!("a chunk of a blob over the limit was gathered")
    }));
    let streamed = server.block_on(plane.put_chunks(over, never));
    assert!(
        matches!(&streamed, Err(CasError::Unavailable(_, why)) if why == "not a SHA-256 digest"),
        "{streamed:?}"
    );
    drop(server);
    let here = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let small = digest_of(b"small");
    let gone = here.block_on(plane.put_chunks(small, parts(&[b"small"])));
    assert!(matches!(gone, Err(CasError::Unavailable(..))), "{gone:?}");
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
    let beside = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-all-in-one-unit");
    let dir = kbf_testkit::dir::TestDir::under(beside, "read-to");
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
    let path = dir.join("blob");
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
}
