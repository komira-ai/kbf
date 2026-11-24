//! The listeners' HTTP/2 keepalive ([`keepalive`], and the watch on their connections,
//! [`crate::conn_watch`]) against a client whose path to the server stalls and then carries
//! on, as a CI runner's path through the tailnet does for up to a minute when the uplink in
//! front of the farm drops, and against one whose path is slow. The server PINGs a quiet
//! connection; a connection that moves nothing either way for the PING interval and the
//! keepalive timeout is closed, and every stream on it fails. Buck2 never recovers a broken
//! Execute stream, so each action on it fails.
//!
//! The client and the server talk through in-memory pipes and a relay that can stall them
//! or carry slowly ([`Path`]), on the paused clock: the stall is the time the test sleeps,
//! and the server's keepalive timers run on the same clock. The server's timing is what
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
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_client::HealthClient;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::grpc::health::v1::{HealthCheckRequest, HealthCheckResponse};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tonic::codegen::Service;
use tonic::codegen::http::Uri;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

use crate::config::Args;
use crate::conn_watch::{Watched, limit};
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
        Self::carrying(client, server, None)
    }

    /// A path between `client` and `server`, open, that carries at most `rate` bytes a
    /// second from the server to the client when there is one.
    fn carrying(client: DuplexStream, server: DuplexStream, rate: Option<u32>) -> Self {
        let (open, gate) = watch::channel(true);
        let (from_client, to_client) = tokio::io::split(client);
        let (from_server, to_server) = tokio::io::split(server);
        tokio::spawn(carry(from_client, to_server, gate.clone(), None));
        tokio::spawn(carry(from_server, to_client, gate, rate));
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
/// read only once open again, and with a `rate` it takes a second for each `rate` bytes it
/// carries. At the end of `from` it ends `to`.
async fn carry(
    mut from: ReadHalf<DuplexStream>,
    mut to: WriteHalf<DuplexStream>,
    mut gate: watch::Receiver<bool>,
    rate: Option<u32>,
) {
    let mut chunk = vec![0; 16 * 1024];
    loop {
        if let Some(rate) = rate {
            let read = u32::try_from(chunk.len()).expect("a 16 KiB chunk");
            tokio::time::sleep(Duration::from_secs(1) * read / rate).await;
        }
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

/// `server_end` watched for `limit` ([`Watched`]), as the listeners watch each of theirs,
/// and then an incoming that never ends: an incoming that ends stops the server.
fn one_connection(
    server_end: DuplexStream,
    limit: Option<Duration>,
) -> impl futures::Stream<Item = io::Result<Watched<DuplexStream>>> {
    futures::stream::iter([Ok(Watched::new(server_end, limit, None))])
        .chain(futures::stream::pending())
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
        let server = keepalive(Server::builder(), timing).add_service(health);
        tokio::spawn(server.serve_with_incoming(one_connection(server_end, limit(timing))));

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

/// The old default timeout, as a control for the rig: with 20 s, the connection that the
/// minute's stall holds up moves nothing for longer than the PING interval and the timeout,
/// and is closed, and the stream fails with the error the CI builds' Execute streams failed
/// with. Without this, a rig whose stall held nothing up would pass the tests above.
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

/// What one ByteStream message of [`Blob`] carries.
const MESSAGE_BYTES: usize = 64 * 1024;

/// A ByteStream service whose every read is `size` bytes, in messages of [`MESSAGE_BYTES`]:
/// a download, as buck2's reads of an action's outputs are.
struct Blob {
    size: usize,
}

#[tonic::async_trait]
impl ByteStream for Blob {
    type ReadStream = futures::stream::BoxStream<'static, Result<ReadResponse, Status>>;

    async fn read(&self, _: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let messages = (0..self.size / MESSAGE_BYTES).map(|_| {
            Ok(ReadResponse {
                data: vec![0; MESSAGE_BYTES],
            })
        });
        Ok(Response::new(futures::stream::iter(messages).boxed()))
    }

    async fn write(
        &self,
        _: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        Err(Status::unimplemented("only reads here"))
    }

    async fn query_write_status(
        &self,
        _: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("only reads here"))
    }
}

/// A client of a [`Blob`] of `size` bytes served by `server`, whose connection is watched
/// for `limit`, over a [`Path`] that carries at most `rate` bytes a second to the client,
/// and the path.
async fn blob_client(
    mut server: Server,
    limit: Option<Duration>,
    size: usize,
    rate: Option<u32>,
) -> (Path, ByteStreamClient<Channel>) {
    let (client_end, path_client) = tokio::io::duplex(PIPE_BYTES);
    let (path_server, server_end) = tokio::io::duplex(PIPE_BYTES);
    let path = Path::carrying(path_client, path_server, rate);
    let server = server.add_service(ByteStreamServer::new(Blob { size }));
    tokio::spawn(server.serve_with_incoming(one_connection(server_end, limit)));
    let channel = Endpoint::from_static("http://kbf.test")
        .connect_with_connector(OnePipe(Some(client_end)))
        .await
        .expect("connected over the pipe");
    (path, ByteStreamClient::new(channel))
}

/// Reads the whole blob `client` serves: the bytes it read, or the error the read ended
/// with.
async fn download(client: &mut ByteStreamClient<Channel>) -> Result<usize, String> {
    download_across(client, None).await
}

/// [`download`], with the path stalled for `stall.1` once the first message has crossed
/// when there is a `stall`: a download the path stops carrying in its middle.
async fn download_across(
    client: &mut ByteStreamClient<Channel>,
    stall: Option<(&Path, Duration)>,
) -> Result<usize, String> {
    let read = async {
        let mut stream = client.read(ReadRequest::default()).await?.into_inner();
        let mut bytes = 0;
        if let Some((path, stall)) = stall {
            bytes += stream.message().await?.map_or(0, |m| m.data.len());
            path.stall();
            let resume = async {
                tokio::time::sleep(stall).await;
                path.resume();
            };
            let (rest, ()) = tokio::join!(read_rest(&mut stream), resume);
            return Ok(bytes + rest?);
        }
        Ok(bytes + read_rest(&mut stream).await?)
    };
    let read = tokio::time::timeout(HANG, read).await;
    read.expect("the download neither ended nor failed")
        .map_err(|e: Status| e.message().to_owned())
}

/// The bytes of the messages left on `stream`.
async fn read_rest(stream: &mut Streaming<ReadResponse>) -> Result<usize, Status> {
    let mut bytes = 0;
    while let Some(message) = stream.message().await? {
        bytes += message.data.len();
    }
    Ok(bytes)
}

/// A download that the client reads all of the time, over a path too slow to carry what the
/// server has already written within the keepalive timeout: 1 MiB in the pipe to the path
/// at 4 KiB/s is about 256 s, as 16 MiB of window and socket buffers is at 64 KiB/s. The
/// client reads nothing for a while (its window updates come once it has read half its
/// window), so the server PINGs, and its PING waits behind that megabyte.
const SLOW_PATH_BYTES_PER_SEC: u32 = 4 * 1024;

/// The blob of the slow path's downloads: 512 s at its rate, so the server PINGs more than
/// once while it is read.
const SLOW_BLOB: usize = 2 * 1024 * 1024;

/// Catches a keepalive that takes a slow answer for a lost client: a download the client
/// goes on reading over a slow path runs to its end, its connection open all along. The
/// path carries the server's bytes the whole time; only the PING's answer is late, because
/// the PING was written behind the download's bytes.
#[tokio::test(start_paused = true)]
async fn a_download_over_a_slow_path_runs_to_its_end() {
    let timing = default_timing();
    let server = keepalive(Server::builder(), timing);
    let (_path, mut client) = blob_client(
        server,
        limit(timing),
        SLOW_BLOB,
        Some(SLOW_PATH_BYTES_PER_SEC),
    )
    .await;

    assert_eq!(download(&mut client).await, Ok(SLOW_BLOB));
}

/// The control for the test above: with the PING's answer as the test of the client, as
/// it was (hyper's own PING timeout, the default interval and timeout, no watch), the same
/// download over the same path breaks, with the error the CI builds' Execute streams failed
/// with. Without this, a rig whose slow path held no PING up would pass the test above.
#[tokio::test(start_paused = true)]
async fn with_the_pings_answer_as_the_test_the_slow_download_breaks() {
    let timing = default_timing();
    let server = Server::builder()
        .http2_keepalive_interval(Some(timing.keepalive_interval))
        .http2_keepalive_timeout(Some(timing.keepalive_timeout));
    let (_path, mut client) =
        blob_client(server, None, SLOW_BLOB, Some(SLOW_PATH_BYTES_PER_SEC)).await;

    assert_eq!(
        download(&mut client).await,
        Err("h2 protocol error: error reading a body from connection".to_owned())
    );
}

/// What the watch counts as a download moving must not keep a client that is gone: with
/// the path stalled in the middle of a download for longer than the PING interval and the
/// keepalive timeout, the server has megabytes it cannot send, and closes the connection.
/// Catches a watch that counts a write as the connection moving although the socket took
/// it only into its buffer.
#[tokio::test(start_paused = true)]
async fn a_download_stalled_past_the_ping_and_its_timeout_is_closed() {
    let timing = default_timing();
    let server = keepalive(Server::builder(), timing);
    let (path, mut client) = blob_client(server, limit(timing), SLOW_BLOB, None).await;
    let stall = timing.keepalive_interval + timing.keepalive_timeout + Duration::from_secs(1);

    assert_eq!(
        download_across(&mut client, Some((&path, stall))).await,
        Err("h2 protocol error: error reading a body from connection".to_owned())
    );
}

/// With the next test, the boundary of the one above: a download whose path stalls one
/// second short of the PING interval and the keepalive timeout carries on once the path
/// does, and runs to its end.
#[tokio::test(start_paused = true)]
async fn a_download_stalled_a_second_short_of_the_ping_and_its_timeout_runs_to_its_end() {
    let timing = default_timing();
    let server = keepalive(Server::builder(), timing);
    let (path, mut client) = blob_client(server, limit(timing), SLOW_BLOB, None).await;
    let stall = timing.keepalive_interval + timing.keepalive_timeout - Duration::from_secs(1);

    assert_eq!(
        download_across(&mut client, Some((&path, stall))).await,
        Ok(SLOW_BLOB)
    );
}

/// Catches a watch that closes a connection only for having no stream open: a client that
/// makes a call, then holds its connection with nothing open for longer than the PING
/// interval and the keepalive timeout, makes its next call on the same connection (the
/// rig's client cannot dial a second one). The server PINGs an idle connection too, and the
/// answers move it.
#[tokio::test(start_paused = true)]
async fn an_idle_connection_outlasts_the_ping_and_its_timeout() {
    let timing = default_timing();
    let (client_end, server_end) = tokio::io::duplex(PIPE_BYTES);
    let readiness = Arc::new(Readiness::default());
    let checks = Arc::new(Checks::new(Arc::clone(&readiness)));
    let health = HealthServer::new(HealthService::of(checks, Duration::from_secs(5)));
    let server = keepalive(Server::builder(), timing).add_service(health);
    tokio::spawn(server.serve_with_incoming(one_connection(server_end, limit(timing))));
    let channel = Endpoint::from_static("http://kbf.test")
        .connect_with_connector(OnePipe(Some(client_end)))
        .await
        .expect("connected over the pipe");
    let mut client = HealthClient::new(channel);
    let check = HealthCheckRequest::default();
    client.check(check.clone()).await.expect("the first call");

    tokio::time::sleep(timing.keepalive_interval + timing.keepalive_timeout * 2).await;

    let answer = client.check(check).await.map(|r| r.into_inner().status);
    assert_eq!(
        answer.map_err(|e| e.message().to_owned()),
        Ok(ServingStatus::Serving as i32)
    );
}
