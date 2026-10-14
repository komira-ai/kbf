//! What the engine bounds: requests and bytes in flight for the whole process, one
//! connection with explicit windows and decode limit, one fetch per digest, and what a
//! dropped lease cancels.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::future::Either;
use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, Chunks, Scope, TransferConfig, digest_of};
use kbf_types::LeaseId;

use super::fake::{Fake, lock, until, within};
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

/// T-1 for the other requests the limit counts. 16 leases upload blobs from memory
/// (`put`) and from chunks (`put_chunks`), each after its FindMissing, while the server
/// holds every write before it commits. Three held phases:
/// 1. The server holds FindMissing too. Calls go one at a time and uploads wait for
///    their answer, so the held call is the one request and must hold the one permit
///    taken: a call that takes none, or releases it before the call, shows 0.
/// 2. FindMissing is answered until held writes fill `--transfer-in-flight`: a write
///    without a permit passes it.
/// 3. With every permit a held write's, the server holds FindMissing again and each
///    lease starts more uploads and reads: a FindMissing (or read) that reaches the
///    server now took no permit and stays there to be counted. FindMissing asks four
///    digests a call, so calls keep coming.
/// Then the gates open, and the most requests ever served at once (reads, writes and
/// FindMissing together) is the limit. The per-walk limits of kbf-outputs and the
/// container driver are per lease, so 16 leases are held to the process's limit only
/// by the engine. Mutants seen red every run: no permit in `put` (phase 2), in
/// `put_chunks` (phase 2), in FindMissing (phases 1 and 3), FindMissing releasing its
/// permit before the call (phase 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploads_finds_and_reads_of_sixteen_leases_share_the_one_limit() {
    let fake = Arc::new(Fake::default());
    let limit = 16;
    let config = TransferConfig {
        in_flight: limit,
        find_missing_chunk: 4,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    let scopes: Vec<Arc<Scope>> = (0..16)
        .map(|i| Arc::new(engine.scope(&LeaseId::new(6, i))))
        .collect();
    // Lease i's blobs of wave w: 10 from memory and 10 from chunks, interleaved.
    let blob = |lease: usize, wave: usize, j: usize| {
        patterned(10_000 + lease * 100 + wave * 40 + j, 50 + j)
    };
    let uploads = |wave: usize| -> Vec<_> {
        scopes
            .iter()
            .enumerate()
            .map(|(i, scope)| {
                let scope = Arc::clone(scope);
                let blobs: Vec<Vec<u8>> = (0..20).map(|j| blob(i, wave, j)).collect();
                tokio::spawn(async move {
                    let calls = blobs.iter().enumerate().map(|(j, bytes)| {
                        if j % 2 == 0 {
                            Either::Left(scope.put(bytes.clone()))
                        } else {
                            Either::Right(scope.put_chunks(digest_of(bytes), chunks_of(bytes)))
                        }
                    });
                    let stored = futures::future::join_all(calls).await;
                    for (bytes, stored) in blobs.iter().zip(stored) {
                        assert_eq!(stored, Ok(digest_of(bytes)));
                    }
                })
            })
            .collect()
    };
    fake.close();
    fake.close_finds();
    let mut leases = uploads(0);
    // One FindMissing call at a time, and every upload waits for its answer: the first
    // call, held at the server, is the one request, and holds the one permit taken.
    within(30, || fake.requests.load(Ordering::SeqCst) >= 1).await;
    assert_eq!(
        (
            fake.requests.load(Ordering::SeqCst),
            engine.stats().requests_in_flight
        ),
        (1, 1),
        "(requests at the server, permits taken) with FindMissing held"
    );
    fake.open_finds();
    within(30, || fake.held_writes.load(Ordering::SeqCst) >= limit).await;
    // Room for writes past the limit to reach the server, were they allowed to.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        (
            fake.held_writes.load(Ordering::SeqCst),
            fake.requests.load(Ordering::SeqCst)
        ),
        (limit, limit),
        "(held writes, requests) at once"
    );
    // Every permit is a held write's, so a FindMissing that reaches the server from
    // here on took none, and stays there to be counted.
    fake.close_finds();
    leases.extend(uploads(1));
    let reads: Vec<Digest> = (0..160)
        .map(|i| fake.insert(patterned(20_000 + i, 70)))
        .collect();
    for (i, scope) in scopes.iter().enumerate() {
        let scope = Arc::clone(scope);
        let mine = reads[i * 10..(i + 1) * 10].to_vec();
        leases.push(tokio::spawn(async move {
            for (digest, read) in mine
                .iter()
                .zip(futures::future::join_all(mine.iter().map(|d| scope.get(d))).await)
            {
                assert_eq!(digest_of(&read.expect("read")), *digest);
            }
        }));
    }
    // Room for requests past the limit to reach the server, were they allowed to.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.requests.load(Ordering::SeqCst),
        limit,
        "requests at once"
    );
    assert_eq!(engine.stats().requests_in_flight, limit);
    fake.open_finds();
    fake.open();
    for lease in leases {
        lease.await.expect("lease");
    }
    assert_eq!(
        fake.most_requests.load(Ordering::SeqCst),
        limit,
        "requests at once"
    );
    assert_eq!(lock(&fake.writes).len(), 2 * 16 * 20, "writes");
    assert!(
        fake.most.load(Ordering::SeqCst) >= 1,
        "no read reached the server"
    );
    let finds = lock(&fake.finds).clone();
    assert!(
        finds.len() >= 2 * 16 * 20 / 4,
        "FindMissing calls: {finds:?}"
    );
    assert_eq!(engine.stats().requests_in_flight, 0);
}

/// Issue #641. One lease uploads 64 blobs (half `put`, half `put_chunks`, as the
/// fallback of a failed segment upload sends them) to a server that holds every write,
/// as a slow store holds it for minutes; then another lease reads 40 blobs. The writes
/// must hold no more than the uploads' share of `--transfer-in-flight` (12 of 16) and
/// the reads must finish while the writes are still held. Before the fix the writes
/// took all 16 permits and the reads waited for them: on the farm node, nine leases'
/// input fills waited 22 minutes for one lease's upload. Mutants seen red: no share
/// taken by `put` or by `put_chunks` (held writes 16, reads stuck), a share of the
/// whole limit (the same).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_uploads_leave_permits_for_another_leases_reads() {
    let fake = Arc::new(Fake::default());
    let limit = 16;
    let config = TransferConfig {
        in_flight: limit,
        ..TransferConfig::default()
    };
    let engine = engine(fake.serve().await, config);
    let reads: Vec<Digest> = (0..40)
        .map(|i| fake.insert(patterned(30_000 + i, 90)))
        .collect();
    fake.close_writes();
    let uploader = Arc::new(engine.scope(&LeaseId::new(7, 0)));
    let blobs: Vec<Vec<u8>> = (0..64).map(|i| patterned(31_000 + i, 60)).collect();
    let upload = {
        let (uploader, blobs) = (Arc::clone(&uploader), blobs.clone());
        tokio::spawn(async move {
            let calls = blobs.iter().enumerate().map(|(i, bytes)| {
                if i % 2 == 0 {
                    Either::Left(uploader.put(bytes.clone()))
                } else {
                    Either::Right(uploader.put_chunks(digest_of(bytes), chunks_of(bytes)))
                }
            });
            futures::future::join_all(calls).await
        })
    };
    let share = 12;
    within(30, || fake.held_writes.load(Ordering::SeqCst) >= share).await;
    // Room for writes past the share to reach the server, were they allowed to.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reader = engine.scope(&LeaseId::new(7, 1));
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        futures::future::join_all(reads.iter().map(|d| reader.get(d))),
    )
    .await;
    assert_eq!(
        (read.is_ok(), fake.held_writes.load(Ordering::SeqCst)),
        (true, share),
        "(reads finished, writes held) while the server holds every write"
    );
    for (digest, got) in reads.iter().zip(read.unwrap_or_default()) {
        assert_eq!(digest_of(&got.expect("read")), *digest);
    }
    fake.open_writes();
    let stored = upload.await.expect("uploads");
    for (bytes, stored) in blobs.iter().zip(stored) {
        assert_eq!(stored, Ok(digest_of(bytes)));
    }
    assert_eq!(lock(&fake.writes).len(), 64, "writes");
    assert_eq!(engine.stats().requests_in_flight, 0);
}

/// `bytes` as a stream of chunks of at most 16 bytes.
fn chunks_of(bytes: &[u8]) -> Chunks {
    let pieces: Vec<std::io::Result<Vec<u8>>> = bytes.chunks(16).map(|c| Ok(c.to_vec())).collect();
    Box::pin(futures::stream::iter(pieces))
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

/// A lease's counters while its reads are merged with another lease's and its uploads
/// race one: what the `transfers:` line of each lease says it moved. Three leases:
/// - Reads: A starts a read of X, held at the server; B asks for X while it is held
///   and joins A's read. The bytes are A's (`read_calls` 1, `read_bytes` X's length);
///   B has one merged read and no bytes; the server served X once. So the sum of
///   `read_bytes` over leases is what the server sent.
/// - Uploads: A and B store the same new blob Y while the server holds writes, so both
///   ask FindMissing before either lands and both send it: each lease shows one write
///   of Y's bytes, and the server saw two. Then C stores Y, which the server now
///   holds: one upload skipped, nothing written.
/// - A refused `put_chunks` (UNAVAILABLE, not retried) is a write call but no blob
///   written.
/// Mutants seen red (each with the values shown, not a timeout): the shared read
/// counted for no lease (A shows 0, 0, 0); the joining lease credited with the bytes
/// (B shows `read_bytes` 300); `written_blobs` added before the write (the refused one
/// shows 1); C's skip counted as written (C shows 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_lease_counts_its_own_transfers_when_reads_merge_and_uploads_race() {
    let fake = Arc::new(Fake::default());
    let x = patterned(500, 300);
    let x_digest = fake.insert(x.clone());
    let engine = engine(fake.serve().await, TransferConfig::default());
    let a = Arc::new(engine.scope(&LeaseId::new(9, 1)));
    let b = Arc::new(engine.scope(&LeaseId::new(9, 2)));
    let c = engine.scope(&LeaseId::new(9, 3));
    let len = x.len() as u64;

    fake.close();
    let first = tokio::spawn({
        let (a, x_digest) = (Arc::clone(&a), x_digest.clone());
        async move { a.get(&x_digest).await }
    });
    within(30, || fake.active.load(Ordering::SeqCst) == 1).await;
    let joined = tokio::spawn({
        let (b, x_digest) = (Arc::clone(&b), x_digest.clone());
        async move { b.get(&x_digest).await }
    });
    within(30, || b.stats().merged_reads == 1).await;
    fake.open();
    assert_eq!(first.await.expect("A"), Ok(x.clone()));
    assert_eq!(joined.await.expect("B"), Ok(x));
    let reads = |s: &Scope| {
        let t = s.stats();
        (t.read_calls, t.read_bytes, t.merged_reads)
    };
    assert_eq!(reads(&a), (1, len, 0), "the lease that started the read");
    assert_eq!(reads(&b), (0, 0, 1), "the lease that joined it");
    assert_eq!(fake.reads_of(&x_digest), [0], "reads of X at the server");

    let y = patterned(501, 200);
    let y_digest = digest_of(&y);
    fake.close();
    let uploads: Vec<_> = [&a, &b]
        .into_iter()
        .map(|scope| {
            let (scope, y) = (Arc::clone(scope), y.clone());
            tokio::spawn(async move { scope.put(y).await })
        })
        .collect();
    within(30, || fake.held_writes.load(Ordering::SeqCst) == 2).await;
    fake.open();
    for upload in uploads {
        assert_eq!(upload.await.expect("upload"), Ok(y_digest.clone()));
    }
    let writes = |s: &Scope| {
        let t = s.stats();
        (
            t.write_calls,
            t.written_blobs,
            t.written_bytes,
            t.outputs_present,
        )
    };
    let sent = y.len() as u64;
    assert_eq!(writes(&a), (1, 1, sent, 0), "A's upload of Y");
    assert_eq!(writes(&b), (1, 1, sent, 0), "B's upload of Y");
    let name = kbf_transfer::label(&y_digest);
    let y_writes = lock(&fake.writes).iter().filter(|w| **w == name).count();
    assert_eq!(y_writes, 2, "writes of Y at the server");
    assert_eq!(c.put(y).await, Ok(y_digest));
    assert_eq!(writes(&c), (0, 0, 0, 1), "C's upload of Y, already held");

    let z = patterned(502, 100);
    fake.fail_writes.store(1, Ordering::SeqCst);
    assert!(c.put_chunks(digest_of(&z), chunks_of(&z)).await.is_err());
    assert_eq!(writes(&c), (1, 0, 0, 1), "C's refused upload of Z");
}
