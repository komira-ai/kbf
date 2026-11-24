//! The listeners' HTTP/2 keepalive ([`keepalive`]) against a client whose path to the
//! server stalls and then carries on, as a CI runner's path through the tailnet does for
//! up to a minute when the uplink in front of the farm drops. The server PINGs a quiet
//! connection; a PING that stays unanswered for the keepalive timeout closes the
//! connection (GOAWAY NO_ERROR, then the close), and every stream on it fails. Buck2 never
//! recovers a broken Execute stream, so each action on it fails.
//!
//! The client and the server talk through in-memory pipes and a relay that can stall them
//! ([`Path`]), on the paused clock: the stall is the time the test sleeps, and the
//! server's keepalive timers run on the same clock. The server's timing is what
//! `kbf-server` and `kbf-frontend` run with when no flag sets it.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use clap::Parser as _;
use futures::StreamExt as _;
use hyper_util::rt::TokioIo;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::grpc::health::v1::{HealthCheckRequest, HealthCheckResponse};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::transport::{Endpoint, Server};
use tonic::{Status, Streaming};

use crate::config::Args;
use crate::grpc_health::HealthService;
use crate::health::{Checks, Readiness};
use crate::serve::{StreamTiming, keepalive};

/// What one pipe holds before its writer waits: more than the frames a stalled connection
/// queues here (a PING, a few window updates).
const PIPE_BYTES: usize = 1 << 20;

/// Longer than any wait here should take: a call still waiting after it has hung.
const HANG: Duration = Duration::from_secs(3600);

/// The longest stall of the farm's uplink seen in a day of the control host's tailscaled
/// log (the poll for the tailnet's map failing after 57.6 s), rounded up.
const OBSERVED_STALL: Duration = Duration::from_secs(60);

/// The timing `kbf-server` and `kbf-frontend` serve with when no flag sets it.
fn default_timing() -> StreamTiming {
    let args = Args::try_parse_from(["kbf-server"]).expect("no flag");
    args.listeners().expect("the default listeners").streams
}

/// The path between the client and the server: while it is stalled nothing crosses it in
/// either direction, and what either side writes waits in the pipes, as it waits in the
/// socket buffers while the network in between drops every packet.
struct Path {
    open: watch::Sender<bool>,
}

impl Path {
    /// A path between `client` and `server`, open.
    fn between(client: DuplexStream, server: DuplexStream) -> Self {
        let (open, gate) = watch::channel(true);
        let (from_client, to_client) = tokio::io::split(client);
        let (from_server, to_server) = tokio::io::split(server);
        tokio::spawn(carry(from_client, to_server, gate.clone()));
        tokio::spawn(carry(from_server, to_client, gate));
        Self { open }
    }

    fn stall(&self) {
        self.open.send_replace(false);
    }

    fn resume(&self) {
        self.open.send_replace(true);
    }
}

/// Copies `from` to `to` while `gate` is open: it reads only while open and writes what it
/// read only once open again. At the end of `from` it ends `to`.
async fn carry(
    mut from: ReadHalf<DuplexStream>,
    mut to: WriteHalf<DuplexStream>,
    mut gate: watch::Receiver<bool>,
) {
    let mut chunk = vec![0; 16 * 1024];
    loop {
        if gate.wait_for(|open| *open).await.is_err() {
            return;
        }
        let read = match from.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if gate.wait_for(|open| *open).await.is_err() {
            return;
        }
        if to.write_all(&chunk[..read]).await.is_err() {
            return;
        }
    }
    // A path cut by a stall ends here only once the gate opened: the end crosses too.
    let _ = gate.wait_for(|open| *open).await;
    let _ = to.shutdown().await;
}

/// A connector that hands the client its one pipe; a second dial fails, so a connection
/// the server closed is not replaced by a new one under the open stream.
struct OnePipe(Option<DuplexStream>);

impl Service<Uri> for OnePipe {
    type Response = TokioIo<DuplexStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let pipe = self.0.take();
        Box::pin(async move {
            pipe.map(TokioIo::new)
                .ok_or_else(|| io::Error::other("this client has one connection"))
        })
    }
}

/// A server over `timing` with the gRPC health service, a client connected to it over a
/// [`Path`], and the client's open `Watch` stream, its first status read: a long-lived
/// stream, as an Execute stream is while its action runs. Stopping `readiness` sends the
/// stream one more status.
struct Rig {
    path: Path,
    readiness: Arc<Readiness>,
    watch: Streaming<HealthCheckResponse>,
}

impl Rig {
    async fn open(timing: StreamTiming) -> Self {
        let (client_end, path_client) = tokio::io::duplex(PIPE_BYTES);
        let (path_server, server_end) = tokio::io::duplex(PIPE_BYTES);
        let path = Path::between(path_client, path_server);

        let readiness = Arc::new(Readiness::default());
        let checks = Arc::new(Checks::new(Arc::clone(&readiness)));
        let health = HealthServer::new(HealthService::of(checks, Duration::from_secs(5)));
        // One connection, then an incoming that never ends: an incoming that ends stops
        // the server.
        let incoming = futures::stream::iter([Ok::<_, io::Error>(server_end)])
            .chain(futures::stream::pending());
        let server = keepalive(Server::builder(), timing).add_service(health);
        tokio::spawn(server.serve_with_incoming(incoming));

        let channel = Endpoint::from_static("http://kbf.test")
            .connect_with_connector(OnePipe(Some(client_end)))
            .await
            .expect("connected over the pipe");
        let mut watch = HealthClient::new(channel)
            .watch(HealthCheckRequest::default())
            .await
            .expect("the watch opened")
            .into_inner();
        assert_eq!(status(next(&mut watch).await), Ok(ServingStatus::Serving));
        Self {
            path,
            readiness,
            watch,
        }
    }

    /// Stalls the path for `stall`, then resumes it.
    async fn stall_for(&self, stall: Duration) {
        self.path.stall();
        tokio::time::sleep(stall).await;
        self.path.resume();
    }
}

/// The next message of `watch`, failing the test if it hangs.
async fn next(watch: &mut Streaming<HealthCheckResponse>) -> Result<HealthCheckResponse, Status> {
    let message = tokio::time::timeout(HANG, watch.message())
        .await
        .expect("the watch neither answered nor failed");
    message.and_then(|m| m.ok_or_else(|| Status::internal("the watch ended")))
}

fn status(message: Result<HealthCheckResponse, Status>) -> Result<ServingStatus, String> {
    message
        .map(|m| ServingStatus::try_from(m.status).expect("a known status"))
        .map_err(|e| e.message().to_owned())
}

/// The listeners' default timing is the one `StreamTiming::default` holds: tests built on
/// either one test what `kbf-server` and `kbf-frontend` run with.
#[test]
fn the_flags_default_timing_is_stream_timings_default() {
    assert_eq!(default_timing(), StreamTiming::default());
}

/// Catches a keepalive that closes a connection whose path stalls for as long as the farm's
/// uplink was seen to stall: the server PINGs while the path is stalled, the answer arrives
/// once it carries on, and the open stream, which would be an Execute stream, goes on and
/// takes its next message. With the old default (an answer within 20 s) the server sent
/// GOAWAY and closed the connection, and the stream failed "error reading a body from
/// connection".
#[tokio::test(start_paused = true)]
async fn a_stream_outlasts_a_path_that_stalls_for_a_minute() {
    let mut rig = Rig::open(default_timing()).await;

    rig.stall_for(OBSERVED_STALL).await;

    rig.readiness.stop();
    assert_eq!(
        status(next(&mut rig.watch).await),
        Ok(ServingStatus::NotServing),
        "the stream broke across the stall"
    );
}

/// The same with the stall one second short of the longest a quiet connection waits for
/// its answer (the PING, an interval after the last read, then the timeout): still served.
/// With the next test, the boundary: catches a PING sent earlier than an interval after
/// the last read, or a timeout shorter than the one set.
#[tokio::test(start_paused = true)]
async fn a_stream_outlasts_a_stall_a_second_short_of_the_ping_and_its_timeout() {
    let timing = default_timing();
    let mut rig = Rig::open(timing).await;

    rig.stall_for(timing.keepalive_interval + timing.keepalive_timeout - Duration::from_secs(1))
        .await;

    rig.readiness.stop();
    assert_eq!(
        status(next(&mut rig.watch).await),
        Ok(ServingStatus::NotServing),
        "the stream broke across the stall"
    );
}

/// A client that is gone for good is still let go: with the path stalled for longer than
/// the PING interval and the keepalive timeout together, the server closes the connection,
/// and once the path carries again the client reads the close, not another message.
/// Catches a keepalive turned off, or its timeout ignored.
#[tokio::test(start_paused = true)]
async fn a_path_stalled_past_the_ping_and_its_timeout_is_closed() {
    let timing = default_timing();
    let mut rig = Rig::open(timing).await;

    rig.stall_for(timing.keepalive_interval + timing.keepalive_timeout + Duration::from_secs(1))
        .await;

    rig.readiness.stop();
    assert_eq!(
        status(next(&mut rig.watch).await),
        Err("h2 protocol error: error reading a body from connection".to_owned()),
        "the server kept a dead connection"
    );
}

/// The old default, as a control for the rig: a PING unanswered for 20 s closes the
/// connection that the minute's stall holds up, and the stream fails with the error the
/// CI builds' Execute streams failed with. Without this, a rig whose stall held nothing up
/// would pass the tests above.
#[tokio::test(start_paused = true)]
async fn with_a_twenty_second_timeout_the_minutes_stall_breaks_the_stream() {
    let timing = StreamTiming {
        keepalive_timeout: Duration::from_secs(20),
        ..default_timing()
    };
    let mut rig = Rig::open(timing).await;

    rig.stall_for(OBSERVED_STALL).await;

    rig.readiness.stop();
    assert_eq!(
        status(next(&mut rig.watch).await),
        Err("h2 protocol error: error reading a body from connection".to_owned())
    );
}
