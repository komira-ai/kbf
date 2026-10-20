//! The routes the proxy answers itself, the update API, limits, the startup hold and the
//! public `Proxy::bind` path.

use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::kit::{CAPS, Rig, finish, grpc_ok, read_all, reserve_ports, script, tagged, until};
use crate::config::{Limits, ProxyConfig, UpstreamTable};
use crate::control::{NoUpdates, UpdateHandler, UpdateReply, UpdateRequest};
use crate::serve::ConnLimits;
use crate::slot::{ActiveSlot, Slot};
use crate::{Proxy, route::Target};

#[tokio::test(start_paused = true)]
async fn probes_and_unknown_paths_are_answered_by_the_proxy() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    let health = client.plain("GET", "/healthz", b"").await;
    assert_eq!((health.status, health.text().as_str()), (Some(200), "ok"));
    let ready = client.plain("GET", "/readyz", b"").await;
    assert_eq!(
        ready.status,
        Some(503),
        "readiness is the poll's, which is not running yet"
    );
    let missing = client.plain("GET", "/nothing", b"").await;
    assert_eq!(
        (missing.status, missing.text().as_str()),
        (Some(404), "not found")
    );
    let rpc = client.unary("/some.Service/Method").await;
    assert_eq!(rpc.status, Some(200));
    assert_eq!(
        rpc.grpc_status().as_deref(),
        Some("12"),
        "UNIMPLEMENTED, never forwarded"
    );
    assert_eq!(rig.a.state.dispatches(), 0);
    assert_eq!(rig.b.state.dispatches(), 0);
}

#[tokio::test(start_paused = true)]
async fn metrics_show_the_gate_and_the_counters() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    let guard = rig
        .core
        .slots
        .hold(Duration::from_secs(20))
        .expect("no hold");
    drop(guard);
    let reply = client.plain("GET", "/metrics", b"").await;
    assert_eq!(reply.status, Some(200));
    assert!(
        reply
            .headers
            .get("content-type")
            .is_some_and(|v| v.as_bytes().starts_with(b"text/plain")),
    );
    let text = reply.text();
    for line in [
        "kbf_proxy_hold_abandoned_total 1",
        "kbf_proxy_held_streams 0",
        "kbf_proxy_panics_total 0",
        "kbf_proxy_slot_open_streams{slot=\"a\"} 0",
        "kbf_proxy_slot_open_streams{slot=\"b\"} 0",
        "# TYPE kbf_proxy_hold_expired_total counter",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
}

/// An update handler that records what it was asked and answers `reply`.
struct Recording {
    seen: Arc<Mutex<Vec<UpdateRequest>>>,
    delay: Duration,
    finished: Arc<AtomicBool>,
    panic: bool,
}

impl UpdateHandler for Recording {
    async fn handle(&self, request: UpdateRequest) -> UpdateReply {
        assert!(!self.panic, "the handler panics");
        self.seen.lock().expect("lock").push(request);
        tokio::time::sleep(self.delay).await;
        self.finished.store(true, Ordering::Release);
        UpdateReply::json(202, b"{\"ok\":true}".to_vec()).with_header(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("7"),
        )
    }
}

fn recording(
    delay: Duration,
    panic: bool,
) -> (Recording, Arc<Mutex<Vec<UpdateRequest>>>, Arc<AtomicBool>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let finished = Arc::new(AtomicBool::new(false));
    let handler = Recording {
        seen: Arc::clone(&seen),
        delay,
        finished: Arc::clone(&finished),
        panic,
    };
    (handler, seen, finished)
}

async fn with_updates(handler: Recording) -> Rig {
    Rig::start_with(tagged("a"), tagged("b"), handler, Limits::default()).await
}

#[tokio::test(start_paused = true)]
async fn the_update_api_calls_the_handler_even_while_the_gate_holds() {
    let (handler, seen, _) = recording(Duration::ZERO, false);
    let rig = with_updates(handler).await;
    let guard = rig
        .core
        .slots
        .hold(Duration::from_secs(20))
        .expect("no hold");
    let reply = rig
        .client()
        .await
        .plain("POST", "/v1/update/frontend/abort?now=1", b"{\"x\":1}")
        .await;
    assert_eq!(reply.status, Some(202));
    assert_eq!(reply.text(), "{\"ok\":true}");
    assert_eq!(reply.headers.get("retry-after").expect("header"), "7");
    assert_eq!(
        reply.headers.get("content-type").expect("type"),
        "application/json"
    );
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(
        seen[0].path, "/v1/update/frontend/abort",
        "the query is not passed"
    );
    assert_eq!(seen[0].body, b"{\"x\":1}");
    drop(guard);
}

#[tokio::test(start_paused = true)]
async fn an_update_body_over_4096_bytes_is_refused_by_the_proxy() {
    static EXACT: [u8; 4096] = [b'a'; 4096];
    static OVER: [u8; 4097] = [b'a'; 4097];
    let (handler, seen, _) = recording(Duration::ZERO, false);
    let rig = with_updates(handler).await;
    let mut client = rig.client().await;
    assert_eq!(
        client
            .plain("POST", "/v1/update/frontend", &EXACT)
            .await
            .status,
        Some(202)
    );
    let over = client.plain("POST", "/v1/update/frontend", &OVER).await;
    assert_eq!(over.status, Some(413));
    assert!(over.text().contains("4096"));
    assert_eq!(
        seen.lock().expect("lock").len(),
        1,
        "the handler never saw the big one"
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_update_handler_gets_a_504_and_still_finishes() {
    let (handler, seen, finished) = recording(Duration::from_secs(45), false);
    let rig = with_updates(handler).await;
    let mut client = rig.client().await;
    let reply = client
        .plain("POST", "/v1/update/frontend/abort", b"{}")
        .await;
    assert_eq!(reply.status, Some(504), "the proxy's own answer at 30 s");
    assert!(!finished.load(Ordering::Acquire));
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert!(
        finished.load(Ordering::Acquire),
        "the handler ran to its end"
    );
    assert_eq!(seen.lock().expect("lock").len(), 1);
    // The call that timed out did not take the connection with it.
    assert_eq!(client.plain("GET", "/healthz", b"").await.status, Some(200));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_stream_is_reset_and_counted_and_the_connection_serves_on() {
    let (handler, _, _) = recording(Duration::ZERO, true);
    let rig = with_updates(handler).await;
    let mut client = rig.client().await;
    let reply = client
        .plain("POST", "/v1/update/frontend/abort", b"{}")
        .await;
    assert!(reply.error.is_some(), "that stream is reset: {reply:?}");
    assert_eq!(rig.core.metrics.panics.load(Ordering::Acquire), 1);
    assert_eq!(client.plain("GET", "/healthz", b"").await.status, Some(200));
    assert!(client.alive());
    let metrics = client.plain("GET", "/metrics", b"").await.text();
    assert!(metrics.contains("kbf_proxy_panics_total 1"));
}

#[tokio::test(start_paused = true)]
async fn the_startup_hold_ends_into_nothing_if_nobody_releases_it() {
    let (slots, guard) = ActiveSlot::starting(Duration::from_secs(20));
    let rig = Rig::build(
        tagged("a"),
        tagged("b"),
        NoUpdates,
        Limits::default(),
        slots,
    )
    .await;
    let abandon = tokio::spawn({
        let core = Arc::clone(&rig.core);
        async move { core.abandon_startup_after(Duration::from_secs(20)).await }
    });
    let mut client = rig.client().await;
    let (response, _body) = client.start(CAPS, true).await;
    let held = tokio::spawn(finish(response));
    until(|| rig.core.queue.held() == 1).await;
    abandon.await.expect("timer");
    assert_eq!(rig.core.slots.abandoned_holds(), 1);
    assert!(!rig.core.slots.holding());
    let reply = held.await.expect("client");
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert!(client.unary(CAPS).await.grpc_status().as_deref() == Some("14"));
    drop(guard);
}

#[tokio::test(start_paused = true)]
async fn a_released_startup_hold_is_left_alone_by_the_timer() {
    let (slots, guard) = ActiveSlot::starting(Duration::from_secs(20));
    let rig = Rig::build(
        tagged("a"),
        tagged("b"),
        NoUpdates,
        Limits::default(),
        slots,
    )
    .await;
    rig.core.slots.adopt(Slot::A, None);
    rig.core.slots.release_into(Slot::A).expect("A is open");
    rig.core
        .abandon_startup_after(Duration::from_secs(20))
        .await;
    assert_eq!(rig.core.slots.abandoned_holds(), 0);
    assert_eq!(rig.core.slots.active(), Some(Slot::A));
    assert_eq!(rig.client().await.unary(CAPS).await.text(), "a");
    drop(guard);
}

#[test]
fn connection_limits_count_per_address_and_in_total() {
    let limits = ConnLimits::new(3, 2);
    let (one, two) = (
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)),
    );
    let first = limits.admit(one).expect("first");
    let second = limits.admit(one).expect("second");
    assert!(limits.admit(one).is_none(), "two per address");
    let other = limits.admit(two).expect("another address has its own two");
    assert!(limits.admit(two).is_none(), "three in total");
    drop(first);
    let again = limits.admit(one).expect("a place was freed");
    assert!(limits.admit(one).is_none());
    drop((second, other, again));
    assert!(limits.admit(two).is_some(), "everything was freed");
}

/// Whether a connection from `ip` is served: an h2 handshake and one probe.
async fn served(rig: &Rig, ip: IpAddr) -> bool {
    let (near, far) = tokio::io::duplex(1 << 16);
    rig.feed_raw(far, ip);
    let Ok((send, connection)) = h2::client::handshake(near).await else {
        return false;
    };
    let driver = tokio::spawn(connection);
    let ready = send.ready().await;
    let served = match ready {
        Ok(mut send) => {
            let request = http::Request::get("http://frontend.test/healthz")
                .body(())
                .expect("req");
            let (response, _) = send.send_request(request, true).expect("send");
            response.await.is_ok()
        }
        Err(_) => false,
    };
    drop(driver);
    served
}

#[tokio::test(start_paused = true)]
async fn a_connection_over_a_limit_is_closed() {
    let limits = Limits {
        max_connections: 2,
        max_connections_per_ip: 1,
        ..Limits::default()
    };
    let rig = Rig::start_with(tagged("a"), tagged("b"), NoUpdates, limits).await;
    let ip = |n| IpAddr::V4(Ipv4Addr::new(192, 0, 2, n));
    let mut first = rig.client_from(ip(1)).await;
    assert_eq!(first.unary(CAPS).await.text(), "a");
    assert!(
        !served(&rig, ip(1)).await,
        "a second connection from one address"
    );
    let second = rig.client_from(ip(2)).await;
    assert!(!served(&rig, ip(3)).await, "a third connection in all");
    assert_eq!(first.unary(CAPS).await.text(), "a", "the first is unharmed");
    assert!(second.alive());
    // A closed connection frees its place.
    drop(second);
    drop(first);
    until(|| rig.core.slots.open_streams() == 0).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(served(&rig, ip(3)).await);
}

#[test]
fn every_slot_and_target_has_its_own_loopback_port() {
    let table = UpstreamTable { base_port: 18_000 };
    let mut seen = std::collections::BTreeSet::new();
    for slot in Slot::ALL {
        for target in [
            Target::Reapi,
            Target::Worker,
            Target::Operator,
            Target::Enroll,
        ] {
            let addr = crate::pool::upstream_addr(table, slot, target);
            assert!(addr.ip().is_loopback(), "{addr}");
            assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
            assert!(seen.insert(addr.port()), "{addr} repeats");
        }
        assert!(seen.insert(table.admin_port(slot)));
    }
    assert_eq!(seen.len(), 10);
}

/// The files of a proxy: certificates, tokens, a secret, a config over them.
pub(super) struct Files {
    pub(super) pki: super::pki::Pki,
    pub(super) config: ProxyConfig,
    pub(super) dir: std::path::PathBuf,
}

pub(super) fn files(name: &str, base: u16, extra: &str) -> Files {
    use super::pki::Pki;
    use crate::scratch::{scratch, write_file};
    let dir = scratch(name);
    let pki = Pki::new("farm CA");
    let paths = pki.write(&dir, &pki.server());
    let token = |file: &str, text: &str| {
        let path = dir.join(file);
        write_file(&path, text.as_bytes(), 0o600);
        path
    };
    let operator = token("operator.token", super::kit::OPERATOR_TOKEN);
    let buck2 = token("buck2.token", super::kit::BUCK2_TOKEN);
    let config = ProxyConfig::from_toml_str(&format!(
        "operator_token_file = {operator:?}\n\
         buck2_token_file = {buck2:?}\n\
         server_cert_file = {:?}\n\
         server_key_file = {:?}\n\
         client_ca_file = {:?}\n\
         proxy_auth_file = {:?}\n\
         hold_max_s = 600\n\
         {extra}\n\
         [upstream]\nbase_port = {base}\n",
        paths.cert,
        paths.key,
        paths.client_ca,
        dir.join("proxy-auth"),
    ))
    .expect("a valid table");
    Files { pki, config, dir }
}

fn listener() -> (std::net::TcpListener, std::net::SocketAddr) {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    (listener, addr)
}

/// `kbf-proxy` through its public entry points, on its own runtime, real sockets and the real
/// clock: TLS in, a certificate's identity to the child, and the bearer mode enforced.
#[test]
fn bind_run_hold_release_and_shutdown() {
    let (base, mut listeners) = reserve_ports();
    let (socket, public) = listener();
    let files = files("bind-run", base, "buck2_auth = \"bearer\"");
    let client = files.pki.client(&["node-3.farm"], Some("daemon"));
    let (handle, runner) = Proxy::bind(files.config.clone(), OwnedFd::from(socket)).expect("bind");
    assert!(handle.slots().holding(), "starts holding, to adopt");
    let serving = std::thread::spawn(move || runner.run(NoUpdates));
    let guard = handle.startup_hold().expect("the startup hold, once");
    assert!(handle.startup_hold().is_none());

    handle.runtime().block_on(async {
        let child = super::kit::start_child_on(listeners.drain(..3).collect(), tagged("real"));
        let tls = super::pki::client_config(&files.pki.ca_pem, Some(&client), &["h2"]);
        let mut client = super::kit::Client::connect_tcp_tls(public, tls).await;
        client.bearer = Some(super::kit::BUCK2_TOKEN.to_owned());
        let (response, mut body) = client.start(CAPS, false).await;
        body.send_data(bytes::Bytes::from_static(b"x"), true)
            .expect("body");
        let waiting = tokio::spawn(finish(response));
        // Held until the supervisor adopts the child and releases.
        eventually(|| handle.snapshot().held == 1).await;
        assert_eq!(handle.open_streams(), 1);
        assert_eq!(handle.expire(), 0, "nothing is past its bound");
        handle.slots().adopt(Slot::A, None);
        handle.slots().release_into(Slot::A).expect("A is open");
        drop(guard);
        assert_eq!(waiting.await.expect("client").text(), "real");
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.active, Some(Slot::A));
        assert!(!snapshot.holding && !snapshot.ready);
        assert_eq!(snapshot.holds_abandoned, 0);
        assert_eq!(child.state.dispatches(), 1);
        let seen = child.state.headers_of(CAPS);
        let node = seen[0]
            .get("x-kbf-node-id")
            .map(http::HeaderValue::as_bytes);
        assert_eq!(
            node,
            Some(&b"node-3.farm"[..]),
            "the certificate's identity"
        );
        let secret = handle.admin_headers();
        assert_eq!(
            seen[0].get("x-kbf-proxy-auth"),
            secret.get("x-kbf-proxy-auth")
        );
        // The bearer mode is enforced on the real path: no key, no service.
        client.bearer = None;
        let refused = client.unary(CAPS).await;
        assert_eq!(refused.grpc_status().as_deref(), Some("16"), "{refused:?}");
        eventually(|| handle.open_streams() == 0).await;
        assert!(client.alive());
    });
    assert!(format!("{handle:?}").contains("ProxyHandle"));
    handle.shutdown();
    handle.shutdown();
    serving.join().expect("the runner thread").expect("run");
}

/// The secret file is created on the first start and read on the next, and the handle says
/// which; the admin headers carry it, and a reload of unchanged files succeeds.
///
/// Kills: a handle that reports `Existing` for a file it wrote; admin calls without the secret.
#[test]
fn bind_reports_where_the_secret_came_from() {
    use crate::identity::SecretOrigin;
    let files = files("bind-origin", 20_000, "");
    let secret_file = files.dir.join("proxy-auth");
    let (socket, _) = listener();
    let (first, _) = Proxy::bind(files.config.clone(), OwnedFd::from(socket)).expect("bind");
    assert_eq!(first.secret_origin(), SecretOrigin::Created);
    let bytes = std::fs::read(&secret_file).expect("the secret file");
    assert_eq!(bytes.len(), 32);
    let admin = first.admin_headers();
    assert_eq!(admin.len(), 1);
    assert_eq!(
        admin.get("x-kbf-proxy-auth").expect("secret"),
        hex::encode(&bytes).as_str()
    );
    first.reload_tls().expect("unchanged files reload");

    let (socket, _) = listener();
    let (second, _) = Proxy::bind(files.config, OwnedFd::from(socket)).expect("bind");
    assert_eq!(second.secret_origin(), SecretOrigin::Existing);
    assert_eq!(second.admin_headers(), admin);
}

/// A proxy does not start on a file it cannot use, and says which.
///
/// Kills: a bind that starts without TLS, without the operator token, or on an open secret;
/// one that creates the secret file before the files that can still refuse it.
#[test]
fn bind_refuses_files_it_cannot_use() {
    use crate::scratch::write_file;
    let bind = |files: &Files| {
        let (socket, _) = listener();
        Proxy::bind(files.config.clone(), OwnedFd::from(socket)).map(|_| ())
    };
    let broken = |name: &str, mode: &str, damage: &dyn Fn(&Files)| {
        let files = files(name, 20_000, &format!("buck2_auth = \"{mode}\""));
        damage(&files);
        let refusal = bind(&files).expect_err(name).to_string();
        // A start that fails leaves no new secret for a child to read and the supervisor to
        // adopt; only the file the test itself wrote is there.
        assert_eq!(files.dir.join("proxy-auth").exists(), name == "open-secret");
        refusal
    };
    let remove =
        |files: &Files, name: &str| std::fs::remove_file(files.dir.join(name)).expect("rm");
    broken("no-operator-token", "none", &|f| {
        remove(f, "operator.token")
    });
    broken("open-operator-token", "none", &|f| {
        write_file(
            &f.dir.join("operator.token"),
            super::kit::OPERATOR_TOKEN.as_bytes(),
            0o644,
        );
    });
    broken("no-buck2-token", "bearer", &|f| remove(f, "buck2.token"));
    broken("no-certificate", "none", &|f| remove(f, "server.pem"));
    broken("no-client-ca", "none", &|f| remove(f, "ca.pem"));
    broken("open-secret", "none", &|f| {
        write_file(&f.dir.join("proxy-auth"), &[1; 32], 0o644);
    });
    // The Buck2 key is read only in the mode that uses it.
    let files = files("buck2-unused", 20_000, "buck2_auth = \"mtls\"");
    remove(&files, "buck2.token");
    bind(&files).expect("mtls reads no Buck2 key");
}

async fn eventually(mut done: impl FnMut() -> bool) {
    for _ in 0..5_000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(done(), "the condition never held");
}

#[tokio::test(start_paused = true)]
async fn shutdown_sends_the_one_goaway_and_lets_a_stream_finish() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        grpc_ok(&mut call.respond, "finished");
    });
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(bytes::Bytes::from_static(b"x"), true)
        .expect("body");
    let open = tokio::spawn(finish(response));
    until(|| rig.a.state.dispatches() == 1).await;
    assert!(client.alive(), "no GOAWAY before the shutdown");
    rig.shutdown();
    let reply = open.await.expect("client");
    assert_eq!(reply.text(), "finished", "the stream in flight completed");
    rig.serving.await.expect("the serve task ends");
    until(|| !client.alive()).await;
}

#[tokio::test(start_paused = true)]
async fn an_accept_that_keeps_failing_backs_off_and_shutdown_still_ends_it() {
    let table = UpstreamTable { base_port: 18_000 };
    let net = Arc::new(super::kit::MemNet::default());
    let core = crate::core::Core::new(
        table,
        Limits::default(),
        ActiveSlot::new(),
        net.connector(),
        super::kit::security(crate::auth::Buck2Auth::None),
    );
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let serving = tokio::spawn(crate::serve::serve(
        core,
        NoUpdates,
        super::kit::failing_listener(),
        crate::tls::Acceptor::Plain(None),
        stopped,
    ));
    // A busy loop would never let this virtual second pass; a backed-off one does.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!serving.is_finished());
    stop.send_replace(true);
    serving.await.expect("the serve task ends");
}

#[tokio::test(start_paused = true)]
async fn a_refusal_of_a_grpc_call_is_a_grpc_response() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    drop(rig.core.slots.hold(Duration::from_secs(20)).expect("hold"));
    // The hold ended into nothing: no frontend is active.
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert_eq!(
        reply
            .headers
            .get("content-type")
            .map(|v| v.to_str().expect("text")),
        Some("application/grpc"),
        "a client that cannot read the answer sees a malformed response, not UNAVAILABLE"
    );
}
