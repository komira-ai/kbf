//! `/readyz`: the proxy's polls of the members are its writers, and a poll never feeds back into
//! routing. The fake member's operator listener answers HTTP/1.1, as the real one does; the
//! tests run under paused time, so the poll's seconds are virtual.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;

use super::kit::{CAPS, finish, grpc_ok, read_all, script, tagged};
use super::pool_kit::Rig;
use crate::pool::member_addr;
use crate::probe::POLL_TIMEOUT;
use crate::route::Target;
use crate::upstreams::State;

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

/// Puts an HTTP/1.1 answerer where member `index`'s operator listener was.
fn operator(rig: &Rig, index: usize, ctl: &Arc<Ctl>) -> JoinHandle<()> {
    let mut incoming = rig
        .net
        .listen(member_addr(rig.members.base(index), Target::Operator));
    let ctl = Arc::clone(ctl);
    tokio::spawn(async move {
        while let Some(io) = incoming.recv().await {
            tokio::spawn(answer(io, Arc::clone(&ctl)));
        }
    })
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

/// Catches a `/readyz` that is 200 with no member that takes streams (the poll's last pass
/// outliving the member's place in the pool), that is 503 beside a ready member, and a poll
/// that asks anything but the member's own `GET /readyz` on its operator port.
#[tokio::test(start_paused = true)]
async fn readyz_needs_a_member_that_is_up_and_ready() {
    let rig = Rig::start(&[tagged("a"), tagged("b")]);
    let a = Ctl::new(200);
    let _listener = operator(&rig, 0, &a);
    let mut client = rig.client().await;
    assert_eq!(readyz(&mut client).await, 503, "no poll has run");
    assert!(!rig.core.is_ready());

    let _poll = rig.probing();
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);
    assert!(rig.core.is_ready());
    let asked = a.requests.lock().expect("lock").clone();
    assert!(
        asked[0].starts_with("GET /readyz HTTP/1.1\r\n"),
        "{asked:?}"
    );

    assert!(rig.members.set_draining("a", true));
    assert_eq!(readyz(&mut client).await, 503, "a fresh pass, no member");
    assert!(!rig.core.is_ready());
    assert!(rig.members.set_draining("a", false));
    assert_eq!(readyz(&mut client).await, 200, "the same member again");
    assert_eq!(rig.dispatches()[1], 0, "b was never asked anything");
}

/// Catches a poll that counts any answer as a pass, or the wrong status; a member that does
/// not speak HTTP/1.1; and one that is never asked again after a failure.
#[tokio::test(start_paused = true)]
async fn only_a_200_is_a_pass_and_a_recovered_member_is_ready_again() {
    let rig = Rig::start(&[tagged("a")]);
    let mut client = rig.client().await;
    let _poll = rig.probing();
    // The fake child of the rig speaks h2 on this port: the poll's HTTP/1.1 is nonsense to it.
    wait_ms(2_500).await;
    assert_eq!(readyz(&mut client).await, 503, "an h2-only member");

    let a = Ctl::new(503);
    let _listener = operator(&rig, 0, &a);
    wait_ms(2_500).await;
    assert!(a.asked() >= 2, "asked again after each failure");
    assert_eq!(readyz(&mut client).await, 503, "the member says not ready");

    a.garbage.store(true, Ordering::Release);
    wait_ms(2_500).await;
    assert_eq!(readyz(&mut client).await, 503, "an answer that is not HTTP");
    a.garbage.store(false, Ordering::Release);

    a.set(200);
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200, "the member recovered");
}

/// Catches a refused connection counted as a pass: the member has no listener at all (it
/// died), and `/readyz` must be 503 and stay 503, however many polls have run.
#[tokio::test(start_paused = true)]
async fn a_dead_member_is_never_ready() {
    let mut rig = Rig::start(&[tagged("a")]);
    rig.children[0].stop_accepting().await;
    let mut client = rig.client().await;
    let _poll = rig.probing();
    wait_ms(12_500).await;
    assert_eq!(readyz(&mut client).await, 503, "nothing listens on a");
    assert!(!rig.core.is_ready());
}

/// Catches a poll with no time limit (one hung member would end all polling and `/readyz` would
/// say 200 for ever on its last pass) and one that waits longer than 3 s.
#[tokio::test(start_paused = true)]
async fn a_hung_member_is_given_three_seconds_and_polled_again() {
    let rig = Rig::start(&[tagged("a")]);
    let a = Ctl::new(200);
    a.hang.store(true, Ordering::Release);
    let _listener = operator(&rig, 0, &a);
    let mut client = rig.client().await;
    let _poll = rig.probing();
    wait_ms(10_500).await;
    assert_eq!(POLL_TIMEOUT, Duration::from_secs(3));
    // Each poll hangs for its 3 s and the next starts at once.
    assert_eq!(a.asked(), 4, "a poll that waits for ever asks once");
    assert_eq!(readyz(&mut client).await, 503);
    a.hang.store(false, Ordering::Release);
    wait_ms(3_100).await;
    assert_eq!(readyz(&mut client).await, 200);
}

/// Catches a poll whose failure reaches the streams: the member that says "not ready" still
/// finishes the stream it has and is still given the next one (a member can be slow to answer
/// while its streams are its to finish).
#[tokio::test(start_paused = true)]
async fn member_health_failure_leaves_open_streams() {
    let slow = script(|mut call| async move {
        read_all(&mut call.body).await;
        tokio::time::sleep(Duration::from_secs(8)).await;
        grpc_ok(&mut call.respond, "slow-a");
    });
    let rig = Rig::start(&[slow]);
    let a = Ctl::new(200);
    let _listener = operator(&rig, 0, &a);
    let mut client = rig.client().await;
    let _poll = rig.probing();
    wait_ms(1_100).await;
    assert_eq!(readyz(&mut client).await, 200);

    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(bytes::Bytes::from_static(b"x"), true)
        .expect("body");
    let open = tokio::spawn(finish(response));
    wait_ms(100).await;
    assert_eq!(rig.members.snapshot()[0].open_streams.reapi, 1);

    a.set(503);
    wait_ms(7_000).await;
    assert_eq!(readyz(&mut client).await, 503, "unhealthy by now");
    let view = rig.members.snapshot();
    assert_eq!(view[0].state, State::Up, "still takes streams");
    assert_eq!(view[0].open_streams.reapi, 1, "the stream is open");
    // A new stream still goes to the unhealthy member, and both finish.
    let second = client.unary(CAPS).await;
    assert_eq!(second.text(), "slow-a");
    let first = open.await.expect("client");
    assert_eq!(
        (first.text().as_str(), first.grpc_status().as_deref()),
        ("slow-a", Some("0"))
    );
    assert_eq!(rig.dispatches(), [2]);
    let metrics = rig.core.metrics.render();
    assert!(
        metrics.contains("kbf_proxy_unavailable_total 0"),
        "{metrics}"
    );
}
