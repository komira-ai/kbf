//! The counts of a daemon's pack reads on its heartbeats: a daemon given the counter of its
//! data plane (`--pack-read`) sends the totals so far in every heartbeat, and a daemon given
//! none sends none.

mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_daemon::cas::Cas as _;
use kbf_daemon::{
    FakeRuntime, PackCommit, PackCounter, PackReads, PackWriter, PackedCas, WriteSettings,
};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{Fetch, PackKeys, ReadError, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex, PackIndexError};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{PackReadCounts, PackWriteCounts, daemon_message};
use kbf_state::ManualClock;
use support::memory::MemoryCas;
use support::{Harness, PROMPT};

/// Packs that answer by the first byte of the blob's hash: 1 serves three bytes, 2 has no such
/// blob, 3 has lost its pack, 4 holds other bytes than the digest, and any other fails the call.
struct Scripted;

impl PackReads for Scripted {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        let answer = match digests[0].hash[0] {
            1 => Ok(vec![Fetch::Found(Bytes::from_static(b"abc"))]),
            2 => Ok(vec![Fetch::Missing]),
            3 => Ok(vec![Fetch::Lost]),
            4 => Ok(vec![Fetch::Damaged]),
            _ => Err(ReadError::Index(PackIndexError::NotMigrated)),
        };
        Box::pin(std::future::ready(answer))
    }
}

/// A digest of three bytes whose hash starts with the byte `first`.
fn digest(first: u8) -> Digest {
    Digest {
        hash: format!("{first:02x}{}", "00".repeat(31)),
        size_bytes: 3,
    }
}

/// One read of the blob `first` names, whatever the answer.
async fn read(cas: &PackedCas<MemoryCas>, first: u8) {
    drop(cas.get(&digest(first)).await);
}

/// Asserts that a daemon given the counter of its data plane puts the totals as they stand in
/// each heartbeat it sends: the first heartbeat carries the reads made before it, each count
/// from the read that adds to it (a served blob and its bytes, a blob in no pack, packs that
/// could not be asked, a lost pack, a damaged one), and a heartbeat sent after one more read
/// carries that read too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_heartbeat_carries_the_pack_reads_so_far() {
    let counter = PackCounter::default();
    let underneath = Arc::new(MemoryCas::default());
    let cas = PackedCas::with_counter(underneath, Arc::new(Scripted), &counter);
    for first in [1, 1, 2, 3, 3, 4, 4, 4, 5, 5, 5, 5] {
        read(&cas, first).await;
    }
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_pack_reads("pack-heartbeat", runtime, counter).await;
    let mut peer = h.welcomed().await;

    let first = peer.heartbeat().await;
    let want = PackReadCounts {
        served: 2,
        served_bytes: 6,
        elsewhere: 1,
        unasked: 4,
        lost: 2,
        damaged: 3,
    };
    assert_eq!(first.pack_reads, Some(want));
    assert_eq!(first.pack_writes, None);

    read(&cas, 1).await;
    let (_, later) = peer
        .expect(PROMPT, |m| match m {
            daemon_message::Message::Heartbeat(beat) => beat
                .pack_reads
                .as_ref()
                .filter(|counts| counts.served == 3)
                .map(|_| beat.clone()),
            _ => None,
        })
        .await
        .expect("a heartbeat after the read");
    let counts = later.pack_reads.expect("counts");
    assert_eq!((counts.served, counts.served_bytes), (3, 9));
    assert_eq!(
        (
            counts.elsewhere,
            counts.unasked,
            counts.lost,
            counts.damaged
        ),
        (1, 4, 2, 3)
    );
}

/// Asserts that a daemon given no counter, as one without `--pack-read` is, sends no counts in
/// any heartbeat: neither field is present in the first or in the next.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_reads_no_packs_sends_no_counts() {
    let off = Duration::from_secs(60);
    let mut h = Harness::start("pack-heartbeat-off", Duration::ZERO, off).await;
    let mut peer = h.welcomed().await;
    let first = peer.heartbeat().await;
    let next = peer.heartbeat().await;
    assert_eq!((first.seq, next.seq), (1, 2));
    assert_eq!(first.pack_reads, None);
    assert_eq!(next.pack_reads, None);
    assert_eq!(first.pack_writes, None);
    assert_eq!(next.pack_writes, None);
}

/// Packs that hold no blob.
struct Nothing;

impl PackReads for Nothing {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        let missing = digests.iter().map(|_| Fetch::Missing).collect();
        Box::pin(std::future::ready(Ok(missing)))
    }
}

/// A plane that writes packs to a memory store, counting into `counter`, and writing while
/// `commit` is on.
fn writing(counter: &PackCounter, commit: &PackCommit) -> PackedCas<MemoryCas> {
    let caps = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let store = Arc::new(MemoryStore::new(caps));
    let index: Arc<dyn PackIndex> =
        Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let keys = PackKeys::new(WriterKind::Daemon, "node1", "boot1").expect("keys");
    let writer = PackWriter::new(store, index, keys, WriteSettings::default());
    let underneath = Arc::new(MemoryCas::default());
    PackedCas::with_counter(underneath, Arc::new(Nothing), counter)
        .with_writes(Arc::new(writer.expect("writer")), commit.clone())
}

/// Asserts that a daemon that reads and writes packs puts the write totals as they stand in each
/// heartbeat: the first carries the two packs written before it (one blob each, no fallback), and
/// a heartbeat sent after one more put, which the frontend's `Welcome` (it does not take commits)
/// turned off, carries that blob as a fallback of the capability being off, the packs kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_heartbeat_carries_the_pack_writes_so_far() {
    let counter = PackCounter::default();
    let commit = PackCommit::new();
    commit.welcomed(true);
    let cas = writing(&counter, &commit);
    for bytes in [&b"one"[..], &b"two"[..]] {
        cas.put(bytes.to_vec()).await.expect("put");
    }
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h =
        Harness::with_pack_counts("pack-write-heartbeat", runtime, counter.clone(), commit).await;
    let mut peer = h.welcomed().await;

    let first = peer.heartbeat().await;
    let written = first.pack_writes.expect("write counts");
    assert_eq!((written.packs, written.packed), (2, 2));
    assert!(written.packed_bytes > 0, "{written:?}");
    let fallbacks = [
        written.fallback_off,
        written.fallback_unfit,
        written.fallback_unsupported,
        written.fallback_refused,
        written.fallback_store,
        written.fallback_record,
        written.fallback_index,
    ];
    assert_eq!(fallbacks, [0; 7]);
    assert_eq!(written, PackWriteCounts::from(counter.stats().writes));
    assert_eq!(first.pack_reads, Some(PackReadCounts::default()));

    cas.put(b"three".to_vec()).await.expect("put");
    let (_, later) = peer
        .expect(PROMPT, |m| match m {
            daemon_message::Message::Heartbeat(beat) => beat
                .pack_writes
                .as_ref()
                .filter(|counts| counts.fallback_off == 1)
                .map(|_| beat.clone()),
            _ => None,
        })
        .await
        .expect("a heartbeat after the put");
    let counts = later.pack_writes.expect("counts");
    assert_eq!((counts.packs, counts.packed), (2, 2));
    assert_eq!(counts.packed_bytes, written.packed_bytes);
}

/// Asserts that a daemon that reads packs and was not told to write them sends read counts and
/// no write counts: the write field is absent, where the read field is present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_writes_no_packs_sends_no_write_counts() {
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let counter = PackCounter::default();
    let mut h = Harness::with_pack_reads("pack-heartbeat-reads", runtime, counter).await;
    let mut peer = h.welcomed().await;
    let first = peer.heartbeat().await;
    assert_eq!(first.pack_reads, Some(PackReadCounts::default()));
    assert_eq!(first.pack_writes, None);
}
