//! Segment upload and segment-range reads against kbf-front's `Segments` service
//! (`docs/design/write-path.md`, sections 4 and 9, "Segment upload (PR 2)"), counted
//! by a wrapper that can also fail Commits.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_front::{Cache, Connections, MemoryMetaLog, SegmentsConfig, SegmentsService};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::segments_server::{Segments, SegmentsServer};
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_transfer::{Cas, MAX_RANGE_BYTES, Spool, TransferConfig, Transfers};
use tonic::{Request, Response, Status};

use super::fake::serve;
use super::{engine, patterned};

type Service = SegmentsService<MemoryMetaLog, MemoryStore>;

/// How the test's `Segments` answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// As the front serves it.
    Serve,
    /// Every Commit fails UNAVAILABLE.
    FailCommits,
    /// Every Begin is UNIMPLEMENTED, as a server without the service answers.
    Unimplemented,
    /// Every Locate fails UNAVAILABLE.
    FailLocate,
    /// Every ReadRange fails UNAVAILABLE.
    FailReadRange,
    /// Every ReadRange answers its bytes with the first one flipped.
    CorruptReadRange,
}

/// `Segments` over the front's, counting calls, answering as `mode` says.
struct Counting {
    inner: Service,
    begins: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    read_ranges: Arc<AtomicUsize>,
    /// Records declared by every `Begin`'s footers.
    declared: Arc<AtomicUsize>,
    mode: Mode,
}

#[tonic::async_trait]
impl Segments for Counting {
    async fn begin(&self, r: Request<BeginRequest>) -> Result<Response<BeginResponse>, Status> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        let records: usize = r
            .get_ref()
            .segments
            .iter()
            .map(|s| {
                kbf_segments::Footer::parse(&s.footer, s.length).map_or(0, |f| f.entries().len())
            })
            .sum();
        self.declared.fetch_add(records, Ordering::SeqCst);
        if self.mode == Mode::Unimplemented {
            return Err(Status::unimplemented("no Segments here"));
        }
        self.inner.begin(r).await
    }

    async fn put_range(
        &self,
        r: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        self.inner.put_range(r).await
    }

    async fn commit(&self, r: Request<CommitRequest>) -> Result<Response<CommitResponse>, Status> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        if self.mode == Mode::FailCommits {
            return Err(Status::unavailable("the store is down"));
        }
        self.inner.commit(r).await
    }

    async fn locate(&self, r: Request<LocateRequest>) -> Result<Response<LocateResponse>, Status> {
        if self.mode == Mode::FailLocate {
            return Err(Status::unavailable("the index is down"));
        }
        self.inner.locate(r).await
    }

    async fn read_range(
        &self,
        r: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        self.read_ranges.fetch_add(1, Ordering::SeqCst);
        if self.mode == Mode::FailReadRange {
            return Err(Status::unavailable("the store is down"));
        }
        let mut answer = self.inner.read_range(r).await?;
        if self.mode == Mode::CorruptReadRange {
            answer.get_mut().bytes[0] ^= 0xff;
        }
        Ok(answer)
    }
}

/// A front with the CAS services and `Segments` answering as `mode` says.
struct Front {
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    engine: Arc<Transfers>,
    begins: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    read_ranges: Arc<AtomicUsize>,
    declared: Arc<AtomicUsize>,
}

impl Front {
    async fn start(mode: Mode) -> Self {
        let cache = Arc::new(Cache::memory());
        let (begins, commits, read_ranges, declared) = Default::default();
        let mut routes = kbf_front::routes(Arc::clone(&cache));
        {
            let counting = Counting {
                inner: SegmentsService::new(
                    Arc::clone(&cache),
                    SegmentsConfig::DEFAULT,
                    Connections::new(),
                ),
                begins: Arc::clone(&begins),
                commits: Arc::clone(&commits),
                read_ranges: Arc::clone(&read_ranges),
                declared: Arc::clone(&declared),
                mode,
            };
            routes = routes.add_service(
                SegmentsServer::new(counting)
                    .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(kbf_front::MAX_MESSAGE_BYTES),
            );
        }
        let addr = serve(routes).await;
        Self {
            cache,
            engine: engine(addr, TransferConfig::default()),
            begins,
            commits,
            read_ranges,
            declared,
        }
    }

    /// Whether every one of `digests` is present.
    async fn all_present(&self, digests: &[Digest]) -> bool {
        let digests: Vec<kbf_types::Digest> = digests
            .iter()
            .map(|d| kbf_front::digest_from_proto(Some(d)).expect("digest"))
            .collect();
        let present = self.cache.present(&digests).await.expect("present");
        present.iter().all(|&p| p)
    }
}

/// A scratch directory of this test binary's own.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-transfer-segments")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// A spool of `blobs`, and their digests.
async fn spooled(
    name: &str,
    blobs: impl IntoIterator<Item = Vec<u8>>,
) -> (Arc<Spool>, Vec<Digest>) {
    let spool = Arc::new(Spool::create(&scratch(name)).expect("spool"));
    let mut digests = Vec::new();
    for blob in blobs {
        digests.push(spool.put(blob).await.expect("spool put"));
    }
    (spool, digests)
}

/// Catches per-blob calls and per-segment commits (mutant: one upload per segment):
/// 15,000 small outputs go as one `Begin`, one `Commit` and one `PutBlobs`, and every
/// one is present after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifteen_thousand_outputs_are_one_upload_and_one_commit() {
    let front = Front::start(Mode::Serve).await;
    let blobs = (0..15_000).map(|i| format!("output file {i}\n").into_bytes());
    let (spool, digests) = spooled("fifteen-thousand", blobs).await;
    front.engine.put_spool(spool).await.expect("put spool");
    assert_eq!(front.begins.load(Ordering::SeqCst), 1);
    assert_eq!(front.commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        front.cache.metrics().segment_commit.count(),
        1,
        "one PutBlobs"
    );
    assert_eq!(
        front.cache.metrics().write.total.get(),
        0,
        "no ByteStream write"
    );
    assert!(front.all_present(&digests).await);
}

/// Catches blobs the server holds sent again (mutant: the FindMissing answer ignored;
/// design step 4.1.1): a spool of 20 blobs, 10 of them uploaded by an earlier spool,
/// declares only the 10 new ones in its `Begin`, and all 20 are present after.
#[tokio::test]
async fn blobs_the_server_holds_are_not_sent_again() {
    let front = Front::start(Mode::Serve).await;
    let (spool, held) = spooled("held-first", (0..10).map(|i| patterned(i, 500))).await;
    front.engine.put_spool(spool).await.expect("first upload");
    assert_eq!(front.declared.load(Ordering::SeqCst), 10);
    let (spool, all) = spooled("held-second", (0..20).map(|i| patterned(i, 500))).await;
    assert_eq!(all[..10], held[..]);
    front.engine.put_spool(spool).await.expect("second upload");
    assert_eq!(
        front.declared.load(Ordering::SeqCst),
        20,
        "the second upload declares only the 10 new blobs"
    );
    assert_eq!(front.begins.load(Ordering::SeqCst), 2);
    assert_eq!(
        front.cache.metrics().write.total.get(),
        0,
        "no ByteStream write"
    );
    assert!(front.all_present(&all).await);
}

/// Catches a new daemon failing against an old server (mutant: UNIMPLEMENTED is an
/// error): a front without `Segments` gets every blob by ByteStream (a large one
/// streamed from the spool), and the next spool does not try `Segments` again.
#[tokio::test]
async fn a_server_without_segments_gets_every_blob_by_bytestream() {
    let front = Front::start(Mode::Unimplemented).await;
    // One blob over `--transfer-memory-blob-bytes`, streamed from the spool.
    let blobs = (0..20)
        .map(|i| patterned(i, 100))
        .chain([patterned(99, 3 << 20)]);
    let (spool, digests) = spooled("unimplemented", blobs).await;
    front.engine.put_spool(spool).await.expect("fall back");
    assert!(front.all_present(&digests).await);
    assert_eq!(front.cache.metrics().write.total.get(), 21);
    let (spool, more) = spooled("unimplemented-2", (20..25).map(|i| patterned(i, 100))).await;
    front
        .engine
        .put_spool(spool)
        .await
        .expect("fall back again");
    assert!(front.all_present(&more).await);
    assert_eq!(
        front.begins.load(Ordering::SeqCst),
        1,
        "Segments asked once"
    );
}

/// Catches a failed upload given up on, or retried forever (mutants: no retry; no
/// fallback): a Commit that always fails is tried twice, then every blob goes by
/// ByteStream and is present.
#[tokio::test]
async fn a_failing_commit_is_retried_once_then_blobs_go_one_by_one() {
    let front = Front::start(Mode::FailCommits).await;
    let (spool, digests) = spooled("failing-commit", (0..10).map(|i| patterned(i, 1000))).await;
    front.engine.put_spool(spool).await.expect("fall back");
    assert_eq!(
        front.begins.load(Ordering::SeqCst),
        2,
        "the upload and one retry"
    );
    assert_eq!(front.commits.load(Ordering::SeqCst), 2);
    assert!(front.all_present(&digests).await);
    assert_eq!(front.cache.metrics().write.total.get(), 10);
}

/// Catches fills still one call per blob (mutant: no coalescing): the 1,000 files of a
/// tree, uploaded as segments, read back through `Locate` and `ReadRange` in at most
/// ceil(bytes / 4 MiB) reads, every blob checked; a blob never uploaded is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_of_a_thousand_files_reads_back_in_few_ranges() {
    let front = Front::start(Mode::Serve).await;
    let blobs: Vec<Vec<u8>> = (0..1000)
        .map(|i| patterned(i, 2000 + (i * 37) % 9000))
        .collect();
    let total: u64 = blobs.iter().map(|b| b.len() as u64).sum();
    let (spool, digests) = spooled("tree", blobs.clone()).await;
    front.engine.put_spool(spool).await.expect("upload");
    let absent = kbf_transfer::digest_of(b"never uploaded");
    let mut asked = digests.clone();
    asked.push(absent.clone());
    let located = front
        .engine
        .scope(&kbf_types::LeaseId::new(1, 1))
        .read_located(&asked)
        .await;
    let located = located.expect("read");
    assert_eq!(located.missing, vec![absent]);
    assert_eq!(located.blobs.len(), 1000);
    for (digest, bytes) in &located.blobs {
        let i = digests.iter().position(|d| d == digest).expect("asked");
        assert_eq!(bytes, &blobs[i]);
    }
    let reads = front.read_ranges.load(Ordering::SeqCst) as u64;
    assert!(
        reads <= total.div_ceil(MAX_RANGE_BYTES),
        "{reads} reads for {total} bytes"
    );
}

/// Catches a fill that fails, or hangs, when the server cannot locate or read (mutants:
/// a failed `Locate` or `ReadRange` returned as an error): every blob asked for comes
/// back missing, so the caller fetches it another way. Also catches bytes from a
/// range trusted unchecked (mutant: the per-blob digest check dropped): a range whose
/// bytes were altered fails the read as corrupt.
#[tokio::test]
async fn reads_by_range_fall_back_to_missing_and_refuse_altered_bytes() {
    for mode in [
        Mode::FailLocate,
        Mode::FailReadRange,
        Mode::CorruptReadRange,
    ] {
        let front = Front::start(mode).await;
        let (spool, digests) = spooled("fill", (0..5).map(|i| patterned(i, 700))).await;
        front.engine.put_spool(spool).await.expect("upload");
        let read = front
            .engine
            .scope(&kbf_types::LeaseId::new(1, 1))
            .read_located(&digests)
            .await;
        if mode == Mode::CorruptReadRange {
            let failed = read.expect_err("altered bytes");
            assert!(format!("{failed:?}").starts_with("Corrupt("), "{failed:?}");
            continue;
        }
        let read = read.expect("missing, not an error");
        assert!(read.blobs.is_empty());
        let mut missing = read.missing;
        missing.sort_by(|a, b| a.hash.cmp(&b.hash));
        let mut want = digests.clone();
        want.sort_by(|a, b| a.hash.cmp(&b.hash));
        assert_eq!(missing, want);
    }
}

/// Catches a spool that cannot be read treated as a failed call (mutant: the spool
/// error retried and then sent blob by blob, each read failing again): when the spool
/// file is cut short after its blobs were spooled, `put_spool` fails with a read
/// error after one `Begin` and no `Commit`. Linux only: the unlinked spool file is
/// reached through `/proc/self/fd`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_spool_cut_short_fails_the_upload_without_a_retry() {
    let front = Front::start(Mode::Serve).await;
    let (spool, _) = spooled("cut-short", (0..3).map(|i| patterned(i, 900))).await;
    let marker = format!(".kbf-spool-{}-", std::process::id());
    let dir = scratch("cut-short");
    let mut cut = 0;
    for entry in std::fs::read_dir("/proc/self/fd").expect("fds") {
        let path = entry.expect("fd").path();
        let target = std::fs::read_link(&path).unwrap_or_default();
        let name = target.to_string_lossy().into_owned();
        if name.starts_with(dir.to_string_lossy().as_ref()) && name.contains(&marker) {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("reopen the spool");
            file.set_len(0).expect("cut");
            cut += 1;
        }
    }
    assert_eq!(cut, 1, "one spool file under the scratch directory");
    let failed = front
        .engine
        .put_spool(spool)
        .await
        .expect_err("the spool cannot be read");
    assert!(format!("{failed:?}").starts_with("Read("), "{failed:?}");
    assert_eq!(front.begins.load(Ordering::SeqCst), 1, "not retried");
    assert_eq!(front.commits.load(Ordering::SeqCst), 0);
}
