//! What the engine bounds: requests and bytes in flight for the whole process, one
//! connection with explicit windows and decode limit, one fetch per digest, and what a
//! dropped lease cancels.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, Scope, TransferConfig, digest_of};
use kbf_types::LeaseId;

use super::fake::{Fake, until, within};
use super::proxy::{Proxy, windows};
use super::{engine, patterned};

/// T-1 (docs/design/worker-cache.md#10-test-plan). 16 leases each read the same 1,000
/// blobs (half of them in reverse order) while the server holds every read: the reads
/// in flight never pass `--transfer-in-flight` (and reach it), each digest is read from
/// the server once, the other 15,000 requests join those reads, and all of it goes over
/// one connection. Catches per-call limits that multiply by the leases, duplicate
/// fetches, and a connection per lease or per request. Mutants seen red: a semaphore
/// per scope (34 reads at once for a limit of 16); no in-flight map (none of the 15,000
/// requests joined a read).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1_sixteen_leases_share_one_limit_one_connection_and_one_read_per_digest() {
    let fake = Arc::new(Fake::default());
    let digests: Vec<Digest> = (0..1000)
        .map(|i| fake.insert(patterned(i, 100 + i)))
        .collect();
    let (proxy, addr) = Proxy::start(fake.serve().await).await;
    let limit = 16;
    let config = TransferConfig {
        in_flight: limit,
        ..TransferConfig::default()
    };
    let engine = engine(addr, config);
    fake.close();
    let scopes: Vec<Arc<Scope>> = (0..16)
        .map(|i| Arc::new(engine.scope(&LeaseId::new(1, i))))
        .collect();
    let mut leases = Vec::new();
    for (i, scope) in scopes.iter().enumerate() {
        let scope = Arc::clone(scope);
        let mut order = digests.clone();
        if i % 2 == 1 {
            order.reverse();
        }
        leases.push(tokio::spawn(async move {
            let reads = futures::future::join_all(order.iter().map(|d| scope.get(d))).await;
            for (digest, read) in order.iter().zip(reads) {
                assert_eq!(digest_of(&read.expect("read")), *digest);
            }
        }));
    }
    let merged = || scopes.iter().map(|s| s.stats().merged_reads).sum::<u64>();
    within(30, || merged() == 15 * 1000).await;
    within(30, || fake.active.load(Ordering::SeqCst) >= limit).await;
    assert_eq!(merged(), 15 * 1000, "requests that joined another's read");
    assert_eq!(fake.active.load(Ordering::SeqCst), limit, "reads at once");
    let stats = engine.stats();
    assert_eq!((stats.requests_in_flight, stats.fetches), (limit, 1000));
    fake.open();
    for lease in leases {
        lease.await.expect("lease");
    }
    assert_eq!(fake.most.load(Ordering::SeqCst), limit, "reads at once");
    for digest in &digests {
        assert_eq!(fake.reads_of(digest), [0], "{digest:?}");
    }
    let started: u64 = scopes.iter().map(|s| s.stats().read_calls).sum();
    assert_eq!(started, 1000);
    assert_eq!(proxy.accepted.load(Ordering::SeqCst), 1, "connections");
    assert_eq!(engine.stats().fetches, 0);
}

/// Catches reads into memory that ignore `--transfer-bytes-in-flight`: with a budget of
/// one blob, three blobs of that size and one twice as large (which waits for the whole
/// budget and goes alone) are read one at a time, though eight requests may be in
/// flight. Mutant seen red: no byte budget (4 reads at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_into_memory_stay_within_the_byte_budget() {
    let fake = Arc::new(Fake::default());
    let mut digests: Vec<Digest> = (0..3).map(|i| fake.insert(patterned(i, 1000))).collect();
    digests.push(fake.insert(patterned(9, 2000)));
    let config = TransferConfig {
        in_flight: 8,
        bytes_in_flight: 1000,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    fake.close();
    let reads = tokio::spawn({
        let engine = Arc::clone(&engine);
        let digests = digests.clone();
        async move { futures::future::join_all(digests.iter().map(|d| engine.get(d))).await }
    });
    until("a read started", || fake.active.load(Ordering::SeqCst) >= 1).await;
    // Room for a second read to start, were it allowed to.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.active.load(Ordering::SeqCst), 1, "reads at once");
    assert_eq!(engine.stats().bytes_in_flight, 1000);
    fake.open();
    for read in reads.await.expect("reads") {
        read.expect("read");
    }
    assert_eq!(fake.most.load(Ordering::SeqCst), 1, "reads at once");
    assert_eq!(engine.stats().bytes_in_flight, 0);
}

/// Catches the engine's connection left at HTTP/2's default windows (64 KiB, which caps
/// a stream at the window per round trip): it asks for `--transfer-stream-window-bytes`
/// per stream and eight times that for the connection. Mutant seen red: no window set
/// (hyper's defaults went out instead: 2 MiB and 5 MiB).
#[tokio::test]
async fn the_connection_asks_for_the_configured_windows() {
    let fake = Arc::new(Fake::default());
    let digest = fake.insert(b"window".to_vec());
    let (proxy, addr) = Proxy::start(fake.serve().await).await;
    let config = TransferConfig {
        stream_window_bytes: 3 << 20,
        ..TransferConfig::default()
    };
    engine(addr, config).get(&digest).await.expect("read");
    let first = super::fake::lock(&proxy.first_bytes)[0].clone();
    assert_eq!(windows(&first), (Some(3 << 20), 24 << 20));
}

/// Catches tonic's 4 MiB decode limit left on the engine's clients: a 4.5 MiB message,
/// which the front may send (its limit is 5 MiB), is read. Mutant seen red: no
/// `max_decoding_message_size` (OUT_OF_RANGE, "decoded message length too large").
#[tokio::test]
async fn a_message_past_four_mib_is_decoded() {
    let fake = Arc::new(Fake::default());
    let bytes = patterned(4, (4 << 20) + (512 << 10));
    let digest = fake.insert(bytes.clone());
    fake.message_bytes.store(bytes.len(), Ordering::SeqCst);
    let engine = engine(fake.serve().await, TransferConfig::default());
    assert_eq!(engine.get(&digest).await, Ok(bytes));
}

/// T-3. With one request in flight, lease A asks for X, Y and Z: X is being read (held
/// by the server), Y and Z are queued. Lease B asks for Y too. Lease A is dropped. X
/// and Z are given up, Z never reaching the server; Y, which B still waits for, is read
/// once and B gets it. Catches leaked work (a dropped lease's queued reads still run)
/// and wrongly cancelled work (a shared read cancelled with the lease that started
/// it). Mutants seen red: give up a flight whatever its waiters (B got "the fetch
/// ended without an answer"); never give up (3 reads left, not 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t3_a_dropped_lease_cancels_what_it_queued_but_not_a_shared_read() {
    let fake = Arc::new(Fake::default());
    let [x, y, z] = [0, 1, 2].map(|i| fake.insert(patterned(i, 3000)));
    let config = TransferConfig {
        in_flight: 1,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    fake.close();
    let a = Arc::new(engine.scope(&LeaseId::new(2, 1)));
    let b = Arc::new(engine.scope(&LeaseId::new(2, 2)));
    // X first, so it takes the one permit; Y and Z once the server holds it.
    let (go, x_started) = tokio::sync::oneshot::channel::<()>();
    let lease_a = tokio::spawn({
        let a = Arc::clone(&a);
        let (x, y, z) = (x.clone(), y.clone(), z.clone());
        async move {
            let rest = async {
                let _ = x_started.await;
                futures::join!(a.get(&y), a.get(&z))
            };
            let _ = futures::join!(a.get(&x), rest);
        }
    });
    until("X is being read", || fake.reads_of(&x).len() == 1).await;
    go.send(()).expect("lease A waits");
    until("Y and Z are queued", || engine.stats().fetches == 3).await;
    let lease_b = tokio::spawn({
        let (b, y) = (Arc::clone(&b), y.clone());
        async move { b.get(&y).await }
    });
    until("B waits on Y", || b.stats().merged_reads == 1).await;
    lease_a.abort();
    assert!(lease_a.await.expect_err("aborted").is_cancelled());
    drop(a);
    within(30, || engine.stats().fetches == 1).await;
    assert_eq!(engine.stats().fetches, 1, "reads left: Y alone");
    fake.open();
    let got = lease_b
        .await
        .expect("lease B")
        .map(|bytes| digest_of(&bytes));
    assert_eq!(got, Ok(y.clone()));
    assert_eq!(fake.reads_of(&x), [0]);
    assert_eq!(fake.reads_of(&y), [0]);
    assert!(
        fake.reads_of(&z).is_empty(),
        "a queued read of a dropped lease ran"
    );
    assert_eq!(engine.stats().fetches, 0);
}

/// Catches a read given up when more than half of its blob had arrived (the next lease
/// to ask would fetch it all again): lease A's read of H stops waiting after 2,000 of
/// its 3,000 bytes; lease B then asks for H and joins the same read, which the server
/// serves once. Mutant seen red: give up whatever has arrived (B joined no read).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_more_than_half_done_finishes_for_the_next_caller() {
    let fake = Arc::new(Fake::default());
    let h = fake.insert(patterned(7, 3000));
    fake.message_bytes.store(2000, Ordering::SeqCst);
    fake.pause.store(true, Ordering::SeqCst);
    let engine = engine(fake.serve().await, TransferConfig::default());
    fake.close();
    let a = Arc::new(engine.scope(&LeaseId::new(3, 1)));
    let lease_a = tokio::spawn({
        let (a, h) = (Arc::clone(&a), h.clone());
        async move { a.get(&h).await }
    });
    until("2,000 bytes of H arrived", || a.stats().read_bytes == 2000).await;
    lease_a.abort();
    let _ = lease_a.await;
    let b = Arc::new(engine.scope(&LeaseId::new(3, 2)));
    let read = tokio::spawn({
        let (b, h) = (Arc::clone(&b), h.clone());
        async move { b.get(&h).await }
    });
    within(10, || b.stats().merged_reads == 1).await;
    assert_eq!(b.stats().merged_reads, 1, "B joined A's read");
    fake.open();
    let got = read.await.expect("lease B").map(|bytes| digest_of(&bytes));
    assert_eq!(got, Ok(h.clone()));
    assert_eq!(fake.reads_of(&h), [0]);
}

/// Catches a waiter that hangs, or panics, when its read's task is gone without an
/// answer (the runtime it ran on shut down, as at daemon exit): it gets UNAVAILABLE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_whose_task_is_gone_is_unavailable() {
    let fake = Arc::new(Fake::default());
    let digest = fake.insert(b"never answered".to_vec());
    let engine = engine(fake.serve().await, TransferConfig::default());
    fake.close();
    let other = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    // The read's task is spawned on the other runtime, by the first caller.
    other.spawn({
        let (engine, digest) = (Arc::clone(&engine), digest.clone());
        async move { engine.get(&digest).await }
    });
    until("the read started", || engine.stats().fetches == 1).await;
    let scope = Arc::new(engine.scope(&LeaseId::new(4, 1)));
    let read = tokio::spawn({
        let (scope, digest) = (Arc::clone(&scope), digest.clone());
        async move { scope.get(&digest).await }
    });
    until("the second caller joined", || {
        scope.stats().merged_reads == 1
    })
    .await;
    other.shutdown_background();
    let got = read.await.expect("read");
    assert!(
        matches!(&got, Err(CasError::Unavailable(_, why)) if why == "the fetch ended without an answer"),
        "{got:?}"
    );
}
