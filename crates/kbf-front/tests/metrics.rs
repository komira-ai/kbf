//! The cache's upload-path metrics (`kbf_front::metrics`), over gRPC against an
//! in-process cache whose store and metadata log take a known time per call: each
//! family moves when its operation runs, by the amount it ran, and no other.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use common::Blob;
use futures::SinkExt as _;
use futures::channel::mpsc;
use kbf_front::metrics::{CacheMetrics, Histogram};
use kbf_front::{Cache, MAX_MESSAGE_BYTES, MemoryMetaLog, MetaLog, MetaLogError};
use kbf_meta::{Applied, Command, MetaState, Retention};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{BatchUpdateBlobsRequest, FindMissingBlobsRequest};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

/// How long the store takes per object PUT. Ten times a footer read, so a timer
/// around the wrong stage reads far off the mark either way.
const PUT: Duration = Duration::from_millis(400);
/// How long the store takes per range GET. A footer read is two of them.
const GET: Duration = Duration::from_millis(20);
/// How long the metadata log takes to commit `PutBlobs`. Other commands are at once.
const PUT_BLOBS: Duration = Duration::from_millis(150);

/// A [`MemoryStore`] whose PUTs take [`PUT`] and range GETs [`GET`].
struct SlowStore(MemoryStore);

impl ObjectStore for SlowStore {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        tokio::time::sleep(PUT).await;
        self.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        tokio::time::sleep(GET).await;
        self.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

/// A [`MemoryMetaLog`] whose `PutBlobs` commits take [`PUT_BLOBS`].
struct SlowLog(MemoryMetaLog);

impl MetaLog for SlowLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        if matches!(command, Command::PutBlobs(_)) {
            tokio::time::sleep(PUT_BLOBS).await;
        }
        self.0.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.0.query(f).await
    }
}

type SlowCache = Cache<SlowLog, SlowStore>;

/// The cache services over a slow cache on a loopback port, and a channel to them.
async fn serve() -> (Arc<SlowCache>, Channel) {
    let cache = Arc::new(
        Cache::open(
            SlowLog(MemoryMetaLog::new(Retention::default())),
            SlowStore(MemoryStore::new(Capabilities::default())),
            KeyPrefix::default(),
        )
        .await
        .expect("open the cache"),
    );
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("local address");
    let routes = kbf_front::routes(Arc::clone(&cache));
    tokio::spawn(async move {
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    (cache, channel)
}

fn bytestream(channel: &Channel) -> ByteStreamClient<Channel> {
    ByteStreamClient::new(channel.clone())
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES)
}

fn cas(channel: &Channel) -> ContentAddressableStorageClient<Channel> {
    ContentAddressableStorageClient::new(channel.clone())
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES)
}

fn write_name(blob: &Blob) -> String {
    format!(
        "uploads/metrics-test/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    )
}

/// `blob` in one ByteStream Write of two messages.
async fn write(channel: &Channel, blob: &Blob) {
    let half = blob.data.len() / 2;
    let messages = vec![
        WriteRequest {
            resource_name: write_name(blob),
            write_offset: 0,
            finish_write: false,
            data: blob.data[..half].to_vec(),
        },
        WriteRequest {
            resource_name: String::new(),
            write_offset: half as i64,
            finish_write: true,
            data: blob.data[half..].to_vec(),
        },
    ];
    let response = bytestream(channel)
        .write(futures::stream::iter(messages))
        .await
        .expect("Write")
        .into_inner();
    assert_eq!(response.committed_size, blob.proto.size_bytes);
}

/// A histogram's count and sum (as a duration, for a latency).
fn seen(h: &Histogram) -> (u64, Duration) {
    (h.count(), Duration::from_nanos(h.sum()))
}

/// The `kbf_meta_commit_seconds` count of `command`.
fn commits(metrics: &CacheMetrics, command: &str) -> u64 {
    let at = kbf_front::metrics::COMMANDS
        .iter()
        .position(|c| *c == command)
        .expect("a command label");
    metrics.commit[at].count()
}

/// Catches a stage of the upload path that is not timed, or timed around the wrong
/// call: the object PUT, the footer read-back (two GETs) and the PutBlobs commit each
/// take a known time here, ten-fold apart, and each histogram must hold one
/// observation of at least its stage's time and less than the PUT's (but the PUT's
/// own). Also the write counted without its bytes (counted per message), its
/// end-to-end time shorter than the stages inside it, a segment's blob count or size
/// not recorded, footer GETs counted as reads, and the in-flight level left raised.
/// Mutants seen red: each stage timer removed; the footer timer moved around the PUT.
#[tokio::test]
async fn a_bytestream_write_times_each_stage_of_its_store() {
    let (cache, channel) = serve().await;
    let blob = Blob::new(vec![7_u8; 10_000]);
    write(&channel, &blob).await;
    let m = cache.metrics();

    let (puts, put) = seen(&m.segment_put);
    let (footers, footer) = seen(&m.segment_footer);
    let (segment_commits, commit) = seen(&m.segment_commit);
    assert_eq!((puts, footers, segment_commits), (1, 1, 1));
    assert!(put >= PUT, "segment put {put:?}");
    assert!(footer >= 2 * GET && footer < PUT, "footer read {footer:?}");
    assert!(
        commit >= PUT_BLOBS && commit < PUT,
        "segment commit {commit:?}"
    );
    let (stores, store) = seen(&m.store_blobs);
    assert_eq!(stores, 1);
    assert!(store >= put + footer + commit, "store {store:?}");
    let (writes, write_time) = seen(&m.write.seconds);
    assert_eq!(writes, 1);
    assert!(
        write_time >= store,
        "write {write_time:?} < store {store:?}"
    );
    assert_eq!(m.write.total.get(), 1);
    assert_eq!(m.write.errors.get(), 0);
    assert_eq!(m.write.bytes.get(), 10_000);
    assert_eq!(m.write.in_flight.get(), 0);
    assert_eq!(m.write_present.get(), 0);
    assert_eq!((m.segment_blobs.count(), m.segment_blobs.sum()), (1, 1));
    assert_eq!(m.segment_bytes.count(), 1);
    assert!(m.segment_bytes.sum() > 10_000, "{}", m.segment_bytes.sum());
    assert_eq!(commits(m, "put_blobs"), 1);
    assert_eq!(m.object_get.count(), 0, "footer GETs counted as reads");

    // The same blob again is answered from the index (a blob stored just now needs no
    // touch): counted as present, and nothing stored.
    write(&channel, &blob).await;
    assert_eq!(m.write.total.get(), 2);
    assert_eq!(m.write_present.get(), 1);
    assert_eq!(m.segment_put.count(), 1);
    assert_eq!(commits(m, "touch"), 0);
    assert!(m.query.count() >= 2, "{}", m.query.count());

    let text = m.render();
    for line in [
        "kbf_bytestream_write_requests_total 2",
        "kbf_bytestream_write_present_total 1",
        "kbf_segment_put_seconds_count 1",
        "kbf_segment_blobs_sum 1",
        "kbf_meta_commit_seconds_count{command=\"put_blobs\"} 1",
    ] {
        assert!(text.lines().any(|l| l == line), "{line} not in\n{text}");
    }
}

/// Catches the in-flight level not raised while a write is open (a stalled upload
/// would not show), bytes counted only once the write finishes (a slow one would
/// show none until its end), and a write its caller abandoned not counted as an
/// error, or left in flight.
#[tokio::test]
async fn an_open_write_is_in_flight_until_it_ends() {
    let (cache, channel) = serve().await;
    let blob = Blob::new(vec![9_u8; 4_000]);
    let (mut messages, upload) = mpsc::channel(1);
    let mut client = bytestream(&channel);
    let call = tokio::spawn(async move { client.write(upload).await });
    messages
        .send(WriteRequest {
            resource_name: write_name(&blob),
            write_offset: 0,
            finish_write: false,
            data: blob.data[..1_000].to_vec(),
        })
        .await
        .expect("send");
    let m = cache.metrics();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while m.write.bytes.get() < 1_000 {
        assert!(tokio::time::Instant::now() < deadline, "no bytes counted");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(m.write.in_flight.get(), 1);
    assert_eq!(m.write.total.get(), 0);
    drop(messages);
    let ended = call.await.expect("the call task");
    assert!(ended.is_err(), "a write without finish_write succeeded");
    while m.write.in_flight.get() > 0 {
        assert!(tokio::time::Instant::now() < deadline, "still in flight");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!((m.write.total.get(), m.write.errors.get()), (1, 1));
    assert_eq!(m.segment_put.count(), 0);
}

/// Catches BatchUpdateBlobs not counted, its blobs or bytes not counted, its blobs
/// not stored as one segment (the segment's blob count says), and its latency not
/// recorded or shorter than its store.
#[tokio::test]
async fn a_batch_update_counts_its_blobs_into_one_segment() {
    let (cache, channel) = serve().await;
    let blobs: Vec<Blob> = (0..3_u8).map(|i| Blob::new(vec![i; 100])).collect();
    let requests = blobs
        .iter()
        .map(|b| kbf_proto::reapi::batch_update_blobs_request::Request {
            digest: Some(b.proto.clone()),
            data: b.data.clone(),
            ..Default::default()
        })
        .collect();
    cas(&channel)
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..Default::default()
        })
        .await
        .expect("BatchUpdateBlobs");
    let m = cache.metrics();
    assert_eq!(m.batch_update.total.get(), 1);
    assert_eq!(m.batch_update.errors.get(), 0);
    assert_eq!(m.batch_update_blobs.get(), 3);
    assert_eq!(m.batch_update.bytes.get(), 300);
    assert_eq!(m.batch_update.in_flight.get(), 0);
    assert_eq!((m.segment_blobs.count(), m.segment_blobs.sum()), (1, 3));
    let (_, batch) = seen(&m.batch_update.seconds);
    let (_, store) = seen(&m.store_blobs);
    assert!(batch >= store && store >= PUT, "{batch:?} {store:?}");
    assert_eq!(
        m.write.total.get(),
        0,
        "a batch counted as a ByteStream write"
    );
}

/// Catches FindMissingBlobs not counted or timed, and the digests asked and those
/// answered missing not counted (or swapped).
#[tokio::test]
async fn find_missing_counts_what_it_was_asked_and_answered() {
    let (cache, channel) = serve().await;
    let held = Blob::new(b"held".to_vec());
    write(&channel, &held).await;
    let absent = [Blob::new(b"one".to_vec()), Blob::new(b"two".to_vec())];
    let asked = [&held, &absent[0], &absent[1]];
    let missing = cas(&channel)
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: asked.iter().map(|b| b.proto.clone()).collect(),
            ..Default::default()
        })
        .await
        .expect("FindMissingBlobs")
        .into_inner()
        .missing_blob_digests;
    assert_eq!(missing.len(), 2);
    let m = cache.metrics();
    assert_eq!(m.find_missing.total.get(), 1);
    assert_eq!(m.find_missing.errors.get(), 0);
    assert_eq!(m.find_missing_digests.get(), 3);
    assert_eq!(m.find_missing_missing.get(), 2);
    assert_eq!(m.find_missing.seconds.count(), 1);
}

/// Catches a read's object GET not timed, or the footer read-back of a write
/// counted with the reads.
#[tokio::test]
async fn a_read_times_its_object_get() {
    let (cache, channel) = serve().await;
    let blob = Blob::new(vec![3_u8; 5_000]);
    write(&channel, &blob).await;
    let m = cache.metrics();
    assert_eq!(m.object_get.count(), 0);
    let mut stream = bytestream(&channel)
        .read(ReadRequest {
            resource_name: format!(
                "blobs/{}/{}",
                blob.digest.hash_hex(),
                blob.digest.size_bytes
            ),
            ..ReadRequest::default()
        })
        .await
        .expect("Read")
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(message) = stream.message().await.expect("a message") {
        bytes.extend_from_slice(&message.data);
    }
    assert_eq!(bytes, blob.data);
    let (gets, get) = seen(&m.object_get);
    assert_eq!(gets, 1);
    assert!(get >= GET, "{get:?}");
}
