//! The front-door proxy in pool mode in front of real `kbf` frontend processes, drained under
//! load (docs/design/kbf-proxy-api.md section 14, "Soak"): 200 long-lived gRPC streams are open
//! through one client connection, spread over two frontends; one frontend is drained through the
//! admin API; every old stream stays open on its frontend and runs to its end there, and every
//! stream opened after the drain is served by the other.
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
//! makes every figure below the child's own and exact. Every action is in the store behind both
//! frontends, so either serves any stream.
//!
//! What this proves, exactly: the old streams end by the client cancelling them, and each
//! cancel reaches the frontend that has the stream (its own count falls); that is not a `Write`
//! or `Read` completing. Both frontends are ready (polled, `/readyz` 200) before the streams
//! start, so each takes a share of them. The drained frontend keeps all the streams it has,
//! open and quiet, and takes no more. The proxy runs here at the library level, so SIGINT and
//! SIGTERM are the verb's own tests (`kbf-installer/tests/proxy_verb.rs`).
//!
//! Why not `ByteStream.Write` or a `Read` nobody reads (the pull request has the detail):
//! a `Write` whose last message is followed by an empty DATA frame with END_STREAM (how the
//! proxy, and every Go gRPC client, ends a request) makes the frontend pass an empty DATA
//! frame on to its storage node, and `h2` 0.4.20 ends a connection that receives more than
//! 100 of them in its life; and `Read`s that stall on flow control share the frontend's one
//! connection to its node, so a few of them starve the rest.

#![cfg(target_os = "linux")]

mod proxy_support;

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::path::Path;

use kbf_proxy::Proxy;
use kbf_proxy::config::Frontend;
use kbf_proxy::control::NoUpdates;
use kbf_testkit::dir::TestDir;
use proxy_support::{
    Child, Open, actions, api_get, connect, free_base, frontend_args, link_kbf, open_executions,
    own_config, proxy_config, proxy_get, storage_node, store, until, until_ready, within,
};
use serde_json::Value;

const LIST: &str = "/v1/proxy/upstreams";

pub fn frontend(dir: &Path, name: &str, node: &Child, base: u16) -> Child {
    let state = dir.join(name).join("state.db");
    Child::start(
        dir,
        name,
        &frontend_args(&node.field("storage="), base, &state),
    )
}

/// Streams opened before the drain and finished after it.
const OLD: usize = 200;
/// Streams opened after the drain: served by the frontend that was not drained.
const NEW: usize = 50;

/// One request to the proxy's admin API: the status code and the JSON body.
fn admin(addr: SocketAddr, method: &str, path: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect to the admin API");
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    let request = format!("{head}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).expect("a status line");
    let body = serde_json::from_str(body).expect("a JSON body");
    (code.parse().expect("a status code"), body)
}

/// The entry of member `name` in the admin API's list.
fn member(list: &Value, name: &str) -> Value {
    let members = list.as_array().expect("a list");
    members
        .iter()
        .find(|m| m["name"] == name)
        .expect("the member is listed")
        .clone()
}

/// The streams the proxy counts on member `name`, from the admin API.
fn proxy_count(admin_addr: SocketAddr, name: &str) -> u64 {
    let (_, list) = admin(admin_addr, "GET", LIST);
    member(&list, name)["open_streams"]["reapi"]
        .as_u64()
        .expect("a count")
}

/// Asserts, with real children, that a drain of one frontend with two hundred streams open
/// leaves the client's one connection alone, resets or fails none of the streams, moves none
/// from the frontend that has it, places the streams that come after on the other frontend
/// only, and that the proxy's counts of the streams are the frontends' own.
#[test]
fn a_drain_with_two_hundred_streams_drops_none_and_the_other_frontend_serves_the_new() {
    // Declared first, so it drops last: the children are reaped before it is removed.
    let scratch = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "proxy-soak");
    let dir = scratch.path().to_path_buf();
    link_kbf(&dir);

    let (node_a, node_b) = (storage_node(&dir, "node-a"), storage_node(&dir, "node-b"));
    let base = free_base();
    let mut a = frontend(&dir, "frontend-a", &node_a, base);
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
    let admin_socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("admin socket");
    let admin_addr = admin_socket.local_addr().expect("addr");
    drop(admin_socket);
    // The proxy terminates TLS: a CA and a server certificate, and the operator token file it
    // reads at start.
    let (mut config, pki) = proxy_config(&dir, base);
    config.frontend.push(Frontend {
        name: "b".to_owned(),
        base_port: base + 10,
    });
    config.admin_listen = Some(admin_addr);
    config.check().expect("a valid pool table");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(public)).expect("bind");
    let serving = std::thread::spawn(move || runner.run(NoUpdates));

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    client_runtime.block_on(async {
        // Every action is in the store behind both frontends.
        let (requests, blobs) = actions("soak", OLD + NEW);
        within("the actions in A", store(&a.field("reapi="), blobs.clone())).await;
        within("the actions in B", store(&b.field("reapi="), blobs)).await;
        let (client, driver) = connect(&public_addr.to_string(), &pki.ca_cert).await;
        // `/readyz` is the proxy's poll of the real frontends: 200 once a poll has passed.
        until_ready(&client).await;
        until("both frontends ready", || {
            let (_, list) = admin(admin_addr, "GET", LIST);
            member(&list, "a")["ready"] == true && member(&list, "b")["ready"] == true
        })
        .await;

        // 200 streams through one connection, each open and idle, spread over both frontends.
        let starting: Vec<_> = requests[..OLD]
            .iter()
            .map(|r| Open::start(&client, r))
            .collect();
        let mut old: Vec<Open> = within("200 old streams", futures::future::join_all(starting))
            .await
            .into_iter()
            .map(|open| open.expect("an old stream starts"))
            .collect();
        let (on_a, on_b) = (open_executions(&api_a), open_executions(&api_b));
        assert_eq!(on_a + on_b, OLD as u64, "the frontends' own counts");
        assert_ne!(on_a, 0, "A took a share");
        assert_ne!(on_b, 0, "B took a share");
        assert_eq!(proxy_count(admin_addr, "a"), on_a);
        assert_eq!(proxy_count(admin_addr, "b"), on_b);

        // The drain of A: it lists A as draining with its streams counted.
        let (code, list) = admin(admin_addr, "POST", &format!("{LIST}/a:drain"));
        assert_eq!(code, 200, "{list}");
        assert_eq!(member(&list, "a")["state"], "draining");
        assert_eq!(member(&list, "a")["open_streams"]["reapi"], on_a);
        assert!(
            !driver.is_finished(),
            "the drain left the client's connection alone"
        );

        // The streams after the drain are all B's.
        let mut new = Vec::new();
        for request in &requests[OLD..] {
            let open = Open::start(&client, request).await;
            new.push(open.expect("a new stream is served by B"));
        }
        assert_eq!(
            open_executions(&api_a),
            on_a,
            "A kept its own, and took no more"
        );
        assert_eq!(open_executions(&api_b), on_b + NEW as u64, "B's own count");

        // Every stream is still open and quiet: no GOAWAY, no reset, on either side.
        for open in old.iter_mut().chain(new.iter_mut()) {
            assert!(open.quiet().await, "a stream ended or spoke");
        }

        // The old streams end (the client cancels them, as Buck2 does) and each frontend sees
        // its own end.
        drop(old);
        until("the old executions end", || {
            open_executions(&api_a) == 0 && open_executions(&api_b) == NEW as u64
        })
        .await;
        until("the proxy's count of A falls to 0", || {
            proxy_count(admin_addr, "a") == 0
        })
        .await;
        until("the proxy's count of B", || {
            proxy_count(admin_addr, "b") == NEW as u64
        })
        .await;

        // The proxy dropped, refused and retried nothing.
        let (code, metrics) = proxy_get(&client, "/metrics").await;
        assert_eq!(code, 200);
        for zero in [
            "kbf_proxy_panics_total",
            "kbf_proxy_streams_refused_total",
            "kbf_proxy_upstream_dial_failed_total",
            "kbf_proxy_retries_total",
            "kbf_proxy_unavailable_total",
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
    });
    handle.shutdown();
    serving.join().expect("the runner thread").expect("run");
    assert!(a.stop().is_some_and(|status| status.success()), "A exits 0");
    assert_eq!(b.stop().and_then(|s| s.code()), Some(0), "B exits 0");
    drop((node_a, node_b));
}

/// Catches a child started with no `--config` of its own: it would read /etc/kbf/kbf.toml
/// on a host that has one, and the soak would then pass or fail by the host it runs on.
#[test]
fn every_child_is_started_with_a_config_of_the_tests_own() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "soak-config");
    let own = dir.join("kbf.toml");
    let argv = own_config(&dir, &["storage".to_owned(), "--x=1".to_owned()]);
    assert_eq!(
        argv,
        [
            "storage".to_owned(),
            format!("--config={}", own.display()),
            "--x=1".to_owned()
        ]
    );
    assert_eq!(std::fs::read(&own).expect("the file"), b"");
    let given = ["storage".to_owned(), "--config=/elsewhere".to_owned()];
    assert_eq!(own_config(&dir, &given), given, "a --config given is kept");
}
