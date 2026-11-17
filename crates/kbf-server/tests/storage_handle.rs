//! The storage node `kbf storage` starts (`kbf_server::storage_node`): the storage it serves
//! handed to a test, and its stop draining a write in flight.

use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_server::{BoundStorage, StorageListener, bind_storage};
use kbf_storage::Storage as _;
use kbf_storage::service::RemoteStorage;
use sha2::Digest as _;

/// A directory of its own, removed when it drops.
fn dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("storage-handle-{name}"),
    )
}

/// The flags of `kbf storage`, plain text on a loopback port, and these.
fn flags(more: &[String]) -> StorageFlags {
    let mut argv = vec!["storage".to_owned()];
    if !more.iter().any(|m| m.starts_with("--storage-listen")) {
        argv.push("--storage-listen=127.0.0.1:0".to_owned());
    }
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken")
}

/// A `file://` cold store and a hot store, both in `dir`.
fn file_stores(dir: &Path) -> Vec<String> {
    vec![
        format!("--cold-store=file://{}", dir.join("cold").display()),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--hot-store=file://{}", dir.join("hot").display()),
    ]
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// Catches a handle that is not the storage the listener serves (a test would read and write
/// another store than other nodes reach over gRPC), and a start line that is not
/// `kbf storage`'s: a blob put through the handle is read back over gRPC from the listener,
/// and one written over gRPC is read through the handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handle_is_the_storage_the_listener_serves() {
    let dir = dir("same");
    let node = start_storage(&flags(&file_stores(&dir)), never)
        .await
        .expect("starts");
    let line = node.start_line().to_owned();
    let want = format!(
        "kbf-storage {} storage={} log=",
        kbf_server::SERVER_VERSION,
        node.addr
    );
    assert!(line.starts_with(&want), "{line}");
    let (storage, addr) = (node.storage(), node.addr);
    tokio::spawn(node.serve());
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let remote = RemoteStorage::new(channel, 0);
    let here = Bytes::from_static(b"put in-process, read over gRPC");
    let digest = storage.put(here.clone()).await.expect("put");
    assert_eq!(remote.read_blob(&digest).await.expect("read"), here);
    let there = Bytes::from_static(b"put over gRPC, read in-process");
    let digest = remote.put(there.clone()).await.expect("put");
    assert_eq!(storage.read_blob(&digest).await.expect("read"), there);
}

/// A storage that tells the test when the service first asks it about a blob: a ByteStream
/// `Write` does that right after it takes the first message off the stream, so the signal
/// means the server has accepted the stream and the write is in flight. Everything else is
/// the inner storage's.
struct Watched {
    inner: Arc<dyn kbf_storage::Storage>,
    asked: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl kbf_storage::Storage for Watched {
    fn find_missing<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> kbf_storage::Answer<'a, Vec<kbf_types::Digest>> {
        if let Some(asked) = self.asked.lock().expect("lock").take() {
            let _ = asked.send(());
        }
        self.inner.find_missing(digests)
    }
    /// Forwards: the inner storage decides what is durable.
    fn find_durable<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> kbf_storage::Answer<'a, Vec<kbf_types::Digest>> {
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a kbf_types::Digest) -> kbf_storage::Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }
    fn read_range<'a>(
        &'a self,
        digest: &'a kbf_types::Digest,
        offset: u64,
        limit: u64,
    ) -> kbf_storage::Answer<'a, kbf_storage::ReadStream> {
        self.inner.read_range(digest, offset, limit)
    }
    fn put(&self, bytes: Bytes) -> kbf_storage::Answer<'_, kbf_types::Digest> {
        self.inner.put(bytes)
    }
    fn stream(&self, size: u64) -> Option<Box<dyn kbf_storage::PutStream>> {
        self.inner.stream(size)
    }
    fn segments(&self) -> Arc<dyn kbf_proto::worker::segments_server::Segments> {
        self.inner.segments()
    }
    fn action_result<'a>(
        &'a self,
        action: &'a kbf_types::Digest,
    ) -> kbf_storage::Answer<'a, Option<(kbf_types::Digest, kbf_proto::reapi::ActionResult)>> {
        self.inner.action_result(action)
    }
    fn prepare_action_result<'a>(
        &'a self,
        result: &'a kbf_proto::reapi::ActionResult,
    ) -> kbf_storage::Answer<'a, kbf_meta::ActionRecord> {
        self.inner.prepare_action_result(result)
    }
    fn commit_action_record(
        &self,
        role: kbf_meta::Role,
        action: kbf_types::Digest,
        record: kbf_meta::ActionRecord,
    ) -> kbf_storage::Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// Whether a new connection to `addr` gets a blob put through within half a second: a
/// call the node takes. A listener that no longer accepts leaves the call unanswered.
async fn takes_a_call(addr: std::net::SocketAddr) -> bool {
    let call = async {
        let endpoint =
            tonic::transport::Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
        match endpoint.connect().await {
            Ok(channel) => {
                let blob = Bytes::from_static(b"a late call");
                RemoteStorage::new(channel, 0).put(blob).await.is_ok()
            }
            Err(_) => false,
        }
    };
    matches!(
        tokio::time::timeout(std::time::Duration::from_millis(500), call).await,
        Ok(true)
    )
}

/// Catches a stop that cuts a write in flight instead of draining it: a ByteStream write
/// has sent its first half and the server has taken it up (the storage was asked about
/// the blob) when the shutdown future completes; once the runtime has run the stop
/// through the listener, the second half goes, and the write must commit the whole blob,
/// `serving` end only after it, and the blob read back. It also catches a stop that goes
/// on taking new calls while it drains: until the write ends, a call on a new connection
/// is taken by no one (once the listener has seen the stop; until then it is served).
///
/// The stop waits for the server, not for the client: a stop that beat the stream's
/// arrival would be refused by the listener, which is right and is not what is tested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_drains_a_write_in_flight() {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let (asked, in_flight) = tokio::sync::oneshot::channel::<()>();
    let held: Arc<dyn kbf_storage::Storage> = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let storage: Arc<dyn kbf_storage::Storage> = Arc::new(Watched {
        inner: Arc::clone(&held),
        asked: std::sync::Mutex::new(Some(asked)),
    });
    let BoundStorage { addr, serving } = bind_storage(
        Arc::clone(&storage),
        StorageListener {
            listen: "127.0.0.1:0".parse().expect("address"),
            tls: None,
            service: kbf_storage::service::Config {
                write_bytes_in_flight: 64 << 20,
            },
        },
        async move {
            let _ = stopped.await;
        },
    )
    .expect("binds");
    let serving = tokio::spawn(serving);

    let blob: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
    let (head, tail) = blob.split_at(1024);
    let hash: String = sha2::Sha256::digest(&blob)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name = format!("uploads/drain/blobs/{hash}/{}", blob.len());
    let first = WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: false,
        data: head.to_vec(),
    };
    let second = WriteRequest {
        resource_name: String::new(),
        write_offset: 1024,
        finish_write: true,
        data: tail.to_vec(),
    };
    // The second request is released once the test says so.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let requests = stream::unfold(
        (Some(first), Some(second), Some(released)),
        |(first, second, released)| async move {
            if let Some(first) = first {
                return Some((first, (None, second, released)));
            }
            let _ = released?.await;
            let second = second?;
            Some((second, (None, None, None)))
        },
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let writing = tokio::spawn(async move {
        ByteStreamClient::new(channel)
            .write(requests)
            .await
            .map(tonic::Response::into_inner)
    });
    in_flight.await.expect("the server took the write up");
    stop.send(()).expect("stop");
    // Let the runtime run the stop through the listener: a bounded number of turns, not
    // a time. A listener that cut its calls ends `serve` within them.
    for _ in 0..1000 {
        if serving.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!serving.is_finished(), "serve ended with a write open");
    // A call made now is not taken. One made before the listener has seen the stop is, so
    // try again a bounded number of times: a listener that went on taking calls while it
    // drains takes every one.
    let mut refused = false;
    for _ in 0..200 {
        if !takes_a_call(addr).await {
            refused = true;
            break;
        }
    }
    assert!(refused, "the stopping node took a new call");
    assert!(!serving.is_finished(), "serve ended with a write open");
    release.send(()).expect("release");
    let response = writing.await.expect("task").expect("the write drained");
    assert_eq!(response.committed_size, 2048);
    serving.await.expect("task").expect("serves to the end");
    let digest =
        kbf_types::Digest::parse(kbf_types::DigestFunction::Sha256, &format!("{hash}/2048"))
            .expect("digest");
    assert_eq!(held.read_blob(&digest).await.expect("read"), blob);
}
