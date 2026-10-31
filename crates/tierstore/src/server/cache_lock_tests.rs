//! A build node and the lock of its cache directory: a start beside a process that holds the
//! directory, and a start that is alone.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};

use super::Drained;
use super::cache_cli_tests::{Node, client, probe, serving};
use super::local_cache::testing::{pattern, tree};
use super::local_cache::{Disk, LocalCache, Mode, SystemDisk};
use super::lock::try_take;
use crate::{Backend, Clock, FaultyBackend, MemoryBackend, SystemClock};

/// Asserts that a build node started over a directory whose lock another cache holds serves and
/// writes nothing to the directory: `/readyz` says 200 `ready read-only`, the gauge says it, a
/// hit is read from the directory and a miss comes from the upstream, the start-up probe of the
/// file system is not made (it writes under `.stage/` and `obj/`), nothing the holder left in
/// `.stage/` is removed, and the listing of the whole directory is the same after the node has
/// drained as before it started. It would catch a start that cleans up or probes in a directory
/// that another process writes to.
#[tokio::test]
async fn a_build_node_over_a_held_directory_serves_read_only_and_leaves_the_directory_alone() {
    let node = Node::new("tscache-lock-ro");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    let body = pattern(1000);
    for key in ["cas/hit", "cas/miss"] {
        memory.put(key, Bytes::from(body.clone())).await.unwrap();
    }
    let upstream = Arc::new(FaultyBackend::new(memory.clone()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let for_holder: Arc<dyn Backend> = upstream.clone();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache = settings.cache.clone();
    let holder = LocalCache::open_locked(cache, for_holder, Arc::clone(&disk), clock).unwrap();
    assert_eq!(holder.mode(), Mode::ReadWrite);
    let _ = holder.get("cas/hit", None).await.unwrap();
    let root = node.cache_dir();
    let stage = root.join(".stage");
    std::fs::write(stage.join("0000000000000009"), b"a fill of the holder").unwrap();
    let before = tree(&root);

    let work = |addr: SocketAddr| async move {
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-only"));
        let store = client(addr);
        let all = ByteRange::new(0, 1000).unwrap();
        for key in ["cas/hit", "cas/miss"] {
            let key = ObjectKey::new(key).unwrap();
            assert_eq!(store.get_range(&key, all).await.unwrap(), body);
        }
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        for line in [
            "tierstore_cache_mode{mode=\"read_only\"} 1\n",
            "tierstore_cache_mode{mode=\"read_write\"} 0\n",
            "tierstore_cache_hits_total 1\n",
            "tierstore_cache_misses_total 1\n",
            "tierstore_cache_fills_total 0\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert_eq!(tree(&root), before);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that a build node that is alone holds the lock of its cache directory for as long as
/// it runs, says `ready read-write` from its first answer, and lets the lock go when it exits:
/// a try of the lock from outside finds it held while the node serves and takes it after. It
/// would catch a node that serves read-write without holding anything, which a second node could
/// then start beside.
#[tokio::test]
async fn a_lone_build_node_holds_the_lock_while_it_serves() {
    let node = Node::new("tscache-lock-lone");
    let settings = node.settings("", "");
    let upstream = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let lock = node.cache_dir().join(".lock");
    let work = |addr: SocketAddr| async move {
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-write"));
        assert!(try_take(&lock).unwrap().is_none());
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    // The tasks of the drain that were cut drop their hold of the cache on the runtime's next
    // turns, so the lock is free after a few yields; a node that never lets it go fails here.
    let lock = node.cache_dir().join(".lock");
    let mut freed = false;
    for _ in 0..100_000 {
        freed = try_take(&lock).unwrap().is_some();
        if freed {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(freed, "the lock was still held after the node exited");
}
