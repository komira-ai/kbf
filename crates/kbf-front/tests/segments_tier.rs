//! `Segments` uploads on the local write tier (`docs/design/write-path.md`, sections
//! 4.2 and 6.1): a commit answered once every segment is synced on the tier and its
//! `PutStaged` committed, blobs read from the tier until flushed, and the direct PUT
//! when the tier does not take a segment.
//!
//! The service is called in process over the same [`Farm`] `write_tier.rs` runs: one
//! metadata log that outlives restarts, a tier directory on a `FaultFs`, and a store
//! whose PUTs a test can hold ("the object store is down"). Each test's comment names
//! the defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Connections, SegmentsConfig, SegmentsService};
use kbf_objstore::Capabilities;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, ObjectRef, PutRangeRequest, ReadRangeRequest, UploadSegment,
};
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry, SegmentWriter};
use kbf_types::Digest;
use support::{Farm, TestBlob, TierCache, blobs, later, start};
use tonic::{Request, Status};

use crate::common::faulty::FaultyStore;
use support::Log;

const GIB: u64 = 1 << 30;

type Service = SegmentsService<Log, FaultyStore>;

fn serve(cache: TierCache) -> (Arc<TierCache>, Service) {
    let cache = Arc::new(cache);
    let service = SegmentsService::new(
        Arc::clone(&cache),
        SegmentsConfig::DEFAULT,
        Connections::new(),
    );
    (cache, service)
}

/// A segment as a daemon sends it: its records, and the footer that indexes them.
struct Seg {
    records: Vec<u8>,
    footer: Vec<u8>,
    length: u64,
}

fn seg(blobs: &[TestBlob]) -> Seg {
    let mut writer = SegmentWriter::new(u64::MAX);
    for b in blobs {
        writer.push(&b.bytes).expect("push");
    }
    let bytes = writer.finish();
    let parsed = Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(parsed.records_len()).expect("len");
    let entries: Vec<IndexEntry> = parsed.entries().to_vec();
    let footer = encode_footer(parsed.records_len(), &entries);
    Seg {
        records: bytes[..records_len].to_vec(),
        length: (records_len + footer.len()) as u64,
        footer,
    }
}

/// Begin, PutRange and Commit of `segs`; the Commit gives up after ten seconds, so a
/// commit stuck on a downed store is a failed assertion, not a hung test.
async fn upload(service: &Service, segs: &[Seg]) -> Result<Vec<String>, Status> {
    let begin = BeginRequest {
        segments: segs
            .iter()
            .map(|s| UploadSegment {
                length: s.length,
                footer: s.footer.clone(),
            })
            .collect(),
    };
    let upload = service
        .begin(Request::new(begin))
        .await?
        .into_inner()
        .upload;
    for (i, s) in segs.iter().enumerate() {
        let put = PutRangeRequest {
            upload,
            segment: u32::try_from(i).expect("segment"),
            offset: 0,
            bytes: s.records.clone(),
        };
        service.put_range(Request::new(put)).await?;
    }
    let commit = service.commit(Request::new(CommitRequest { upload }));
    let answered = tokio::time::timeout(Duration::from_secs(10), commit).await;
    let committed = answered.expect("the commit answered while the store is down")?;
    Ok(committed
        .into_inner()
        .committed
        .into_iter()
        .map(|d| d.hash)
        .collect())
}

/// Every blob of `blobs` is present, and read back whole: by `FindMissing`, by the
/// batch read, and as the daemon does, from `Locate` and `ReadRange`.
async fn assert_readable(cache: &TierCache, service: &Service, blobs: &[TestBlob]) {
    let digests: Vec<Digest> = blobs.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), []);
    let read = cache.read_blobs(&digests).await.expect("batch");
    let located = service
        .locate(Request::new(kbf_proto::worker::LocateRequest {
            digests: digests.iter().map(kbf_front::digest_to_proto).collect(),
        }))
        .await
        .expect("locate")
        .into_inner()
        .locations;
    for (b, at) in blobs.iter().zip(located) {
        assert_eq!(read[&b.digest].as_ref().expect("batch read"), &b.bytes);
        let ObjectRef { epoch, seq } = at.object.expect("located");
        let range = ReadRangeRequest {
            object: Some(ObjectRef { epoch, seq }),
            offset: at.offset,
            length: b.bytes.len() as u64,
        };
        let bytes = service
            .read_range(Request::new(range))
            .await
            .expect("read range")
            .into_inner()
            .bytes;
        assert_eq!(bytes, b.bytes.as_ref());
    }
}

fn counter(cache: &TierCache, name: &str) -> String {
    let metrics = cache.render_metrics();
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| panic!("no {name} in {metrics}"))
        .to_owned()
}

/// Catches: a `Segments` commit that PUTs while a tier is attached (the cold-upload
/// wait on the object store), and blobs staged for a commit that cannot be found or
/// read until the store takes them. With every PUT held, three segments commit, none
/// reaches the store, three objects are staged, and every blob is present and read
/// back by every read path.
#[tokio::test]
async fn an_upload_commits_on_the_tier_while_the_store_is_down() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 6, 4096);
    let segs: Vec<Seg> = all.chunks(2).map(seg).collect();
    let committed = upload(&service, &segs).await.expect("committed");
    let mut want: Vec<_> = all.iter().map(|b| b.digest.hash_hex()).collect();
    let mut got = committed;
    want.sort();
    got.sort();
    assert_eq!(got, want);
    assert!(farm.store.put_keys().is_empty(), "no PUT");
    assert_eq!(farm.store.puts_in_flight(), 0);
    assert_eq!(counter(&cache, "kbf_write_tier_staged_total"), "3");
    for b in &all {
        assert_eq!(farm.log.located(&b.digest).1, cache.tier_id());
    }
    assert_readable(&cache, &service, &all).await;
    assert!(farm.store.put_keys().is_empty(), "reads stayed local");
}

/// Catches: staged upload objects the flusher does not PUT (so the store never gets
/// them), `ObjectFlushed` committed before the PUT, and reads that fail once flushed.
#[tokio::test]
async fn once_the_store_recovers_the_flusher_puts_the_uploaded_objects() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 4, 4096);
    let segs: Vec<Seg> = all.chunks(2).map(seg).collect();
    upload(&service, &segs).await.expect("committed");
    let objects: Vec<_> = all.iter().map(|b| farm.log.object_of(&b.digest)).collect();
    assert_eq!(farm.log.applied("object_flushed"), 0);
    farm.store.release_puts();
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "{round:?}");
    assert_eq!(farm.log.applied("object_flushed"), 2);
    assert_eq!(farm.store.put_keys().len(), 2);
    for object in &objects {
        assert!(
            farm.store
                .stored(&support::key(&cache, *object))
                .await
                .is_some()
        );
    }
    assert_readable(&cache, &service, &all).await;
}

/// Catches: a full tier that fails the upload, or loses a segment; and a fallback that
/// stops counting. At the budget the segments past it are PUT as before and indexed
/// by `PutBlobs`, the commit succeeds, and every blob reads back.
#[tokio::test]
async fn a_tier_at_its_budget_falls_back_to_the_direct_put() {
    let farm = Farm::new(Capabilities::default());
    let (cache, service) = serve(farm.start(10_000).await);
    let all = blobs(1, 3, 4096);
    let segs: Vec<Seg> = all.chunks(1).map(seg).collect();
    let committed = upload(&service, &segs).await.expect("committed");
    assert_eq!(committed.len(), 3);
    assert_eq!(counter(&cache, "kbf_write_tier_staged_total"), "2");
    assert_eq!(counter(&cache, "kbf_write_tier_full_total"), "1");
    assert_eq!(
        farm.store.put_keys().len(),
        1,
        "one segment past the budget"
    );
    assert_eq!(farm.log.applied("put_blobs"), 1);
    assert_readable(&cache, &service, &all).await;
}

/// Catches: an acknowledged upload that a restart loses (the commit answered before
/// the block was synced, or the index not read back at the next start), and a block
/// that is not flushed after the restart. The store is down for the commit; the disk
/// loses what was not synced.
#[tokio::test]
async fn a_committed_upload_survives_a_restart_and_is_flushed_after_it() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    let (cache, service) = serve(farm.start(GIB).await);
    let all = blobs(1, 4, 4096);
    let segs: Vec<Seg> = all.chunks(2).map(seg).collect();
    upload(&service, &segs).await.expect("committed");
    drop(service);
    drop(cache);

    let cut = farm.fs.crash(0);
    let (cache, startup) = start(&cut, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert!(startup.recovery.lost.is_empty(), "{startup:?}");
    let (cache, service) = serve(cache);
    assert!(farm.store.put_keys().is_empty());
    assert_readable(&cache, &service, &all).await;
    farm.store.release_puts();
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2, "{round:?}");
    assert_readable(&cache, &service, &all).await;
}
