//! Admission on the server's REAPI listener (`docs/design/write-path.md`, sections
//! 3.3 and 9): concurrent ByteStream `Write`s over one connection are admitted before
//! they buffer, so the bytes held in receive buffers stay under the bound, and every
//! upload still completes (a caller over the bound waits; nothing is refused).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::stream;
use kbf_front::{Cache, MemoryMetaLog, WriteConfig};
use kbf_meta::Retention;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::{Listeners, bind_server};
use tonic::transport::Endpoint;

/// The size of each blob, and of each message carrying part of one.
const BLOB: usize = 4 << 20;
const CHUNK: usize = 1 << 20;

/// The writes, and the bound: room for three Writes at once, so 45 wait. The test of
/// `docs/design/write-path.md` section 9 is 48 Writes of 64 MiB against a 1 GiB bound
/// (sixteen at once); this one is smaller, so the server does not hash 3 GiB, and
/// leaves more Writes waiting, each holding unread data on the one connection, which
/// is what the flow-control windows must allow for.
const WRITES: usize = 48;
const BOUND: u64 = 3 * BLOB as u64;

/// A bucket that keeps only the last [`KEPT`] bytes of each object (the footer the
/// cache reads back), so the uploads do not stay in memory. Reads of anything earlier
/// fail.
#[derive(Default)]
struct TailStore(Mutex<BTreeMap<ObjectKey, (u64, Bytes)>>);

const KEPT: usize = 64 << 10;

impl ObjectStore for TailStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        _retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let from = body.len().saturating_sub(KEPT);
        let tail = Bytes::copy_from_slice(&body[from..]);
        let mut objects = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        objects.insert(key.clone(), (from as u64, tail));
        Ok(())
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let objects = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (from, tail) = objects
            .get(key)
            .ok_or_else(|| ObjectStoreError::NotFound(key.clone()))?;
        let start = range
            .offset()
            .checked_sub(*from)
            .ok_or_else(|| ObjectStoreError::Protocol("not kept".to_owned()))?;
        let start = usize::try_from(start).expect("small").min(tail.len());
        let end = start.saturating_add(usize::try_from(range.size()).expect("small"));
        Ok(tail.slice(start..end.min(tail.len())))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let mut objects = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        objects.remove(key);
        Ok(())
    }

    async fn list(
        &self,
        _prefix: &KeyPrefix,
        _after: Option<&ListToken>,
        _max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        Err(ObjectStoreError::Unsupported("list"))
    }
}

fn listeners() -> Listeners {
    Listeners {
        reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
        worker: SocketAddr::from(([127, 0, 0, 1], 0)),
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(1),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
    }
}

/// 48 concurrent 4 MiB Writes against a 12 MiB bound, over one connection to the
/// REAPI listener as `bind_server` sets it up: every Write completes, the bytes held in
/// receive buffers never passed the bound, and at least two Writes buffered at once
/// (the bound is not a lock).
///
/// Catches: an admission taken after buffering, which lets every Write buffer at once;
/// a connection window smaller than every stream's window, with which the Writes
/// waiting for admission take the whole window and the admitted ones never finish (the
/// timeout fires). Mutants seen red: the permit taken in `store_blobs` instead of at
/// stream start; the listener's flow-control windows left at the transport's defaults.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_stay_under_the_bytes_in_flight() {
    let writes = WriteConfig {
        bytes_in_flight: BOUND,
        ..WriteConfig::DEFAULT
    };
    let meta = MemoryMetaLog::new(Retention::default());
    let cache = Arc::new(
        Cache::open(meta, TailStore::default(), KeyPrefix::default())
            .await
            .expect("open")
            .with_writes(writes),
    );
    let bound = bind_server(Arc::clone(&cache), listeners(), std::future::pending()).expect("bind");
    let reapi = bound.reapi;
    tokio::spawn(bound.serving);
    let channel = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");

    let mut calls = Vec::new();
    let mut digests = Vec::new();
    for i in 0..WRITES {
        let mut data = vec![0u8; BLOB];
        data[..8].copy_from_slice(&(i as u64).to_le_bytes());
        let digest = kbf_segments::sha256(&data);
        digests.push(digest);
        let name = format!("uploads/u{i}/blobs/{}/{BLOB}", digest.hash_hex());
        let last = BLOB / CHUNK - 1;
        let messages: Vec<WriteRequest> = data
            .chunks(CHUNK)
            .enumerate()
            .map(|(j, c)| WriteRequest {
                resource_name: if j == 0 { name.clone() } else { String::new() },
                write_offset: (j * CHUNK) as i64,
                finish_write: j == last,
                data: c.to_vec(),
            })
            .collect();
        let mut client = ByteStreamClient::new(channel.clone());
        calls.push(tokio::spawn(async move {
            client.write(stream::iter(messages)).await
        }));
    }
    let all = async {
        for call in calls {
            let response = call.await.expect("join").expect("write").into_inner();
            assert_eq!(response.committed_size, BLOB as i64);
        }
    };
    tokio::time::timeout(Duration::from_secs(60), all)
        .await
        .expect("every Write finishes");
    let peak = cache.write_buffer_peak();
    assert!(
        peak <= BOUND,
        "{peak} bytes buffered over a {BOUND}-byte bound"
    );
    assert!(
        peak >= 2 * BLOB as u64,
        "{peak}: the Writes never overlapped"
    );
    for d in &digests {
        assert!(cache.is_durable(d).await.expect("durable"), "{d}");
    }
}
