//! `kbf storage-and-daemon`'s storage half (`kbf_server::storage_half`): the storage
//! node `kbf storage` starts, with the storage it serves handed to the caller and the
//! node's one cache honoured.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::storage_half::start;
use kbf_server::storage_node::{StorageFlags, command, parse};
use kbf_storage::Storage as _;
use kbf_storage::local_cache::LocalCache;
use kbf_storage::service::RemoteStorage;
use sha2::Digest as _;

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-half")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
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
        format!("--hot-store=file://{}", dir.join("hot").display()),
    ]
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// Catches a handle that is not the storage the listener serves (the daemon half would
/// read and write another store than other nodes reach over gRPC), and a start line that
/// is not `kbf storage`'s: a blob put through the handle is read back over gRPC from the
/// listener, and one written over gRPC is read through the handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handle_is_the_storage_the_listener_serves() {
    let half = start(&flags(&file_stores(&dir("same"))), never)
        .await
        .expect("starts");
    let line = half.start_line().to_owned();
    let want = format!(
        "kbf-storage {} storage={} log=",
        kbf_server::SERVER_VERSION,
        half.addr
    );
    assert!(line.starts_with(&want), "{line}");
    let (storage, addr) = (half.storage(), half.addr);
    tokio::spawn(half.serve());
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

/// Catches the node's one cache not given the storage half's bytes (its budget would
/// leave the hot store out): with `local_cache` set, the half attaches to it at start,
/// so a second attach is refused; without one there is nothing to attach to.
#[tokio::test]
async fn the_one_cache_counts_the_storage_halfs_bytes() {
    let mut shared = flags(&file_stores(&dir("one-cache")));
    let local = Arc::new(LocalCache::new());
    shared.server.local_cache = Some(Arc::clone(&local));
    let half = start(&shared, never).await.expect("starts");
    assert!(
        !local.attach_storage(Box::new(|| 0)),
        "the half did not attach"
    );
    drop(half);
}

/// Catches a half that serves past its shutdown future, or never starts serving.
#[tokio::test]
async fn the_half_stops_when_told() {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let half = start(&flags(&file_stores(&dir("stop"))), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let serving = tokio::spawn(half.serve());
    stop.send(()).expect("stop");
    serving.await.expect("task").expect("serves to the end");
}

/// Catches a start whose listener is checked after a store is opened, or not at all: a
/// plain-text listener off loopback is refused as `kbf storage` refuses it, and no store
/// is made.
#[tokio::test]
async fn a_listener_kbf_storage_refuses_is_refused_first() {
    let d = dir("refused");
    let mut wide = file_stores(&d);
    wide.push("--storage-listen=0.0.0.0:0".to_owned());
    let refused = start(&flags(&wide), never).await.err().expect("refused");
    let said = refused.to_string();
    assert!(said.contains("0.0.0.0:0"), "{said}");
    assert!(!d.join("hot").exists(), "a refused start opened a store");
}

/// Catches a stop that cuts a write in flight instead of draining it: a ByteStream write
/// has sent its first half when the shutdown future completes; once the runtime has run the
/// stop through the listener, the second half goes, and the write must commit the whole
/// blob, `serve` end only after it, and the blob read through the
/// handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_drains_a_write_in_flight() {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let half = start(&flags(&file_stores(&dir("drain"))), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let (storage, addr) = (half.storage(), half.addr);
    let serving = tokio::spawn(half.serve());

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
    // The second request is released once the first has been taken for sending.
    let (sent, first_sent) = tokio::sync::oneshot::channel::<()>();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let requests = stream::unfold(
        (Some(first), Some(second), Some(sent), Some(released)),
        |(first, second, sent, released)| async move {
            if let Some(first) = first {
                return Some((first, (None, second, sent, released)));
            }
            if let Some(sent) = sent {
                let _ = sent.send(());
            }
            let _ = released?.await;
            let second = second?;
            Some((second, (None, None, None, None)))
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
    first_sent.await.expect("the first half was taken");
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
    release.send(()).expect("release");
    let response = writing.await.expect("task").expect("the write drained");
    assert_eq!(response.committed_size, 2048);
    serving.await.expect("task").expect("serves to the end");
    let digest =
        kbf_types::Digest::parse(kbf_types::DigestFunction::Sha256, &format!("{hash}/2048"))
            .expect("digest");
    assert_eq!(storage.read_blob(&digest).await.expect("read"), blob);
}
