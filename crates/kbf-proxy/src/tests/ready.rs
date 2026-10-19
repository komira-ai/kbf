//! `/readyz`: the proxy's poll of the active child is its one writer, and it never feeds back
//! into routing. The fake child's operator listener answers HTTP/1.1, as the real one does;
//! the tests run under paused time, so the poll's seconds are virtual.

use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

use super::kit::{CAPS, Rig, addr_of, finish, grpc_ok, read_all, reserve_ports, script, tagged};
use crate::config::ProxyConfig;
use crate::control::NoUpdates;
use crate::probe::{FRESH_FOR, POLL_TIMEOUT, Readiness, poll};
use crate::route::Target;
use crate::slot::{ActiveSlot, Retire, Slot};
use crate::{Proxy, pool::tcp_connector};

/// What a fake operator listener does with a poll.
struct Ctl {
    status: AtomicU16,
    hang: AtomicBool,
    garbage: AtomicBool,
    /// Milliseconds the answer waits after the request is read.
    delay_ms: AtomicU64,
    asked: AtomicUsize,
    requests: Mutex<Vec<String>>,
}

impl Ctl {
    fn new(status: u16) -> Arc<Self> {
        Arc::new(Self {
            status: AtomicU16::new(status),
            hang: AtomicBool::new(false),
            garbage: AtomicBool::new(false),
            delay_ms: AtomicU64::new(0),
            asked: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn set(&self, status: u16) {
        self.status.store(status, Ordering::Release);
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::Acquire)
    }
}

/// Reads one request head and answers it as `ctl` says.
async fn answer(mut io: impl AsyncRead + AsyncWrite + Unpin, ctl: Arc<Ctl>) {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if io.read_exact(&mut byte).await.is_err() {
            return;
        }
        head.push(byte[0]);
    }
    ctl.asked.fetch_add(1, Ordering::AcqRel);
    ctl.requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(String::from_utf8_lossy(&head).into_owned());
    if ctl.hang.load(Ordering::Acquire) {
        std::future::pending::<()>().await;
    }
    let delay = ctl.delay_ms.load(Ordering::Acquire);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let reply = if ctl.garbage.load(Ordering::Acquire) {
        "this is not http at all".to_owned()
    } else {
        format!(
            "HTTP/1.1 {} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ctl.status.load(Ordering::Acquire)
        )
    };
    let _ = io.write_all(reply.as_bytes()).await;
    let _ = io.shutdown().await;
}

/// Puts an HTTP/1.1 answerer where `slot`'s operator listener was.
fn operator(rig: &Rig, slot: Slot, ctl: &Arc<Ctl>) -> JoinHandle<()> {
    let mut incoming = rig.net.listen(addr_of(rig.table, slot, Target::Operator));
    let ctl = Arc::clone(ctl);
    tokio::spawn(async move {
        while let Some(io) = incoming.recv().await {
            tokio::spawn(answer(io, Arc::clone(&ctl)));
        }
    })
}

fn polling(rig: &Rig) -> JoinHandle<()> {
    tokio::spawn(poll(Arc::clone(&rig.core)))
}

async fn wait_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn readyz(client: &mut super::kit::Client) -> u16 {
    client
        .plain("GET", "/readyz", b"")
        .await
        .status
        .expect("an answer")
}

/// Catches a `/readyz` that is 200 with no active child (the poll's last pass outliving the
/// child), that is 503 beside a ready active child, and a poll that asks anything but the
/// active child's own `GET /readyz` on its operator port.
#[tokio::test(start_paused = true)]
async fn readyz_needs_active_ready_child() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let a = Ctl::new(200);
    let _listener = operator(&rig, Slot::A, &a);
    let mut client = rig.client().await;
    assert_eq!(readyz(&mut client).await, 503, "no poll has run");
    assert!(!rig.core.snapshot().ready);

    let _poll = polling(&rig);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);
    assert!(rig.core.snapshot().ready);
    let asked = a.requests.lock().expect("lock").clone();
    assert!(
        asked[0].starts_with("GET /readyz HTTP/1.1\r\n"),
        "{asked:?}"
    );

    rig.core.slots.clear_active();
    assert_eq!(readyz(&mut client).await, 503, "a fresh pass, no child");
    assert!(!rig.core.snapshot().ready);
    rig.core.slots.set_active(Slot::A).expect("A is open");
    assert_eq!(readyz(&mut client).await, 200, "the same child again");
    assert_eq!(rig.b.state.dispatches(), 0, "B was never asked anything");
}

/// Catches a poll that counts any answer as a pass, or the wrong status; a child that does
/// not speak HTTP/1.1; and one that is never asked again after a failure.
#[tokio::test(start_paused = true)]
async fn only_a_200_is_a_pass_and_a_recovered_child_is_ready_again() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    let _poll = polling(&rig);
    // The fake child of the rig speaks h2 on this port: the poll's HTTP/1.1 is nonsense to it.
    wait_ms(2_500).await;
    assert_eq!(readyz(&mut client).await, 503, "an h2-only child");

    let a = Ctl::new(503);
    let _listener = operator(&rig, Slot::A, &a);
    wait_ms(2_500).await;
    assert!(a.asked() >= 2, "asked again after each failure");
    assert_eq!(readyz(&mut client).await, 503, "the child says not ready");

    a.garbage.store(true, Ordering::Release);
    wait_ms(2_500).await;
    assert_eq!(readyz(&mut client).await, 503, "an answer that is not HTTP");
    a.garbage.store(false, Ordering::Release);

    a.set(200);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200, "the child recovered");
}

/// Catches a freshness window that is never over, or that ends at the wrong second.
#[tokio::test(start_paused = true)]
async fn a_pass_goes_stale_five_seconds_after_it() {
    let slots = ActiveSlot::new();
    slots.open(Slot::A);
    slots.set_active(Slot::A).expect("A is open");
    let ready = Readiness::new(tcp_connector());
    let t0 = Instant::now();
    assert!(!ready.is_ready(&slots, t0), "nothing has passed");
    ready.pass(Slot::A, slots.generation(Slot::A), t0);
    assert!(ready.is_ready(&slots, t0));
    assert_eq!(FRESH_FOR, Duration::from_secs(5));
    assert!(ready.is_ready(&slots, t0 + FRESH_FOR), "the fifth second");
    assert!(!ready.is_ready(&slots, t0 + FRESH_FOR + Duration::from_nanos(1)));
    assert!(
        ready.is_ready(&slots, t0 - Duration::from_secs(1)),
        "a clock behind the pass is not stale"
    );
}

/// Catches readiness that survives the child it was measured on: a flip to the other slot, and
/// a slot that was retired and opened again (a new process), each wait for their own pass.
#[tokio::test(start_paused = true)]
async fn a_new_child_is_ready_only_after_its_own_poll() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let (a, b) = (Ctl::new(200), Ctl::new(200));
    let _la = operator(&rig, Slot::A, &a);
    let _lb = operator(&rig, Slot::B, &b);
    let mut client = rig.client().await;
    let _poll = polling(&rig);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);

    rig.flip_to_b();
    assert_eq!(readyz(&mut client).await, 503, "B has not been polled");
    wait_ms(1_000).await;
    assert_eq!(readyz(&mut client).await, 200, "B answered its poll");
    assert!(b.asked() >= 1);

    // B's process is replaced: same slot, new generation.
    rig.core.slots.clear_active();
    assert_eq!(rig.core.slots.retire(Slot::B), Retire::Closed);
    rig.core.slots.open(Slot::B);
    rig.core.slots.set_active(Slot::B).expect("B is open again");
    assert_eq!(readyz(&mut client).await, 503, "the new B is unpolled");
    wait_ms(1_000).await;
    assert_eq!(readyz(&mut client).await, 200);
}

/// Catches a refused connection counted as a pass: the active slot has no listener at all (the
/// child died), and `/readyz` must be 503 and stay 503, however many polls have run.
#[tokio::test(start_paused = true)]
async fn a_dead_child_is_never_ready() {
    let mut rig = Rig::start(tagged("a"), tagged("b")).await;
    rig.a.stop_accepting().await;
    let mut client = rig.client().await;
    let _poll = polling(&rig);
    wait_ms(12_500).await;
    assert_eq!(readyz(&mut client).await, 503, "nothing listens on A");
    assert!(!rig.core.snapshot().ready);
}

/// Catches a generation read after the question: the poll of A's first process answers 200
/// after that process was replaced by a new one in the same slot, and that answer says
/// nothing about the new process. `/readyz` waits for the new process's own pass.
#[tokio::test(start_paused = true)]
async fn a_child_replaced_while_asked_is_not_ready_on_the_old_answer() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let a = Ctl::new(200);
    a.delay_ms.store(2_000, Ordering::Release);
    let _listener = operator(&rig, Slot::A, &a);
    let mut client = rig.client().await;
    let _poll = polling(&rig);
    // The first poll starts at 1 s and is answered at 3 s.
    wait_ms(1_500).await;
    assert_eq!(a.asked(), 1, "the poll is in flight");
    rig.core.slots.clear_active();
    assert_eq!(rig.core.slots.retire(Slot::A), Retire::Closed);
    rig.core.slots.open(Slot::A);
    rig.core.slots.set_active(Slot::A).expect("A is open again");
    // The new process answers as slowly: its own poll is answered at 5 s.
    wait_ms(1_600).await;
    assert_eq!(
        readyz(&mut client).await,
        503,
        "the old process's answer is not the new one's"
    );
    wait_ms(2_100).await;
    assert_eq!(readyz(&mut client).await, 200, "the new process answered");
}

/// Catches a poll with no time limit (one hung child would end all polling and `/readyz` would
/// say 200 for ever on its last pass) and one that waits longer than 3 s.
#[tokio::test(start_paused = true)]
async fn a_hung_child_is_given_three_seconds_and_polled_again() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let a = Ctl::new(200);
    a.hang.store(true, Ordering::Release);
    let _listener = operator(&rig, Slot::A, &a);
    let mut client = rig.client().await;
    let _poll = polling(&rig);
    wait_ms(10_500).await;
    assert_eq!(POLL_TIMEOUT, Duration::from_secs(3));
    // Polls at 1 s, 4 s, 7 s and 10 s: each hangs for its 3 s and the next starts at once.
    assert_eq!(a.asked(), 4, "a poll that waits for ever asks once");
    assert_eq!(readyz(&mut client).await, 503);
    a.hang.store(false, Ordering::Release);
    wait_ms(3_100).await;
    assert_eq!(readyz(&mut client).await, 200);
}

/// Catches a poll whose failure reaches the streams: the child that says "not ready" still
/// finishes the stream it has and is still given the next one (a quiesce makes a child slow
/// to answer, and a stream bound to it is its to finish).
#[tokio::test(start_paused = true)]
async fn child_health_failure_leaves_open_streams() {
    let slow = script(|mut call| async move {
        read_all(&mut call.body).await;
        tokio::time::sleep(Duration::from_secs(8)).await;
        grpc_ok(&mut call.respond, "slow-a");
    });
    let rig = Rig::start(slow, tagged("b")).await;
    let a = Ctl::new(200);
    let _listener = operator(&rig, Slot::A, &a);
    let mut client = rig.client().await;
    let _poll = polling(&rig);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);

    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(bytes::Bytes::from_static(b"x"), true)
        .expect("body");
    let open = tokio::spawn(finish(response));
    wait_ms(100).await;
    assert_eq!(rig.core.slots.open_permits(Slot::A), 1);

    a.set(503);
    wait_ms(7_000).await;
    assert_eq!(readyz(&mut client).await, 503, "unhealthy by now");
    assert_eq!(
        rig.core.slots.active(),
        Some(Slot::A),
        "still the active slot"
    );
    assert_eq!(
        rig.core.slots.open_permits(Slot::A),
        1,
        "the stream is open"
    );
    // A new stream still binds to the unhealthy child, and both finish.
    let second = client.unary(CAPS).await;
    assert_eq!(second.text(), "slow-a");
    let first = open.await.expect("client");
    assert_eq!(
        (first.text().as_str(), first.grpc_status().as_deref()),
        ("slow-a", Some("0"))
    );
    assert_eq!(rig.a.state.dispatches(), 2);
    let metrics = rig.core.metrics.render(&rig.core.slots, &rig.core.queue);
    assert!(
        metrics.contains("kbf_proxy_unavailable_total 0"),
        "{metrics}"
    );
}

/// `Proxy::bind` and `run` start the poll: with a child on real sockets answering HTTP/1.1,
/// `/readyz` on the public port goes from 503 to 200 once the supervisor has made the child
/// the active slot.
#[test]
fn run_polls_the_active_child_on_real_sockets() {
    let (base, mut listeners) = reserve_ports();
    let public = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let public_addr: SocketAddr = public.local_addr().expect("addr");
    let config = ProxyConfig::from_toml_str(&format!(
        "operator_token_file = \"/etc/kbf/operator.token\"\n\
         server_cert_file = \"/etc/kbf/tls/server.pem\"\n\
         server_key_file = \"/etc/kbf/tls/server.key\"\n\
         client_ca_file = \"/etc/kbf/tls/ca.pem\"\n\
         hold_max_s = 600\n\
         [upstream]\nbase_port = {base}\n"
    ))
    .expect("a valid table");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(public)).expect("bind");
    let serving = std::thread::spawn(move || runner.run(NoUpdates));
    let ctl = Ctl::new(200);
    handle.runtime().block_on(async {
        let operator = listeners.remove(2);
        operator.set_nonblocking(true).expect("non-blocking");
        let operator = tokio::net::TcpListener::from_std(operator).expect("listener");
        let served = Arc::clone(&ctl);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = operator.accept().await {
                tokio::spawn(answer(tcp, Arc::clone(&served)));
            }
        });
        let mut client = super::kit::Client::connect_tcp(public_addr).await;
        assert_eq!(readyz(&mut client).await, 503, "no active child yet");
        handle.slots().adopt(Slot::A, None);
        let mut ready = false;
        for _ in 0..5_000 {
            if readyz(&mut client).await == 200 {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(ready, "the poll never made the active child ready");
        assert!(handle.snapshot().ready);
    });
    handle.shutdown();
    serving.join().expect("the runner thread").expect("run");
}
