//! Pool mode end to end: the serving core over fake members on an in-memory network, the
//! admin API over in-memory connections, and `Proxy::bind` and `run` on real sockets. They run
//! on a current-thread runtime under paused time, as the other tests of the core do, except
//! the ones on real sockets.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use bytes::Bytes;
use h2::Reason;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use super::kit::{
    CAPS, CLIENT_IP, Child, Client, MemNet, Script, failing_listener, finish, grpc_ok, read_all,
    reserve_ports, script, security, start_child_on, tagged, until,
};
use crate::auth::Buck2Auth;
use crate::config::{Frontend, Limits, ProxyConfig};
use crate::control::NoUpdates;
use crate::core::{Core, Mode};
use crate::metrics::Metrics;
use crate::pool::{BoxIo, member_addr};
use crate::pool_admin;
use crate::pool_probe;
use crate::route::Target;
use crate::serve::{Listen, serve};
use crate::slot::ActiveSlot;
use crate::tls::Acceptor;
use crate::upstreams::{State, Upstreams};
use crate::{Proxy, ProxyHandle};

const WAIT_EXECUTION: &str = "/build.bazel.remote.execution.v2.Execution/WaitExecution";
const LIST: &str = "/v1/proxy/upstreams";

const TARGETS: [Target; 4] = [
    Target::Reapi,
    Target::Worker,
    Target::Operator,
    Target::Enroll,
];

type Sender = mpsc::UnboundedSender<(DuplexStream, IpAddr)>;

/// A listener fed by the tests: the proxy's public one, or the admin API's.
struct Feed {
    rx: mpsc::UnboundedReceiver<(DuplexStream, IpAddr)>,
}

impl Feed {
    fn pair() -> (Sender, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Self { rx })
    }
}

impl Listen for Feed {
    async fn accept(&mut self) -> io::Result<(BoxIo, IpAddr)> {
        let (io, peer) = self
            .rx
            .recv()
            .await
            .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionAborted))?;
        Ok((Box::new(io), peer))
    }
}

fn frontend(name: &str, base_port: u16) -> Frontend {
    Frontend {
        name: name.to_owned(),
        base_port,
    }
}

/// The frontends `a`, `b` and `c`, the first `n` of them, ten ports apart.
fn frontends(n: usize) -> Vec<Frontend> {
    let names = ["a", "b", "c"];
    let one = |i: usize| frontend(names[i], 18_000 + 10 * u16::try_from(i).expect("a few"));
    (0..n).map(one).collect()
}

/// The serving core in pool mode over one fake child per script (members `a`, `b`, `c`).
struct PoolRig {
    core: Arc<Core>,
    members: Arc<Upstreams>,
    net: Arc<MemNet>,
    children: Vec<Child>,
    feed: Sender,
    // Kept: the serve task stops when its sender is gone.
    _stop: watch::Sender<bool>,
}

impl PoolRig {
    fn start(scripts: &[Script]) -> Self {
        let members = Upstreams::new(&frontends(scripts.len()));
        let net = Arc::new(MemNet::default());
        let core = Core::with_metrics(
            Mode::Pool(Arc::clone(&members)),
            Limits::default(),
            ActiveSlot::new(),
            net.connector(),
            security(Buck2Auth::None),
            Arc::new(Metrics::default()),
            true,
        );
        let (feed, incoming) = Feed::pair();
        let (stop, stopped) = watch::channel(false);
        let accepting = Acceptor::Plain(None);
        tokio::spawn(serve(
            Arc::clone(&core),
            NoUpdates,
            incoming,
            accepting,
            stopped,
        ));
        let mut children = Vec::new();
        for (index, one) in scripts.iter().enumerate() {
            let base = members.base(index);
            let addrs = TARGETS.map(|target| member_addr(base, target)).to_vec();
            children.push(Child::start(&net, addrs, one));
        }
        Self {
            core,
            members,
            net,
            children,
            feed,
            _stop: stop,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.feed, CLIENT_IP).await
    }

    /// The streams each child has been given.
    fn dispatches(&self) -> Vec<usize> {
        let count = |child: &Child| child.state.dispatches();
        self.children.iter().map(count).collect()
    }

    /// Starts the poll of every member.
    fn probing(&self) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        for index in 0..self.members.len() {
            let (core, members) = (Arc::clone(&self.core), Arc::clone(&self.members));
            tasks.push(tokio::spawn(pool_probe::poll(core, members, index)));
        }
        tasks
    }
}

fn answering(code: u16) -> Arc<AtomicU16> {
    Arc::new(AtomicU16::new(code))
}

/// Reads one request head and answers it with the status in `status`.
async fn answer(mut io: impl AsyncRead + AsyncWrite + Unpin, status: Arc<AtomicU16>) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if io.read_exact(&mut byte).await.is_err() {
            return;
        }
        head.push(byte[0]);
    }
    let code = status.load(Ordering::Acquire);
    let reply = format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = io.write_all(reply.as_bytes()).await;
    let _ = io.shutdown().await;
}

/// Puts an HTTP/1.1 answerer where member `index`'s operator listener was.
fn operator(rig: &PoolRig, index: usize, status: &Arc<AtomicU16>) -> JoinHandle<()> {
    let addr = member_addr(rig.members.base(index), Target::Operator);
    let mut incoming = rig.net.listen(addr);
    let status = Arc::clone(status);
    tokio::spawn(async move {
        while let Some(io) = incoming.recv().await {
            tokio::spawn(answer(io, Arc::clone(&status)));
        }
    })
}

async fn wait_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn readyz(client: &mut Client) -> u16 {
    let reply = client.plain("GET", "/readyz", b"").await;
    reply.status.expect("an answer")
}

/// A child that reads the request, waits for a permit of `release`, and answers `tag`.
fn held_until(release: &Arc<Semaphore>, tag: &'static str) -> Script {
    let release = Arc::clone(release);
    script(move |mut call| {
        let release = Arc::clone(&release);
        async move {
            read_all(&mut call.body).await;
            release.acquire().await.expect("open").forget();
            grpc_ok(&mut call.respond, tag);
        }
    })
}

/// A child that refuses every stream, after reading the whole request.
fn refusing() -> Script {
    script(|mut call| async move {
        read_all(&mut call.body).await;
        call.respond.send_reset(Reason::REFUSED_STREAM);
    })
}

/// Asserts that streams flow to members that no probe has passed yet (all up, none ready),
/// and that two idle members share them: calls that end before the next begins alternate.
#[tokio::test(start_paused = true)]
async fn both_members_take_traffic() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let mut client = rig.client().await;
    let mut tags = Vec::new();
    for _ in 0..4 {
        tags.push(client.unary(CAPS).await.text());
        // The call's place is given back before the next call is placed.
        until(|| rig.members.open_total() == 0).await;
    }
    assert_eq!(tags, ["a", "b", "a", "b"]);
    assert_eq!(rig.dispatches(), [2, 2]);
}

/// Asserts that a stream held open on `a` sends the calls that come meanwhile to `b`, which
/// has fewer open, and that the held stream is answered by `a` when it is let go.
#[tokio::test(start_paused = true)]
async fn long_held_streams_on_a_send_new_calls_to_b() {
    let release = Arc::new(Semaphore::new(0));
    let rig = PoolRig::start(&[held_until(&release, "a"), tagged("b")]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let held = tokio::spawn(finish(response));
    until(|| rig.dispatches()[0] == 1).await;
    for _ in 0..5 {
        assert_eq!(client.unary(CAPS).await.text(), "b");
        // The call's place on `b` is given back before the next call is placed: with one
        // stream open on each, the tie would go to the member the pick starts at.
        until(|| rig.members.snapshot()[1].open_streams.reapi == 0).await;
    }
    assert_eq!(rig.dispatches(), [1, 5]);
    release.add_permits(1);
    let reply = held.await.expect("client");
    assert_eq!(reply.text(), "a");
}

/// Asserts that after `a` is drained new calls reach only `b`, that the stream open on `a`
/// goes on and finishes with its answer and no reset, and that `a`'s open streams then fall
/// to 0.
#[tokio::test(start_paused = true)]
async fn draining_a_member_leaves_its_open_stream_to_finish() {
    let release = Arc::new(Semaphore::new(0));
    let rig = PoolRig::start(&[held_until(&release, "a"), tagged("b")]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let open = tokio::spawn(finish(response));
    until(|| rig.dispatches()[0] == 1).await;

    assert!(rig.members.set_draining("a", true));
    for _ in 0..4 {
        assert_eq!(client.unary(CAPS).await.text(), "b");
    }
    assert_eq!(rig.dispatches(), [1, 4], "a took nothing more");
    let view = rig.members.snapshot();
    assert_eq!(view[0].state, State::Draining);
    assert_eq!(view[0].open_streams.reapi, 1);

    release.add_permits(1);
    let reply = open.await.expect("client");
    assert!(reply.error.is_none(), "no reset: {:?}", reply.error);
    assert_eq!(reply.text(), "a");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    until(|| rig.members.snapshot()[0].open_streams.reapi == 0).await;
}

/// Asserts that undraining a member puts it back among the members a call can reach.
#[tokio::test(start_paused = true)]
async fn undraining_restores_a_member() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let mut client = rig.client().await;
    assert!(rig.members.set_draining("a", true));
    for _ in 0..3 {
        assert_eq!(client.unary(CAPS).await.text(), "b");
    }
    assert_eq!(rig.dispatches(), [0, 3]);
    assert!(rig.members.set_draining("a", false));
    let mut tags = vec![client.unary(CAPS).await.text()];
    tags.push(client.unary(CAPS).await.text());
    tags.sort();
    assert_eq!(tags, ["a", "b"]);
}

/// Asserts that when every member is draining a call is answered `UNAVAILABLE` by the proxy,
/// nothing reaches a child, and calls go through again once a member is undrained.
#[tokio::test(start_paused = true)]
async fn all_members_draining_is_unavailable() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let mut client = rig.client().await;
    assert!(rig.members.set_draining("a", true));
    assert!(rig.members.set_draining("b", true));
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert_eq!(rig.dispatches(), [0, 0]);
    assert_eq!(rig.core.metrics.unavailable.load(Ordering::Acquire), 1);
    assert!(rig.members.set_draining("b", false));
    assert_eq!(client.unary(CAPS).await.text(), "b");
}

/// Asserts that a member that refuses the stream (REFUSED_STREAM) is not asked again for it:
/// the request is placed on the other member with its whole body, once, and the failure is
/// counted against the member that refused.
#[tokio::test(start_paused = true)]
async fn a_refusing_member_has_the_request_retried_on_the_other() {
    let rig = PoolRig::start(&[refusing(), tagged("b")]);
    let mut client = rig.client().await;
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.text(), "b");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert_eq!(rig.dispatches(), [1, 1]);
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    let line = "kbf_proxy_member_failed_total{member=\"a\"} 1\n";
    assert!(rig.members.render().contains(line));
    until(|| rig.core.slots.open_streams() == 0).await;
}

/// Asserts that when both members refuse, the stream is tried on each once and then answered
/// `UNAVAILABLE`, and no member is tried twice.
#[tokio::test(start_paused = true)]
async fn when_every_member_refuses_the_stream_is_unavailable() {
    let rig = PoolRig::start(&[refusing(), refusing()]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert_eq!(rig.dispatches(), [1, 1]);
}

/// Asserts that with one member only, the member that refused is not tried again and the
/// stream is answered `UNAVAILABLE` at once.
#[tokio::test(start_paused = true)]
async fn the_only_member_that_refused_is_not_tried_again() {
    let rig = PoolRig::start(&[refusing()]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert_eq!(rig.dispatches(), [1]);
}

/// Asserts that a member whose connection ends the stream with a GOAWAY that proves it
/// unprocessed is not asked again for it either: the other member answers.
#[tokio::test(start_paused = true)]
async fn a_member_that_goes_away_has_the_request_retried_on_the_other() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let first = &rig.children[0].state;
    first.raw_goaway_first.store(true, Ordering::Release);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.text(), "b");
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
}

/// Asserts that a member nothing can connect to is made not ready by the failure, that the
/// request is placed on the other member, and that the next calls do not try it again: only
/// `b` is dialed after the one failed dial.
#[tokio::test(start_paused = true)]
async fn a_member_that_cannot_be_connected_to_is_ejected() {
    let mut rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    rig.children[0].stop_accepting().await;
    rig.members.set_ready(0, true);
    rig.members.set_ready(1, true);
    let mut client = rig.client().await;
    // Headers only, so the request is whole when the connection fails and can be placed again.
    let (response, _body) = client.start(CAPS, true).await;
    assert_eq!(finish(response).await.text(), "b");
    let ready: Vec<bool> = rig.members.snapshot().iter().map(|m| m.ready).collect();
    assert_eq!(ready, [false, true]);
    let dials = || rig.core.metrics.upstream_dials.load(Ordering::Acquire);
    assert_eq!(dials(), 2, "one dial that failed, one that did not");
    for _ in 0..2 {
        assert_eq!(client.unary(CAPS).await.text(), "b");
    }
    assert_eq!(dials(), 2, "a is not dialed again");
}

/// Asserts that `/readyz` is 200 while some member is up and its poll passes, and 503 only
/// when no member is ready: one member failing its poll changes nothing, both failing is 503,
/// a draining member does not count, and a member that passes again makes it 200.
#[tokio::test(start_paused = true)]
async fn readyz_is_503_only_when_no_member_is_ready() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let (a, b) = (answering(200), answering(200));
    let _listeners = (operator(&rig, 0, &a), operator(&rig, 1, &b));
    let mut client = rig.client().await;
    assert_eq!(readyz(&mut client).await, 503, "no poll has run");
    let _polls = rig.probing();
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);

    a.store(503, Ordering::Release);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200, "b is ready");
    b.store(503, Ordering::Release);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 503, "neither is");

    a.store(200, Ordering::Release);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);
    assert!(rig.members.set_draining("a", true));
    assert_eq!(readyz(&mut client).await, 503, "ready member drains");
    assert!(rig.members.set_draining("a", false));
    assert_eq!(readyz(&mut client).await, 200);
}

/// Asserts that a ready member is preferred: with `a` not ready and `b` ready, calls go to
/// `b`, and when a poll passes for `a` the two share them again.
#[tokio::test(start_paused = true)]
async fn calls_prefer_the_ready_member_and_share_when_both_are() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let (a, b) = (answering(503), answering(200));
    let _listeners = (operator(&rig, 0, &a), operator(&rig, 1, &b));
    let _polls = rig.probing();
    wait_ms(1_100).await;
    let mut client = rig.client().await;
    for _ in 0..3 {
        assert_eq!(client.unary(CAPS).await.text(), "b");
    }
    a.store(200, Ordering::Release);
    wait_ms(1_100).await;
    let mut tags = vec![client.unary(CAPS).await.text()];
    tags.push(client.unary(CAPS).await.text());
    tags.sort();
    assert_eq!(tags, ["a", "b"]);
}

/// Asserts that a pool does not read a `WaitExecution` for the term of an old child: a call
/// that sends its headers and no message is placed on a member at once, not after the time a
/// slot-mode proxy waits for the message, and a whole one is placed like any other.
#[tokio::test(start_paused = true)]
async fn a_wait_execution_is_placed_like_any_call() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let mut client = rig.client().await;
    let began = tokio::time::Instant::now();
    let (_response, _body) = client.start(WAIT_EXECUTION, false).await;
    until(|| rig.dispatches()[0] == 1).await;
    assert!(began.elapsed() < crate::stream::PEEK_FOR);
    assert_eq!(client.unary(WAIT_EXECUTION).await.text(), "b");
}

/// Asserts that `GET /metrics` carries the per-member series (state, readiness, streams open,
/// streams placed) and, with no one child to ask, appends no child series and counts no
/// failure to get them.
#[tokio::test(start_paused = true)]
async fn the_metrics_have_a_series_for_each_member() {
    let rig = PoolRig::start(&[tagged("a"), tagged("b")]);
    let mut client = rig.client().await;
    assert_eq!(client.unary(CAPS).await.text(), "a");
    assert!(rig.members.set_draining("b", true));
    let reply = client.plain("GET", "/metrics", b"").await;
    assert_eq!(reply.status, Some(200));
    let text = reply.text();
    for line in [
        "kbf_proxy_member_up{member=\"a\"} 1\n",
        "kbf_proxy_member_up{member=\"b\"} 0\n",
        "kbf_proxy_member_picked_total{member=\"a\"} 1\n",
        "kbf_proxy_member_open_streams{member=\"b\",target=\"reapi\"} 0\n",
        "kbf_proxy_child_metrics_failed_total 0\n",
    ] {
        assert!(text.contains(line), "{line:?} is not in:\n{text}");
    }
}

/// One HTTP/1.1 request on `io`, answered with the connection closed: the whole response.
async fn exchange(io: impl AsyncRead + AsyncWrite + Unpin, method: &str, path: &str) -> String {
    exchange_with(io, method, path, ("127.0.0.1:18090", "")).await
}

/// [`exchange`] with the `Host` value and the header lines (each ends in CRLF) in `head`.
async fn exchange_with(
    mut io: impl AsyncRead + AsyncWrite + Unpin,
    method: &str,
    path: &str,
    head: (&str, &str),
) -> String {
    let (host, extra) = head;
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra}");
    let request = format!("{head}Connection: close\r\n\r\n");
    io.write_all(request.as_bytes()).await.expect("send");
    let mut text = String::new();
    io.read_to_string(&mut text).await.expect("the response");
    text
}

/// The status code of a whole response.
fn code_of(response: &str) -> u16 {
    let code = response.split_whitespace().nth(1).expect("a status line");
    code.parse().expect("a number")
}

/// Whether a list of members in `response` shows `name` in `state`.
fn shows(response: &str, name: &str, state: &str) -> bool {
    response.contains(&format!("\"name\":\"{name}\",\"state\":\"{state}\""))
}

/// One request to the admin API over a new in-memory connection.
async fn admin(feed: &Sender, method: &str, path: &str) -> String {
    let (near, far) = tokio::io::duplex(1 << 16);
    feed.send((far, CLIENT_IP)).expect("the admin API listens");
    exchange(near, method, path).await
}

/// Asserts that the admin API answers over a connection as it does in-process: the list, a
/// drain and an undrain that change the members and answer the list, an unknown member
/// (404), a path it has not (404) and a wrong method (405 naming the right one).
#[tokio::test(start_paused = true)]
async fn the_admin_api_lists_drains_and_undrains_over_a_connection() {
    let members = Upstreams::new(&frontends(2));
    let (feed, incoming) = Feed::pair();
    let serving = tokio::spawn(pool_admin::serve(
        incoming,
        Arc::clone(&members),
        Limits::default(),
    ));

    let list = admin(&feed, "GET", LIST).await;
    assert_eq!(code_of(&list), 200, "{list}");
    assert!(list.contains("content-type: application/json"), "{list}");
    assert!(shows(&list, "a", "up"), "{list}");

    let drained = admin(&feed, "POST", &format!("{LIST}/a:drain")).await;
    assert_eq!(code_of(&drained), 200, "{drained}");
    assert!(shows(&drained, "a", "draining"), "{drained}");
    let states: Vec<State> = members.snapshot().iter().map(|m| m.state).collect();
    assert_eq!(states, [State::Draining, State::Up]);

    let undrained = admin(&feed, "POST", &format!("{LIST}/a:undrain")).await;
    assert_eq!(code_of(&undrained), 200, "{undrained}");
    assert!(shows(&undrained, "a", "up"), "{undrained}");

    let unknown = admin(&feed, "POST", &format!("{LIST}/zzz:drain")).await;
    assert_eq!(code_of(&unknown), 404, "{unknown}");
    assert_eq!(code_of(&admin(&feed, "GET", "/v1/other").await), 404);
    let wrong = admin(&feed, "GET", &format!("{LIST}/a:drain")).await;
    assert_eq!(code_of(&wrong), 405, "{wrong}");
    assert!(wrong.contains("allow: POST"), "{wrong}");
    serving.abort();
}

/// Asserts that over a connection the admin API answers a request that has an `Origin`
/// header, or a `Host` that is a name, with a 403, and drains nothing.
#[tokio::test(start_paused = true)]
async fn the_admin_api_refuses_a_request_a_web_page_could_send() {
    let members = Upstreams::new(&frontends(2));
    let (feed, incoming) = Feed::pair();
    let serving = tokio::spawn(pool_admin::serve(
        incoming,
        Arc::clone(&members),
        Limits::default(),
    ));
    let drain = format!("{LIST}/a:drain");
    let (near, far) = tokio::io::duplex(1 << 16);
    feed.send((far, CLIENT_IP)).expect("the admin API listens");
    let page = ("127.0.0.1:18090", "Origin: http://example.test\r\n");
    let from_a_page = exchange_with(near, "POST", &drain, page).await;
    assert_eq!(code_of(&from_a_page), 403, "{from_a_page}");

    let (near, far) = tokio::io::duplex(1 << 16);
    feed.send((far, CLIENT_IP)).expect("the admin API listens");
    let rebound = exchange_with(near, "POST", &drain, ("rebind.test", "")).await;
    assert_eq!(code_of(&rebound), 403, "{rebound}");

    let states: Vec<State> = members.snapshot().iter().map(|m| m.state).collect();
    assert_eq!(states, [State::Up, State::Up]);
    serving.abort();
}

/// Asserts that the admin API's accept loop backs off when accepting fails, and does not
/// spin or end.
#[tokio::test(start_paused = true)]
async fn the_admin_api_backs_off_when_accept_fails() {
    let members = Upstreams::new(&frontends(1));
    let serving = tokio::spawn(pool_admin::serve(
        failing_listener(),
        members,
        Limits::default(),
    ));
    // A busy loop would never let this virtual second pass; a backed-off one does.
    wait_ms(1_000).await;
    assert!(!serving.is_finished());
    serving.abort();
}

/// A free loopback address: bound and let go.
fn free_address() -> SocketAddr {
    let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    socket.local_addr().expect("addr")
}

/// The config of a files' proxy in pool mode: frontends `a` at `base` and `b` ten above, and
/// the admin API at `admin`.
fn pooled(mut config: ProxyConfig, base: u16, admin: Option<SocketAddr>) -> ProxyConfig {
    config.upstream = None;
    config.frontend = vec![frontend("a", base), frontend("b", base + 10)];
    config.admin_listen = admin;
    config.check().expect("a valid pool table");
    config
}

/// Starts the proxy of `config` on a thread of its own, on a public socket it is handed.
fn run_proxy(config: ProxyConfig) -> (ProxyHandle, SocketAddr, std::thread::JoinHandle<()>) {
    let public = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let public_addr = public.local_addr().expect("addr");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(public)).expect("bind");
    let serving = std::thread::spawn(move || runner.run(NoUpdates).expect("run"));
    (handle, public_addr, serving)
}

/// Serves `listener` with the operator answers of `status`.
fn serve_operator(listener: std::net::TcpListener, status: &Arc<AtomicU16>) {
    listener.set_nonblocking(true).expect("non-blocking");
    let listener = TcpListener::from_std(listener).expect("listener");
    let status = Arc::clone(status);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(answer(tcp, Arc::clone(&status)));
        }
    });
}

/// One request to the admin API on its real socket.
async fn admin_tcp(addr: SocketAddr, method: &str, path: &str) -> String {
    let tcp = TcpStream::connect(addr).await.expect("the admin API");
    exchange(tcp, method, path).await
}

/// Whether the admin API at `addr` shows both members ready.
async fn both_ready(addr: SocketAddr) -> bool {
    let list = admin_tcp(addr, "GET", LIST).await;
    list.matches("\"ready\":true").count() == 2
}

/// Asserts that `Proxy::bind` and `run` serve a pool on real sockets: there is no startup
/// hold, the members' polls make `/readyz` 200, calls reach the members, and a drain asked of
/// the admin API on its own loopback socket sends the next calls to the other member and shows
/// in the metrics of the public port.
#[test]
fn run_serves_a_pool_on_real_sockets() {
    let (base, listeners) = reserve_ports();
    let admin_addr = free_address();
    let mut files = super::bypass::files("pool-real", base, "");
    files.config = pooled(files.config, base, Some(admin_addr));
    let (handle, public, serving) = run_proxy(files.config.clone());
    assert!(!handle.slots().holding(), "a pool starts with no hold");
    assert!(handle.startup_hold().is_none());

    handle.runtime().block_on(async {
        // The sockets are `a`'s REAPI, worker and operator ports, then `b`'s: the first two of
        // each speak h2, the operator port answers the polls.
        let mut sockets = listeners.into_iter();
        let mut next = || sockets.next().expect("six sockets");
        let (a_reapi, a_worker, a_operator) = (next(), next(), next());
        let (b_reapi, b_worker, b_operator) = (next(), next(), next());
        let _a = start_child_on(vec![a_reapi, a_worker], tagged("a"));
        let _b = start_child_on(vec![b_reapi, b_worker], tagged("b"));
        let ok = answering(200);
        serve_operator(a_operator, &ok);
        serve_operator(b_operator, &ok);

        let tls = super::pki::client_config(&files.pki.ca_pem, None, &["h2"]);
        let mut client = Client::connect_tcp_tls(public, tls).await;
        let mut ready = false;
        for _ in 0..5_000 {
            ready = both_ready(admin_addr).await;
            if ready {
                break;
            }
            wait_ms(5).await;
        }
        assert!(ready, "the polls never made both members ready");
        assert_eq!(readyz(&mut client).await, 200);

        let mut tags = vec![client.unary(CAPS).await.text()];
        tags.push(client.unary(CAPS).await.text());
        tags.sort();
        assert_eq!(tags, ["a", "b"]);

        let drained = admin_tcp(admin_addr, "POST", &format!("{LIST}/a:drain")).await;
        assert_eq!(code_of(&drained), 200, "{drained}");
        for _ in 0..3 {
            assert_eq!(client.unary(CAPS).await.text(), "b");
        }
        let metrics = client.plain("GET", "/metrics", b"").await.text();
        let line = "kbf_proxy_member_up{member=\"a\"} 0\n";
        assert!(metrics.contains(line), "{metrics}");
        for _ in 0..5_000 {
            if handle.open_streams() == 0 {
                break;
            }
            wait_ms(5).await;
        }
        assert_eq!(handle.open_streams(), 0);
    });
    handle.shutdown();
    serving.join().expect("the runner thread");
}

/// Asserts that a pool without `admin_listen` serves with no admin socket: `/readyz` is 503
/// with no member behind it, and the proxy stops on a shutdown.
#[test]
fn a_pool_without_an_admin_address_serves_and_stops() {
    let files = super::bypass::files("pool-no-admin", 20_000, "");
    let config = pooled(files.config.clone(), 20_000, None);
    let (handle, public, serving) = run_proxy(config);
    handle.runtime().block_on(async {
        let tls = super::pki::client_config(&files.pki.ca_pem, None, &["h2"]);
        let mut client = Client::connect_tcp_tls(public, tls).await;
        assert_eq!(readyz(&mut client).await, 503, "no member answers");
    });
    handle.shutdown();
    serving.join().expect("the runner thread");
}

/// Asserts that an admin address that is in use is a failed start, and that the start made no
/// secret file before it failed.
#[test]
fn bind_refuses_an_admin_address_that_is_in_use() {
    let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let admin_addr = taken.local_addr().expect("addr");
    let files = super::bypass::files("pool-admin-busy", 20_000, "");
    let config = pooled(files.config.clone(), 20_000, Some(admin_addr));
    let public = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    assert!(Proxy::bind(config, OwnedFd::from(public)).is_err());
    assert!(!files.dir.join("proxy-auth").exists());
    drop(taken);
}
