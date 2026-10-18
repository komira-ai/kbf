//! An upload begun on no tracked connection. In the `segments` test binary, so the
//! reaper's wait on the upload's connection is measured with both of its arms in one
//! instantiation.

use std::time::Duration;

use kbf_proto::worker::BeginRequest;
use kbf_proto::worker::segments_server::Segments as _;
use kbf_storage::SegmentsConfig;
use tonic::Request;

use super::support::{Front, blob, seg};

/// Catches an upload with no connection of its own dropped at once (mutant: the
/// reaper takes a missing connection for a closed one): a `Begin` that reaches the
/// service with no peer address, as from a listener whose streams are not wrapped by
/// `Connections::track`, keeps its upload and its budget until the idle time drops
/// them.
#[tokio::test]
async fn an_upload_on_no_tracked_connection_is_dropped_only_when_idle() {
    let config = SegmentsConfig {
        idle: Duration::from_secs(1),
        ..SegmentsConfig::DEFAULT
    };
    let front = Front::start(config).await;
    let s = seg(&[blob(120, 2000)]);
    front
        .service
        .begin(Request::new(BeginRequest {
            segments: vec![s.upload_segment()],
        }))
        .await
        .expect("begin");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(front.service.open_uploads(), 1, "kept while not idle");
    assert!(front.cache.bytes_admitted() >= s.length);
    front.wait_until_no_uploads().await;
    assert_eq!(front.cache.bytes_admitted(), 0);
}
