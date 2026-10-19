//! The front-door proxy in front of real `kbf` frontend processes, through a swap
//! (docs/design/kbf-proxy-api.md section 14, "Soak"): 200 long-lived gRPC streams are open
//! through the proxy to the frontend in slot A; the swap holds the gate, makes a second real
//! frontend the active slot (slot B) and releases; every old stream then runs to its end on A,
//! and every stream opened during the hold or after it is served by B.
//!
//! Everything but the proxy library, which runs in this process the way the installer's verb
//! runs it, is a child process: two `kbf storage` nodes (file stores under the scratch
//! directory, one behind each frontend) and two `kbf frontend` processes. The children are
//! this test binary run through a link named `kbf`, which answers as the real binary's own
//! `main` before the harness starts (the same trick as `m1.rs`), so the test needs no other
//! binary built and no service: not Postgres, not MinIO.
//!
//! The streams are `Execution.Execute` calls, the stream a Buck2 build holds open per action:
//! no worker runs them, so each is queued on its frontend and the stream stays open and idle.
//! The frontend counts them itself (`open_executions` of `GET /v1/frontend/drain`), which
//! makes every figure below the child's own and exact. Which child served a stream is read
//! from that count, and from where the action is: each action is in the store behind one
//! frontend only, so a stream the proxy sent to the wrong child would be refused.
//!
//! What this proves, exactly: the old streams end by the client cancelling them, and each
//! cancel reaches A (A's own count falls to 0); that is not a `Write` or `Read` completing.
//! B is ready (polled, `/readyz` 200) before the hold begins, so a release before B is ready
//! is not exercised. `retire(A)` while the 200 streams are open is `Busy(200)` and leaves
//! them open and quiet. The proxy runs here at the library level, so SIGINT/SIGTERM are the
//! verb's own tests (`kbf-installer/tests/proxy_verb.rs`).
//!
//! The proxy has no admin listener to poll (U2 builds it); its `/readyz` poll asks each
//! child's operator listener, which is the `--api-listen` port, so the soak also proves
//! `/readyz` against the real thing: 200 with A active, and 200 again once B is.
//!
//! Why not `ByteStream.Write` or a `Read` nobody reads (the pull request has the detail):
//! a `Write` whose last message is followed by an empty DATA frame with END_STREAM (how the
//! proxy, and every Go gRPC client, ends a request) makes the frontend pass an empty DATA
//! frame on to its storage node, and `h2` 0.4.20 ends a connection that receives more than
//! 100 of them in its life; and `Read`s that stall on flow control share the frontend's one
//! connection to its node, so a few of them starve the rest.

#![cfg(target_os = "linux")]

mod proxy_support;

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_proxy::Proxy;
use kbf_proxy::control::NoUpdates;
use kbf_proxy::slot::{Retire, Slot};
use proxy_support::{
    Child, Open, actions, api_get, connect, frontend_args, free_base, open_executions, proxy_config,
    proxy_get, storage_node, store, until, until_ready, within,
};

pub fn frontend(dir: &Path, name: &str, node: &Child, base: u16) -> Child {
    Child::start(dir, name, &frontend_args(&node.field("storage="), base))
}

/// Streams opened before the swap and finished after it.
const OLD: usize = 200;
/// Streams opened while the gate holds: served by the new child.
const HELD: usize = 20;
/// Streams opened and finished after the swap: served by the new child.
const NEW: usize = 30;

/// Catches, with real children, a proxy that: closes or GOAWAYs the client's one connection
/// at the swap, resets or fails a stream bound before it, moves an old stream to the new
/// child or a new one to the old, loses a held stream, retires the old slot under a stream,
/// or reports `/readyz` 503 beside a ready real child.
#[test]
fn the_flip_with_two_hundred_streams_drops_none_and_each_child_serves_its_own() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("proxy-soak-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).expect("scratch");
    std::os::unix::fs::symlink(
        std::env::current_exe().expect("this binary"),
        dir.join("bin/kbf"),
    )
    .expect("link kbf");

    let (node_a, node_b) = (storage_node(&dir, "node-a"), storage_node(&dir, "node-b"));
    let base = free_base();
    let mut a = frontend(&dir, "frontend-a", &node_a, base);
    // The candidate runs from the start, out of the proxy's table until the swap, so that
    // its store can hold its actions before any stream asks for them.
    let mut b = frontend(&dir, "frontend-b", &node_b, base + 10);
    let (api_a, api_b) = (
        format!("127.0.0.1:{}", base + 2),
        format!("127.0.0.1:{}", base + 12),
    );
    assert_eq!(
        (api_get(&api_a, "/readyz").0, api_get(&api_b, "/readyz").0),
        (200, 200)
    );

    let public = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("public socket");
    let public_addr: SocketAddr = public.local_addr().expect("addr");
    // The proxy terminates TLS (U4b-2): a CA and a server certificate, and the operator
    // token file it reads at start.
    let (config, pki) = proxy_config(&dir, base);
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(public)).expect("bind");
    let serving = std::thread::spawn(move || runner.run(NoUpdates));
    // The supervisor's first act: adopt A, end the startup hold.
    let startup = handle.startup_hold().expect("the startup hold");
    handle.slots().adopt(Slot::A, None);
    handle.slots().release_into(Slot::A).expect("A is open");
    drop(startup);

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    client_runtime.block_on(async {
        // The old actions are in the store behind A only, the new ones behind B only.
        let (old_requests, old_blobs) = actions("old", OLD);
        let (new_requests, new_blobs) = actions("new", HELD + NEW);
        within("the old actions", store(&a.field("reapi="), old_blobs)).await;
        within("the new actions", store(&b.field("reapi="), new_blobs)).await;
        let (client, driver) = connect(&public_addr.to_string(), &pki.ca_cert).await;
        // `/readyz` is the proxy's poll of the real child: 200 once a poll has passed.
        until_ready(&client).await;

        // 200 streams through one connection to A, each open and idle.
        let starting: Vec<_> = old_requests
            .iter()
            .map(|r| Open::start(&client, r))
            .collect();
        let mut old: Vec<Open> = within("200 old streams", futures::future::join_all(starting))
            .await
            .into_iter()
            .map(|open| open.expect("an old stream starts on A"))
            .collect();
        assert_eq!(open_executions(&api_a), OLD as u64, "A's own count");
        assert_eq!(open_executions(&api_b), 0);
        assert_eq!(handle.snapshot().stats(Slot::A).open_streams, OLD as u64);
        assert_eq!(handle.snapshot().active, Some(Slot::A));

        // The swap: hold, make B the active slot, release.
        let hold = handle
            .slots()
            .hold(Duration::from_secs(20))
            .expect("not holding");
        let mut held = Vec::new();
        for request in &new_requests[..HELD] {
            held.push(tokio::spawn({
                let (client, request) = (client.clone(), request.clone());
                async move { Open::start(&client, &request).await }
            }));
        }
        until("the proxy holds the new streams", || {
            handle.snapshot().held == HELD
        })
        .await;
        assert_eq!(open_executions(&api_b), 0, "nothing reaches B while held");
        handle.slots().open(Slot::B);
        handle.slots().release_into(Slot::B).expect("B is open");
        drop(hold);
        assert_eq!(handle.snapshot().active, Some(Slot::B));
        assert!(
            !driver.is_finished(),
            "the flip left the client's connection alone"
        );

        // The held streams and the streams after the swap are all B's.
        let mut new = Vec::new();
        for stream in held {
            let open = within("a held stream", stream).await.expect("task");
            new.push(open.expect("a held stream is served by B"));
        }
        for request in &new_requests[HELD..] {
            let open = Open::start(&client, request).await;
            new.push(open.expect("a new stream is served by B"));
        }
        // And an old action after the swap goes to B, which has not got it: it is refused.
        let refused = Open::start(&client, &old_requests[0]).await;
        assert!(
            refused.is_err(),
            "a new stream for an old action is not served by A"
        );
        assert_eq!(
            open_executions(&api_b),
            (HELD + NEW) as u64,
            "B's own count"
        );
        assert_eq!(
            open_executions(&api_a),
            OLD as u64,
            "A kept its own, and took no more"
        );
        let snapshot = handle.snapshot();
        assert_eq!(
            (
                snapshot.stats(Slot::A).open_streams,
                snapshot.stats(Slot::B).open_streams
            ),
            (OLD as u64, (HELD + NEW) as u64)
        );

        // Every stream is still open and quiet: no GOAWAY, no reset, on either side. A is
        // not retired under its streams (`can_retire` is the supervisor's read-only probe;
        // `retire` is its commit, and it would close the slot's connections).
        for open in old.iter_mut().chain(new.iter_mut()) {
            assert!(open.quiet().await, "a stream ended or spoke");
        }
        assert!(!handle.slots().can_retire(Slot::A));
        // The supervisor's commit with 200 streams open is refused, and refusing latches
        // nothing: every stream is still open and quiet afterwards, and A still counts them.
        assert_eq!(
            handle.slots().retire(Slot::A),
            Retire::Busy(OLD as u64),
            "retire of a slot with open streams"
        );
        for open in old.iter_mut().chain(new.iter_mut()) {
            assert!(
                open.quiet().await,
                "a refused retire ended or woke a stream"
            );
        }
        assert_eq!(open_executions(&api_a), OLD as u64, "A's count after it");
        assert_eq!(handle.snapshot().stats(Slot::A).open_streams, OLD as u64);

        // The old streams end (the client cancels them, as Buck2 does) and A sees each one.
        drop(old);
        until("A's executions end", || open_executions(&api_a) == 0).await;
        assert_eq!(
            open_executions(&api_b),
            (HELD + NEW) as u64,
            "B's are untouched"
        );
        until("A's last permit drops", || {
            handle.slots().can_retire(Slot::A)
        })
        .await;
        assert_eq!(handle.slots().retire(Slot::A), Retire::Closed);
        assert!(a.stop().is_some_and(|status| status.success()), "A exits 0");

        // `/readyz` is B's now, and the proxy dropped, refused and retried nothing.
        until_ready(&client).await;
        let (code, metrics) = proxy_get(&client, "/metrics").await;
        assert_eq!(code, 200);
        for zero in [
            "kbf_proxy_panics_total",
            "kbf_proxy_hold_rejected_total",
            "kbf_proxy_streams_refused_total",
            "kbf_proxy_upstream_dial_failed_total",
            "kbf_proxy_retries_total",
            "kbf_proxy_hold_expired_total",
            "kbf_proxy_hold_abandoned_total",
            "kbf_proxy_held_streams",
        ] {
            assert!(
                metrics.contains(&format!("\n{zero} 0\n")),
                "{zero} is not 0:\n{metrics}"
            );
        }
        assert!(
            !driver.is_finished(),
            "one connection carried every stream, to the end"
        );
        drop(new);
        until("B's executions end", || open_executions(&api_b) == 0).await;
        assert_eq!(b.stop().and_then(|s| s.code()), Some(0), "B exits 0");
    });
    assert_eq!(handle.snapshot().holds_abandoned, 0);
    handle.shutdown();
    serving.join().expect("the runner thread").expect("run");
    drop((node_a, node_b));
}
