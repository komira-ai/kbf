//! The drain sequence of a frontend over a state store (`POST /v1/frontend/drain` and a stop
//! signal, `kbf_server::sequence`): two frontends in this process over one real Postgres
//! database (the server `KBF_TEST_POSTGRES_URL` names; these tests fail without it), as the
//! tests of retire mode have them. A client plays Buck2. A frontend started here has no executor,
//! so no execution is open on it; a drain that waits for open executions is tested over the
//! in-process cell in `frontend_drain.rs`. `--drain-lb-wait` and `--drain-deadline` are given
//! as an operator gives them, but with a day of seconds, which no test lives to see run out: a
//! test ends a wait by hand (`Drain::end_lb_wait`) and follows the drain by its phase events
//! (`Drain::phases`), never by time.

use futures::FutureExt as _;
use kbf_server::api::FRONTEND_DRAIN;
use kbf_server::drain::Phase;
use tonic::Code;

use super::retire::{Member, Pair, bearer, exchange, execute_refusal, readyz};
use super::*;

/// The longest a test waits for an event: a bound that only a hang reaches.
const SAFETY: Duration = Duration::from_secs(30);

/// Both waits of the drain as long as a day.
const HELD: [&str; 2] = ["--drain-lb-wait=86400", "--drain-deadline=86400"];

/// One call to the drain endpoint of the frontend at `api`; `auth` is the header lines to
/// send besides the content type.
async fn drain_call(api: SocketAddr, method: &str, auth: &str) -> (u16, Value) {
    let request = format!(
        "{method} {FRONTEND_DRAIN} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Type: application/json\r\n{auth}Content-Length: 0\r\n\r\n"
    );
    exchange(api, &request).await
}

/// `POST /v1/frontend/drain` with the token and no body, which answers 200.
async fn begin_drain(api: SocketAddr) -> Value {
    let (code, status) = drain_call(api, "POST", &bearer()).await;
    assert_eq!(code, 200, "{status}");
    status
}

/// Waits until the drain of `member` is in `phase`, by the events of its phases.
async fn reaches(member: &Member, phase: Phase) {
    let mut phases = member.drain.phases();
    let waiting = phases.wait_for(|now| *now == phase);
    let reached = tokio::time::timeout(SAFETY, waiting).await;
    let seen = reached.expect("the phase is reached in time");
    seen.expect("the drain is alive");
}

/// Waits until `member`'s drain has left the wait: the sequence publishes `draining` only once
/// it refuses new Execute calls and has sent GOAWAY, so an Execute sent after this is one the
/// drain has had the chance to refuse (`Drain::end_lb_wait` only fires the event the
/// sequence acts on).
async fn leaves_the_wait(member: &Member) {
    let mut phases = member.drain.phases();
    let waiting = phases.wait_for(|now| !matches!(now, Phase::Serving | Phase::LbWait));
    let left = tokio::time::timeout(SAFETY, waiting).await;
    let seen = left.expect("the wait is left in time");
    seen.expect("the drain is alive");
}

/// An HTTP/2 client connection to a listener, and the bytes the listener has sent on it.
struct Conn {
    stream: TcpStream,
    sent: Vec<u8>,
}

impl Conn {
    /// A connection to `addr` that has said HTTP/2 without negotiating it (the connection
    /// preface and an empty SETTINGS frame), as an h2c client does, and has waited for the
    /// listener's own SETTINGS frame: the listener has accepted it and serves HTTP/2 on it.
    async fn open(addr: SocketAddr) -> Self {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        preface.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
        stream.write_all(&preface).await.expect("send the preface");
        let mut conn = Self {
            stream,
            sent: Vec::new(),
        };
        conn.receives(SETTINGS).await;
        conn
    }

    /// Whether a GOAWAY frame is among the bytes the listener has sent by now: a look that
    /// takes in what has arrived and does not wait.
    fn goaway_so_far(&mut self) -> bool {
        let mut chunk = [0_u8; 1024];
        while let Some(Ok(read @ 1..)) = self.stream.read(&mut chunk).now_or_never() {
            self.sent.extend_from_slice(&chunk[..read]);
        }
        frame_types(&self.sent).contains(&GOAWAY)
    }

    /// Waits until the listener has sent a frame of type `kind` on this connection.
    async fn receives(&mut self, kind: u8) {
        let reading = async {
            let mut chunk = [0_u8; 1024];
            while !frame_types(&self.sent).contains(&kind) {
                let read = self.stream.read(&mut chunk).await.expect("read");
                assert_ne!(read, 0, "the connection closed before the frame was sent");
                self.sent.extend_from_slice(&chunk[..read]);
            }
        };
        let sent = tokio::time::timeout(SAFETY, reading).await;
        sent.expect("the frame is sent in time");
    }

    /// Waits until the listener has sent a GOAWAY frame on this connection.
    async fn goaway(&mut self) {
        self.receives(GOAWAY).await;
    }
}

/// The types of the HTTP/2 frames in `bytes`: a frame is a head of nine bytes, the first
/// three the length of the payload and the fourth its type, and the payload.
fn frame_types(mut bytes: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    while let [a, b, c, kind, _, _, _, _, _, rest @ ..] = bytes {
        let length = usize::try_from(u32::from_be_bytes([0, *a, *b, *c])).unwrap_or(usize::MAX);
        types.push(*kind);
        bytes = rest.get(length..).unwrap_or_default();
    }
    types
}

/// The type of a SETTINGS frame.
const SETTINGS: u8 = 4;

/// The type of a GOAWAY frame.
const GOAWAY: u8 = 7;

/// Asserts that a new Execute of `job` on `client` is not taken once the wait is over. The
/// frontend refuses it UNAVAILABLE and sends the REAPI listener GOAWAY in the same step, so
/// on the connection `client` already holds it surfaces as UNAVAILABLE from the service or
/// as CANCELLED when the GOAWAY got there first (as in
/// an open execute surviving a stop), or as INTERNAL when the
/// frontend's GOAWAY (NO_ERROR) ended the call on that connection ([`GOAWAY_NO_ERROR`]). Any
/// other code, any other INTERNAL (a GOAWAY with an error, a connection that failed), or an
/// Execute that is answered, fails, and the whole status is printed.
async fn assert_refused_after_the_wait(client: &Client, job: &Job) {
    let refused = execute_refusal(client, job).await;
    let refused = refused.expect("a new Execute is refused after the wait");
    assert!(
        is_a_refusal(&refused),
        "a new Execute was not refused after the wait: {refused:?}"
    );
}

/// How h2 words the error of a call ended by the peer's GOAWAY with NO_ERROR, the code the
/// frontend's drain sends: the only INTERNAL that is a refusal after the wait. hyper wraps that
/// error, so it is in the status's chain of sources, not in its message ("h2 protocol error:
/// http2 error", as CI showed).
const GOAWAY_NO_ERROR: &str = "connection error received: not a result of an error";

/// Whether an error in the chain of sources of `status` is the peer's GOAWAY with NO_ERROR. A
/// GOAWAY with an error code, a local protocol error and an INTERNAL from the service carry no
/// such source.
fn ended_by_goaway_no_error(status: &tonic::Status) -> bool {
    let mut next = std::error::Error::source(status);
    while let Some(error) = next {
        if error.to_string().contains(GOAWAY_NO_ERROR) {
            return true;
        }
        next = error.source();
    }
    false
}

/// Whether `status` is how the frontend's refusal of new work after the wait reaches a client:
/// UNAVAILABLE from the service, CANCELLED by the GOAWAY, or a call the GOAWAY (NO_ERROR)
/// ended.
fn is_a_refusal(status: &tonic::Status) -> bool {
    match status.code() {
        Code::Unavailable | Code::Cancelled => true,
        Code::Internal => ended_by_goaway_no_error(status),
        _ => false,
    }
}

/// Asserts that in the wait the frontend answers `/readyz` 503 at once and serves on (a unary
/// call is answered), that when the wait is ended a new Execute is refused UNAVAILABLE, and
/// that the frontend, with nothing open, exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_serves_unary_calls_and_its_end_refuses_new_work() {
    let pair = Pair::start_with("drain-wait", &HELD).await;
    let a = &pair.a;
    let client = a.client().await;
    let (first, second) = (
        Job::new("asked in the wait", &[]),
        Job::new("refused after the wait", &[]),
    );
    for job in [&first, &second] {
        client.upload(&job.blobs()).await;
    }
    assert_eq!(a.drain.phase(), Phase::Serving);

    let begun = begin_drain(a.api).await;
    assert_eq!(begun["phase"], "lb_wait", "{begun}");
    assert_eq!(begun["draining"], true, "{begun}");
    assert_eq!(a.drain.phase(), Phase::LbWait, "ended on its own");
    assert_eq!(readyz(a.api).await, 503, "ready in the wait");
    assert!(
        client.holds(&first.action).await,
        "a unary call in the wait"
    );

    a.drain.end_lb_wait();
    leaves_the_wait(a).await;
    assert_refused_after_the_wait(&client, &second).await;
    a.exits().await;
    drop(client);
    pair.stop().await;
}

/// Asserts that a stop signal runs the same sequence: `/readyz` answers 503 once the phase
/// is `lb_wait` while a unary call is answered, a new Execute is refused after the wait is
/// ended, and the frontend exits 0 once it has nothing open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_signal_runs_the_same_sequence() {
    let mut pair = Pair::start_with("drain-signal", &HELD).await;
    let a = &pair.a;
    let client = a.client().await;
    let (first, second) = (
        Job::new("asked in the wait", &[]),
        Job::new("refused after the wait", &[]),
    );
    for job in [&first, &second] {
        client.upload(&job.blobs()).await;
    }

    pair.a.signal();
    let a = &pair.a;
    // The signal is taken by the serving task, which answers not ready before it says the
    // phase is the wait.
    reaches(a, Phase::LbWait).await;
    assert_eq!(readyz(a.api).await, 503, "ready after the signal");
    assert!(
        client.holds(&first.action).await,
        "a unary call in the wait"
    );

    a.drain.end_lb_wait();
    leaves_the_wait(a).await;
    assert_refused_after_the_wait(&client, &second).await;
    a.exits().await;
    drop(client);
    pair.stop().await;
}

/// Asserts that every listener is sent GOAWAY, and none before the wait is over: an HTTP/2
/// client on the REAPI listener, one on the worker listener and one on the operator API have
/// been sent no GOAWAY in the wait, whose phase is `lb_wait` and whose listeners all still
/// accept; and each is sent one when the wait is ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_listener_is_sent_goaway_when_the_wait_ends() {
    let pair = Pair::start_with("drain-goaway", &HELD).await;
    let a = &pair.a;
    let client = a.client().await;
    let mut open = [
        Conn::open(a.reapi).await,
        Conn::open(a.worker).await,
        Conn::open(a.api).await,
    ];

    begin_drain(a.api).await;
    assert_eq!(a.drain.phase(), Phase::LbWait, "ended on its own");
    // A new connection to each listener is accepted and answered in the wait; the look at
    // the first ones below is made after these round trips.
    for listener in [a.reapi, a.worker, a.api] {
        Conn::open(listener).await;
    }
    let [reapi, worker, operator] = &mut open;
    let early = [
        reapi.goaway_so_far(),
        worker.goaway_so_far(),
        operator.goaway_so_far(),
    ];
    assert_eq!(early, [false, false, false], "GOAWAY in the wait");

    a.drain.end_lb_wait();
    tokio::join!(reapi.goaway(), worker.goaway(), operator.goaway());

    drop(open);
    a.exits().await;
    drop(client);
    pair.stop().await;
}
