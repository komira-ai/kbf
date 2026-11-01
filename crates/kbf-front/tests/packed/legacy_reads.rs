//! `kbf_storage_legacy_read_total` of a storage node behind a proxy given packs: a read the
//! frontend serves from a pack never reaches the node and is not counted; a read the node
//! serves from its own copy is counted once, whichever call carried it; a read that finds
//! nothing is not counted. The node here is a real one (`StorageService` over a cache), and its
//! scrape is the cache's. Each test says the behaviour it asserts.

use std::sync::Arc;

use bytes::Bytes;
use kbf_front::{Cache, CacheStorage, MemoryMetaLog, Owner, Owners, PackSettings, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::MemoryStore;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::{ActionResult, GetActionResultRequest};
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_proto::worker::{ObjectRef, ReadRangeRequest};
use kbf_storage::Storage;
use kbf_storage::legacy::{LegacyReads, Path};
use kbf_storage::service::{Admission, Config, StorageService};
use kbf_types::Digest;
use prost::Message as _;
use tonic::transport::Channel;

use crate::harness;
use crate::support::{Rig, blob, read, update};

/// A real storage node counting its legacy reads, and a frontend with packs in front of it.
struct Farm {
    rig: Rig,
    node: Arc<dyn Storage>,
    node_channel: Channel,
    reads: Arc<LegacyReads>,
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
}

async fn farm() -> Farm {
    let reads = Arc::new(LegacyReads::default());
    let cache = Arc::new(Cache::memory().with_legacy_reads(Arc::clone(&reads)));
    let node: Arc<dyn Storage> = Arc::new(CacheStorage::new(
        Arc::clone(&cache),
        SegmentsConfig::DEFAULT,
    ));
    let config = Config {
        write_bytes_in_flight: 64 << 20,
    };
    let service = StorageService::new(Arc::clone(&node), Admission::Open, config)
        .with_legacy_reads(Arc::clone(&reads));
    let node_channel = harness::serve(service.routes()).await;
    let owners = Owners::single(Owner::new("node", node_channel.clone()));
    let rig = Rig::over(owners, PackSettings::default()).await;
    Farm {
        rig,
        node,
        node_channel,
        reads,
        cache,
    }
}

impl Farm {
    /// The rows of `kbf_storage_legacy_read_total` the node's scrape shows, in the order of
    /// [`Path::ALL`].
    fn scraped(&self) -> Vec<u64> {
        let text = self.cache.render_metrics();
        Path::ALL
            .iter()
            .map(|path| {
                let labels = format!(r#"{{kind="{}",source="{}"}}"#, path.kind(), path.source());
                let prefix = format!("kbf_storage_legacy_read_total{labels} ");
                let row = text.lines().find_map(|line| line.strip_prefix(&prefix));
                row.unwrap_or_else(|| panic!("no row {prefix} in {text}"))
                    .parse()
                    .expect("a count")
            })
            .collect()
    }
}

/// The bytes of a ByteStream Read of `digest` through the frontend.
async fn read_stream(front: &Channel, digest: &Digest) -> Result<Vec<u8>, tonic::Status> {
    let request = ReadRequest {
        resource_name: format!("main/blobs/{}/{}", digest.hash_hex(), digest.size_bytes),
        ..ReadRequest::default()
    };
    let mut messages = ByteStreamClient::new(front.clone())
        .read(request)
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(message) = messages.message().await? {
        out.extend_from_slice(&message.data);
    }
    Ok(out)
}

/// Asserts that blobs a client uploaded through the frontend (packed, so the node never held
/// them) are served from their pack by a ByteStream Read and by a BatchReadBlobs without the
/// node counting a read, and that the node's scrape shows every row at 0 meanwhile.
#[tokio::test]
async fn reads_served_from_a_pack_are_not_counted() {
    let farm = farm().await;
    let (digest, bytes) = blob("a blob a client uploaded");
    assert_eq!(
        update(&farm.rig.front, &[(digest, bytes.clone())]).await,
        [0]
    );
    let missing = farm.node.find_missing(&[digest]).await.expect("asked");
    assert_eq!(missing, [digest], "the node never held it");

    let streamed = read_stream(&farm.rig.front, &digest).await;
    assert_eq!(streamed.expect("read"), bytes.to_vec());
    let batched = read(&farm.rig.front, &[digest]).await;
    assert_eq!(batched, [(0, bytes.to_vec())]);

    assert_eq!(
        farm.rig.packs.metrics().reads.get(),
        2,
        "both came from the pack"
    );
    assert_eq!(farm.reads.total(), 0);
    assert_eq!(farm.scraped(), [0, 0, 0, 0, 0]);
}

/// Asserts that a blob only the node holds (written before packs) is a pack miss that the
/// frontend reads from the node, and that each such read is one count on `cas/bytestream`,
/// whether it came by a ByteStream Read or by a BatchReadBlobs, and that the node's scrape
/// shows the same numbers.
#[tokio::test]
async fn reads_the_node_serves_after_a_pack_miss_are_counted_once_each() {
    let farm = farm().await;
    let (digest, bytes) = blob("a blob from before packs");
    farm.node
        .put(bytes.clone())
        .await
        .expect("stored on the node");
    assert_eq!(farm.reads.total(), 0, "a write is not a read");

    let streamed = read_stream(&farm.rig.front, &digest).await;
    assert_eq!(streamed.expect("read"), bytes.to_vec());
    assert_eq!(farm.reads.get(Path::CasByteStream), 1);

    let batched = read(&farm.rig.front, &[digest]).await;
    assert_eq!(batched, [(0, bytes.to_vec())]);
    assert_eq!(farm.reads.get(Path::CasByteStream), 2);

    assert_eq!(
        farm.rig.packs.metrics().fallbacks.get(),
        2,
        "both missed the pack"
    );
    assert_eq!(farm.reads.total(), 2);
    assert_eq!(farm.scraped(), [2, 0, 0, 0, 0]);
}

/// Asserts that a read nobody can serve, a ByteStream Read of a blob in neither a pack nor the
/// node and a ReadRange of a segment object the node lacks, is an error and is not counted.
#[tokio::test]
async fn a_read_that_finds_nothing_is_not_counted() {
    let farm = farm().await;
    let (digest, _) = blob("a blob nobody holds");

    let streamed = read_stream(&farm.rig.front, &digest).await;
    assert!(streamed.is_err(), "{streamed:?}");

    let range = ReadRangeRequest {
        object: Some(ObjectRef {
            epoch: 1,
            seq: 99,
            ..ObjectRef::default()
        }),
        offset: 0,
        length: 8,
    };
    let answer = SegmentsClient::new(farm.node_channel.clone())
        .read_range(range)
        .await;
    assert!(answer.is_err(), "{answer:?}");

    assert_eq!(farm.reads.total(), 0);
    assert_eq!(farm.scraped(), [0, 0, 0, 0, 0]);
}

/// Asserts that a GetActionResult the node answers with an entry is one count on
/// `ac/action_store` and counts no blob, and that one it has no entry for is not counted.
#[tokio::test]
async fn an_action_cache_lookup_counts_a_hit_and_not_a_miss() {
    let farm = farm().await;
    let action = harness_digest("an action");
    let result = ActionResult {
        exit_code: 3,
        ..ActionResult::default()
    };
    let stored = farm
        .node
        .put(Bytes::from(result.encode_to_vec()))
        .await
        .expect("the result is stored");
    let record = ActionRecord {
        result: stored,
        closure: std::iter::empty().collect(),
    };
    let committed = farm.node.commit_action_record(Role::Daemon, action, record);
    committed.await.expect("the node takes the entry");
    assert_eq!(farm.reads.total(), 0, "a write is not a read");

    let missing = ask(&farm.rig.front, &harness_digest("another action")).await;
    assert!(missing.is_err(), "no entry");
    assert_eq!(farm.reads.total(), 0, "a lookup that missed");

    let hit = ask(&farm.rig.front, &action).await.expect("a hit");
    assert_eq!(hit.exit_code, 3);
    assert_eq!(farm.reads.get(Path::AcActionStore), 1);
    assert_eq!(farm.reads.total(), 1);
    assert_eq!(farm.scraped(), [0, 0, 0, 1, 0]);
}

fn harness_digest(text: &str) -> Digest {
    blob(text).0
}

async fn ask(front: &Channel, action: &Digest) -> Result<ActionResult, tonic::Status> {
    let request = GetActionResultRequest {
        action_digest: Some(kbf_front::digest_to_proto(action)),
        ..GetActionResultRequest::default()
    };
    let mut client = ActionCacheClient::new(front.clone());
    Ok(client.get_action_result(request).await?.into_inner())
}
