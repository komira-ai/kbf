//! A `Segments` upload the cache has no write-tier room for, read from the process gauge
//! (`kbf_bytes_in_flight`) while its PUT is held at the object store: the segment is hashed
//! and rebuilt (`Cache::verify_built`), and the copy it PUTs is counted until the PUT is
//! done with it, not only the body it was made from.
//!
//! A test binary of its own, with one test: the in-memory object store keeps the `Bytes`
//! it is given, so the count is not 0 after the PUT, and no other test may share the
//! process.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Connections, SegmentsConfig, SegmentsService};
use kbf_objstore::Capabilities;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{BeginRequest, CommitRequest, PutRangeRequest, UploadSegment};
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry, SegmentWriter};
use kbf_storage::in_flight::process;
use support::{Farm, blobs};
use tonic::Request;

/// Catches: the copy `verify_built` makes of a segment the tier has no room for (records,
/// then a footer rebuilt from the hashing pass) counted not at all (the body it replaces
/// is dropped as `verify_built` returns, and the PUT then holds bytes the gauge does not
/// know), or counted by a guard that is gone before `store_segment` has PUT it. The PUT is
/// held at the store: while it is held the segment is counted at its length, once.
///
/// Mutant: the body `verify_built` returns made with `Bytes::from` (the count is 0 while
/// the PUT is held).
#[tokio::test]
async fn a_segment_with_no_tier_room_is_counted_until_its_put_is_done_with_it() {
    let farm = Farm::new(Capabilities::default());
    farm.store.hold_puts();
    // A tier of 1000 bytes: no segment fits, so every one is hashed and PUT directly.
    let cache = Arc::new(farm.start(1000).await);
    let service = Arc::new(SegmentsService::new(
        Arc::clone(&cache),
        SegmentsConfig::DEFAULT,
        Connections::new(),
    ));
    let all = blobs(1, 2, 4096);
    let mut writer = SegmentWriter::new(u64::MAX);
    for b in &all {
        writer.push(&b.bytes).expect("push");
    }
    let bytes = writer.finish();
    let parsed = Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(parsed.records_len()).expect("len");
    let entries: Vec<IndexEntry> = parsed.entries().to_vec();
    let footer = encode_footer(parsed.records_len(), &entries);
    let length = (records_len + footer.len()) as u64;

    let begin = BeginRequest {
        segments: vec![UploadSegment {
            length,
            footer: footer.clone(),
        }],
    };
    let upload = service
        .begin(Request::new(begin))
        .await
        .expect("begin")
        .into_inner()
        .upload;
    let put = PutRangeRequest {
        upload,
        segment: 0,
        offset: 0,
        bytes: bytes[..records_len].to_vec(),
    };
    service.put_range(Request::new(put)).await.expect("range");
    let committing = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.commit(Request::new(CommitRequest { upload })).await })
    };

    // The PUT arrives at the store and waits there.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while farm.store.puts_in_flight() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "the PUT never came");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        process().current(),
        length,
        "the segment being PUT is counted at its length, once"
    );
    assert!(process().peak() >= length);

    farm.store.release_puts();
    committing
        .await
        .expect("the commit task")
        .expect("committed");
}
