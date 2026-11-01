//! The drain sequence of a frontend over a state store (`POST /v1/frontend/drain` and a stop
//! signal, `kbf_server::sequence`): two frontends in this process over one real Postgres
//! database (the server `KBF_TEST_POSTGRES_URL` names; these tests fail without it), as the
//! tests of retire mode have them. A fake daemon and a client play the farm's node and Buck2.
//! `--drain-lb-wait` and `--drain-deadline` are given as an operator gives them, but with
//! a day of seconds, which no test lives to see run out: a test ends a wait by hand
//! (`Drain::end_lb_wait`, `Drain::pass_deadline`) and follows the drain by its phase events
//! (`Drain::phases`) and by the intake polls the frontend counts, never by time.

use futures::FutureExt as _;
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_server::api::FRONTEND_DRAIN;
use kbf_server::drain::Phase;
use tonic::{Code, Status, Streaming};

use super::retire::{
    Member, Pair, bearer, exchange, execute_refusal, polled, queued, read_only_exchange, readyz,
};
use super::*;

/// The longest a test waits for an event: a bound that only a hang reaches.
const SAFETY: Duration = Duration::from_secs(30);

/// Both waits of the drain as long as a day.
const HELD: [&str; 2] = ["--drain-lb-wait=86400", "--drain-deadline=86400"];

/// No wait for the load balancer, and a deadline as long as a day.
const NO_WAIT: [&str; 2] = ["--drain-lb-wait=0", "--drain-deadline=86400"];

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

/// The status `GET /v1/frontend/drain` answers, which is 200, asked again on a connection
/// the API closed unanswered as GOAWAY passed its socket on ([`read_only_exchange`]).
async fn status(api: SocketAddr) -> Value {
    let request =
        format!("GET {FRONTEND_DRAIN} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    let (code, status) = read_only_exchange(api, &request).await;
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

/// The status an Execute stream ends with, within thirty seconds.
async fn stream_end(ops: &mut Streaming<Operation>) -> Status {
    let ending = async {
        loop {
            match ops.message().await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the stream ended without an error"),
                Err(status) => return status,
            }
        }
    };
    let ended = tokio::time::timeout(SAFETY, ending).await;
    ended.expect("the stream ends in time")
}

/// Whether `ops` has ended, or holds an error to read, by now: a look that does not wait.
/// The updates it holds are read and dropped.
fn has_ended(ops: &mut Streaming<Operation>) -> bool {
    loop {
        match ops.message().now_or_never() {
            None => return false,
            Some(Ok(Some(_))) => {}
            Some(_) => return true,
        }
    }
}

/// Reads `ops` until its operation is executing: a lease for it has started on a node.
async fn until_executing(ops: &mut Streaming<Operation>) {
    let reading = async {
        loop {
            let op = ops.message().await.expect("healthy").expect("an update");
            if support::stage(&op) == ExecStage::Executing as i32 {
                return;
            }
        }
    };
    let read = tokio::time::timeout(SAFETY, reading).await;
    read.expect("the operation starts executing in time");
}

/// Asserts that a new Execute of `job` on `client` is not taken once the wait is over. The
/// frontend refuses it UNAVAILABLE and sends the REAPI listener GOAWAY in the same step, so
/// on the connection `client` already holds it surfaces as UNAVAILABLE from the service or
/// as CANCELLED when the GOAWAY got there first (as in
/// `an_open_execute_survives_stop_until_its_action_is_done`), or as INTERNAL when the
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

/// Asserts that in the wait the frontend answers `/readyz` 503 at once and serves on: a
/// unary call is answered and a new Execute is taken. When the wait is ended the phase is
/// `draining`, a new Execute is refused UNAVAILABLE, and the frontend takes no more work in
/// from the store (an operation queued on the other frontend then stays off its queue, which
/// is read after the frontend has run intake polls that began after it was queued), while the
/// operation it holds is run by a daemon of the other frontend and its caller gets the
/// result: the stream of that queued operation, which a worker could take, is kept (it has
/// not ended after the frontend has run intake polls since the wait ended, and it ends with
/// the result). Then the frontend, with nothing open, exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_serves_everything_and_its_end_hands_the_work_to_the_other_frontend() {
    let pair = Pair::start_with("drain-wait", &HELD).await;
    let (a, b) = (&pair.a, &pair.b);
    let on_a = Client::connect(a.reapi, a.worker).await;
    let on_b = Client::connect(b.reapi, b.worker).await;
    let (first, second, third, hold) = (
        Job::new("taken in the wait", &[]),
        Job::new("refused after the wait", &[]),
        Job::new("queued on the other frontend", &[]),
        Job::new("keeps the drained frontend open", &[]),
    );
    for job in [&first, &second, &third, &hold] {
        on_a.upload(&job.blobs()).await;
    }
    let result = support::output(&on_b, "built while the frontend drains", 0).await;
    assert_eq!(a.drain.phase(), Phase::Serving);
    // The one node of this frontend has room for one lease: `hold` takes it and keeps the
    // frontend from exiting at once, and `first` queues behind it.
    let mut node_a = on_a.daemon("node-a", 1, 4).await;
    let mut held = on_a.execute(&hold.action).await;
    let held_start = node_a.start().await;
    until_executing(&mut held).await;

    let begun = begin_drain(a.api).await;
    assert_eq!(begun["phase"], "lb_wait", "{begun}");
    assert_eq!(begun["draining"], true, "{begun}");
    assert_eq!(a.drain.phase(), Phase::LbWait, "ended on its own");
    assert_eq!(readyz(a.api).await, 503, "ready in the wait");
    assert!(on_a.holds(&first.action).await, "a unary call in the wait");
    let mut ops = on_a.execute(&first.action).await;
    ops.message().await.expect("healthy").expect("QUEUED");

    a.drain.end_lb_wait();
    reaches(a, Phase::Draining).await;
    assert_refused_after_the_wait(&on_a, &second).await;

    let mut late = on_b.execute(&third.action).await;
    late.message().await.expect("healthy").expect("QUEUED");
    polled(b, 2).await;
    assert_eq!(queued(b.api).await, 2, "both queued on the other");
    // The last of two intake polls of the drained frontend began after `third` was queued.
    polled(a, 2).await;
    assert_eq!(queued(a.api).await, 1, "the drained frontend took work in");
    // `first` waits behind the running `hold` on the busy node, so a worker could take it:
    // its stream is kept, though this frontend places nothing now (the other frontend
    // takes the operation). It is read after the intake polls above, which began after the
    // wait ended and are where a stream like this one would be ended.
    assert!(
        !has_ended(&mut ops),
        "the stream of a queued operation ended"
    );

    let mut daemon = on_b.daemon("node-b", 4, 8).await;
    for _ in 0..2 {
        let start = daemon.start().await;
        let ack = daemon.report(ran(start.lease_id, &result)).await;
        assert!(ack.accepted, "{ack:?}");
    }
    let last = done(&mut late).await;
    assert_eq!(response(&last).result, Some(result.clone()));
    // The caller of `first` gets the result the other frontend records.
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result.clone()));
    let ack = node_a.report(ran(held_start.lease_id, &result)).await;
    assert!(ack.accepted, "{ack:?}");
    done(&mut held).await;

    a.exits().await;
    drop((ops, held, late, daemon, node_a, on_a, on_b));
    pair.stop().await;
}

/// Asserts that an execution that is running on a node of the drained frontend finishes
/// there: the frontend is draining with the execution open and still answers, the node's
/// result is taken after the wait, its caller gets it, the node's stream ends UNAVAILABLE
/// once the execution is gone (its daemon dials another frontend), and the frontend exits
/// 0. The wait is zero here (`--drain-lb-wait=0`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_execution_finishes_before_the_frontend_exits() {
    let pair = Pair::start_with("drain-finish", &NO_WAIT).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("finishes while the frontend drains", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&client, "built while the frontend drains", 0).await;
    let serving = status(a.api).await;
    assert_eq!(serving["open_executions"], 1, "{serving}");

    let begun = begin_drain(a.api).await;
    assert_eq!(begun["draining"], true, "{begun}");
    reaches(a, Phase::Draining).await;
    let draining = status(a.api).await;
    assert_eq!(draining["phase"], "draining", "{draining}");
    assert_eq!(draining["open_executions"], 1, "{draining}");

    let ack = daemon.report(ran(start.lease_id, &result)).await;
    assert!(ack.accepted, "{ack:?}");
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    a.exits().await;
    drop((ops, daemon, client));
    pair.stop().await;
}

/// Asserts that when the deadline passes (by hand here, as its time running out passes it)
/// with an execution open, the drain reports and goes on: the status says `deadline_passed`
/// with the open count, the phase stays `draining`, the caller's stream and the node's are
/// not ended, and the frontend stays up. The node's result is then taken, the caller gets
/// it, and the frontend exits 0 once the execution is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_the_deadline_the_streams_stay_open_until_the_execution_ends() {
    let pair = Pair::start_with("drain-deadline", &NO_WAIT).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("kept past the deadline", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let start = daemon.start().await;
    let result = support::output(&client, "built past the deadline", 0).await;

    begin_drain(a.api).await;
    reaches(a, Phase::Draining).await;
    let before = status(a.api).await;
    assert!(before.get("deadline_passed").is_none(), "{before}");
    a.drain.pass_deadline();
    let mut passed = before;
    for _ in 0..SAFETY.as_secs() * 10 {
        passed = status(a.api).await;
        if passed.get("deadline_passed").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(passed["deadline_passed"], true, "{passed}");
    assert_eq!(passed["phase"], "draining", "{passed}");
    assert_eq!(passed["open_executions"], 1, "{passed}");
    // Nothing ends in a quiet moment: the caller's stream and the node's stay open.
    let quiet = Duration::from_secs(2);
    let ended = tokio::time::timeout(quiet, stream_end(&mut ops)).await;
    assert!(ended.is_err(), "the caller's stream was ended");
    let ended = tokio::time::timeout(quiet, daemon.ended()).await;
    assert!(ended.is_err(), "the node's stream was ended");

    let ack = daemon.report(ran(start.lease_id, &result)).await;
    assert!(ack.accepted, "{ack:?}");
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
    a.exits().await;
    drop((ops, daemon, client));
    pair.stop().await;
}

/// Asserts that a stop signal runs the same sequence: `/readyz` answers 503 once the phase
/// is `lb_wait` while a unary call is answered and a new Execute is taken, the phase is
/// `draining` after the wait is ended and a new Execute is refused, and the frontend exits 0
/// once its executions are gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_signal_runs_the_same_sequence() {
    let mut pair = Pair::start_with("drain-signal", &HELD).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let (first, second, third) = (
        Job::new("open before the signal", &[]),
        Job::new("taken in the wait", &[]),
        Job::new("refused after the wait", &[]),
    );
    for job in [&first, &second, &third] {
        client.upload(&job.blobs()).await;
    }
    // A node of the frontend runs both operations, so their streams have a lease to wait for.
    let daemon = client.daemon("node-a", 4, 8).await;
    let mut before = client.execute(&first.action).await;
    until_executing(&mut before).await;

    pair.a.signal();
    let a = &pair.a;
    // The signal is taken by the serving task, which answers not ready before it says the
    // phase is the wait.
    reaches(a, Phase::LbWait).await;
    assert_eq!(readyz(a.api).await, 503, "ready after the signal");
    assert!(
        client.holds(&second.action).await,
        "a unary call in the wait"
    );
    let mut in_the_wait = client.execute(&second.action).await;
    until_executing(&mut in_the_wait).await;

    a.drain.end_lb_wait();
    reaches(a, Phase::Draining).await;
    assert_refused_after_the_wait(&client, &third).await;
    let draining = status(a.api).await;
    assert_eq!(draining["phase"], "draining", "{draining}");
    assert_eq!(draining["open_executions"], 2, "{draining}");

    drop((before, in_the_wait));
    a.exits().await;
    drop((daemon, client));
    pair.stop().await;
}

/// Asserts that every listener is sent GOAWAY, and none before the wait is over: an HTTP/2
/// client on the REAPI listener, one on the worker listener and one on the operator API have
/// been sent no GOAWAY in the wait, whose phase is `lb_wait` and whose listeners all still
/// accept; and each is sent one when the wait is ended, and the operator API goes on
/// answering after it, with the phase `draining`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_listener_is_sent_goaway_when_the_wait_ends() {
    let pair = Pair::start_with("drain-goaway", &HELD).await;
    let a = &pair.a;
    let client = Client::connect(a.reapi, a.worker).await;
    let job = Job::new("holds the drain open", &[]);
    client.upload(&job.blobs()).await;
    let _daemon = client.daemon("node-a", 4, 8).await;
    let mut ops = client.execute(&job.action).await;
    until_executing(&mut ops).await;
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
    reaches(a, Phase::Draining).await;
    tokio::join!(reapi.goaway(), worker.goaway(), operator.goaway());
    let draining = status(a.api).await;
    assert_eq!(draining["phase"], "draining", "{draining}");

    drop((ops, open));
    a.exits().await;
    drop(client);
    pair.stop().await;
}
