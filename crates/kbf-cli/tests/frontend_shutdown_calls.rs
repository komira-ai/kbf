//! Stopping a `kbf frontend` process while calls other than an Execute are in flight. The
//! frontend is the `kbf` binary of this build, started with the flags a farm gives it, over a
//! state store, so a stop signal runs its drain sequence (`frontend_shutdown/rig.rs`).
//!
//! - A large `BatchReadBlobs` and a ByteStream read that a slow client is still reading when the
//!   drain has nothing else to wait for are written to their end before the process exits, and
//!   an Execute stream open beside them gets its answer during the drain.
//! - When the operator's bound (`--shutdown-timeout-secs`) runs out, a call whose handler is
//!   still at work is answered UNAVAILABLE, which a client makes again, and is not cut by the
//!   exit.
//!
//! The client of these calls speaks HTTP/2 itself (the `h2` crate) over a connection it reads
//! as slowly as a test needs, and not at all while it is held: what a client far away, or busy,
//! is to a frontend that writes it a large answer.

#![cfg(unix)]

#[path = "frontend_shutdown/rig.rs"]
mod rig;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::{Buf as _, Bytes, BytesMut};
use kbf_proto::google::bytestream::{ReadRequest, ReadResponse, WriteRequest};
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest, ExecuteResponse,
    batch_update_blobs_request,
};
use prost::Message as _;
use rig::{Blob, PROMPT, Rig, next};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tonic::Code;
use tonic::codegen::http;

/// The size of each large answer: more than the frontend's connection and the sockets between
/// it and the client hold at once (hyper keeps 400 KiB of a stream's data, the client's
/// socket [`RECEIVE_BUFFER`]), so while the client is held the frontend still has part of
/// each answer to write.
const LARGE: usize = 8 << 20;

/// The flow-control windows the slow client gives, stream and connection: larger than both
/// answers, so only the client's reading holds an answer back, never the window.
const WINDOW: u32 = 64 << 20;

/// The slow client's socket receive buffer.
const RECEIVE_BUFFER: u32 = 64 << 10;

/// What the slow client reads at most at a time, fast and slow, and how long it waits after
/// each read.
const FAST_SIP: usize = 256 << 10;
const SLOW_SIP: usize = 8 << 10;
const PAUSE: Duration = Duration::from_millis(10);

/// The last bytes of the connection the slow client reads slowly: about two and a half seconds
/// of reading, many times what a frontend that exits once it has handed over the last frame of
/// its answers takes to exit, so such a frontend is caught with part of an answer unwritten.
const SLOW_TAIL: usize = 2 << 20;

/// A connection a client reads slowly, and not at all while `held` is set: [`FAST_SIP`] at a
/// time until `fast_for` bytes are read, then [`SLOW_SIP`], each read followed by [`PAUSE`].
struct Slow {
    tcp: TcpStream,
    held: Arc<AtomicBool>,
    fast_for: usize,
    read: Arc<AtomicUsize>,
    wait: Pin<Box<tokio::time::Sleep>>,
}

impl Slow {
    fn pause(&mut self) {
        let next = tokio::time::Instant::now() + PAUSE;
        self.wait.as_mut().reset(next);
    }
}

impl AsyncRead for Slow {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            ready!(self.wait.as_mut().poll(cx));
            if !self.held.load(Ordering::SeqCst) {
                break;
            }
            self.pause();
        }
        let sip = if self.read.load(Ordering::SeqCst) < self.fast_for {
            FAST_SIP
        } else {
            SLOW_SIP
        };
        let mut space = vec![0_u8; buf.remaining().min(sip)];
        let mut part = ReadBuf::new(&mut space);
        ready!(Pin::new(&mut self.tcp).poll_read(cx, &mut part))?;
        let got = part.filled();
        buf.put_slice(got);
        if !got.is_empty() {
            self.read.fetch_add(got.len(), Ordering::SeqCst);
            self.pause();
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Slow {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.tcp).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_shutdown(cx)
    }
}

/// An HTTP/2 client of the REAPI listener at `reapi` over a [`Slow`] connection, and the task
/// that drives the connection. `held` holds its reading; it reads fast for `fast_for` bytes.
async fn slow_client(
    reapi: SocketAddr,
    held: &Arc<AtomicBool>,
    fast_for: usize,
) -> h2::client::SendRequest<Bytes> {
    // A small receive buffer, set before the connection, so the kernel holds little of an
    // answer the client has not read and the rest stays with the frontend, whose exit loses it.
    let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
    socket
        .set_recv_buffer_size(RECEIVE_BUFFER)
        .expect("a receive buffer size");
    let tcp: TcpStream = socket.connect(reapi).await.expect("connect");
    let io = Slow {
        tcp,
        held: Arc::clone(held),
        fast_for,
        read: Arc::new(AtomicUsize::new(0)),
        wait: Box::pin(tokio::time::sleep(Duration::ZERO)),
    };
    let (send, connection) = h2::client::Builder::new()
        .initial_window_size(WINDOW)
        .initial_connection_window_size(WINDOW)
        .handshake::<_, Bytes>(io)
        .await
        .expect("an HTTP/2 connection");
    // Ends when the frontend closes the connection; what that says is read off the calls.
    tokio::spawn(connection);
    send
}

/// `message` as one gRPC message: no compression, its length, its bytes.
fn framed(message: &impl prost::Message) -> Bytes {
    let body = message.encode_to_vec();
    let mut frame = Vec::with_capacity(body.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&u32::try_from(body.len()).expect("a size").to_be_bytes());
    frame.extend_from_slice(&body);
    Bytes::from(frame)
}

/// Opens the gRPC call `path` on `send` and sends `first`, the whole request when `last`.
/// Returns the head of the answer, still to come, and the request's stream.
async fn open(
    send: &h2::client::SendRequest<Bytes>,
    path: &str,
    first: &impl prost::Message,
    last: bool,
) -> (h2::client::ResponseFuture, h2::SendStream<Bytes>) {
    let mut ready = send
        .clone()
        .ready()
        .await
        .expect("the connection takes a call");
    let request = http::Request::post(format!("http://127.0.0.1{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .expect("a request");
    let (answer, mut sending) = ready
        .send_request(request, false)
        .expect("the call is sent");
    sending
        .send_data(framed(first), last)
        .expect("the message is sent");
    (answer, sending)
}

/// Makes the call `path` with `message` on `send` and waits for the head of the answer, which
/// must be one that has a body to come.
async fn call(
    send: &h2::client::SendRequest<Bytes>,
    path: &str,
    message: &impl prost::Message,
) -> h2::RecvStream {
    let (answer, _sent) = open(send, path, message, true).await;
    let head = tokio::time::timeout(PROMPT, answer)
        .await
        .expect("the head of the answer in time")
        .expect("the head of the answer");
    assert_eq!(head.status(), 200, "{path}");
    assert!(
        head.headers().get("grpc-status").is_none(),
        "{path} was answered in headers alone: {:?}",
        head.headers()
    );
    head.into_body()
}

/// Reads the answer `body` to its end and returns its messages, each without its prefix, and
/// the gRPC status its trailers give. A body that breaks off (its connection closed with part
/// of it unwritten) fails the test, saying how much of it came.
async fn read_to_end(mut body: h2::RecvStream, what: &str) -> (Vec<Bytes>, String) {
    let mut data = BytesMut::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap_or_else(|e| {
            panic!(
                "{what}: the answer broke off after {} bytes: {e}",
                data.len()
            )
        });
        body.flow_control()
            .release_capacity(chunk.len())
            .expect("capacity released");
        data.extend_from_slice(&chunk);
    }
    let trailers = body
        .trailers()
        .await
        .unwrap_or_else(|e| panic!("{what}: no trailers after {} bytes: {e}", data.len()))
        .unwrap_or_else(|| panic!("{what}: an answer with no trailers"));
    let status = trailers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("none")
        .to_owned();
    let mut data = data.freeze();
    let mut messages = Vec::new();
    while !data.is_empty() {
        assert!(data.len() >= 5, "{what}: a part of a message prefix");
        assert_eq!(data[0], 0, "{what}: a compressed message");
        let length = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
        let length = usize::try_from(length).expect("a size");
        data.advance(5);
        assert!(data.len() >= length, "{what}: a part of a message");
        messages.push(data.split_to(length));
    }
    (messages, status)
}

/// `size` bytes no two of the blobs of a test share.
fn pattern(size: usize, seed: u8) -> Vec<u8> {
    (0..size)
        .map(|i| u8::try_from(i % 251).expect("a byte") ^ seed)
        .collect()
}

/// The ByteStream resource of `blob`.
fn resource(blob: &Blob) -> String {
    format!("blobs/{}/{}", blob.digest.hash, blob.digest.size_bytes)
}

/// Asserts what a frontend that is stopped owes the calls it took, whatever their kind: a large
/// `BatchReadBlobs` and a large ByteStream read, made by a client that then stops reading, and an
/// Execute stream whose job runs. SIGTERM comes while all three are in flight; the drain sends
/// GOAWAY and waits. The job then ends (lost by its host three times, the last attempt spent) and
/// the Execute stream gets the done operation that says so, on the stream it began on. Only the
/// two reads are left, both still being written: the client reads them on, the end of the
/// connection slowly, and each answer comes to its end, every byte of the blob and an OK status.
/// The frontend then exits 0 by itself.
///
/// Catches a frontend that exits once the last frame of each answer has been handed to its
/// connection, which still holds bytes of it for the client, so the answer breaks off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_still_being_written_at_the_end_of_the_drain_are_written_to_their_end() {
    let mut rig = Rig::start("slow-reads", &[]).await;
    let batch = Blob::raw(pattern(LARGE, 1));
    let streamed = Blob::raw(pattern(LARGE, 2));
    rig.upload(&[&batch]).await;
    rig.upload(&[&streamed]).await;
    let (mut executed, _) = rig.run_on_a_host("answered beside two reads").await;
    let first = rig.job_submitted().await;

    let held = Arc::new(AtomicBool::new(false));
    let everything = 2 * LARGE;
    let send = slow_client(rig.reapi, &held, everything - SLOW_TAIL).await;
    let batch_answer = call(
        &send,
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
        &BatchReadBlobsRequest {
            instance_name: "main".to_owned(),
            digests: vec![batch.digest.clone()],
            ..Default::default()
        },
    )
    .await;
    let read_answer = call(
        &send,
        "/google.bytestream.ByteStream/Read",
        &ReadRequest {
            resource_name: resource(&streamed),
            ..Default::default()
        },
    )
    .await;
    held.store(true, Ordering::SeqCst);

    rig.frontend.terminate();
    rig.not_ready().await;
    rig.draining().await;
    let mut job = first;
    for attempt in 1..=3 {
        assert!(rig.fake.lose(&job), "attempt {attempt} lost");
        if attempt < 3 {
            job = rig.job_after(&job).await;
            assert!(rig.fake.place(&job, "host-1"), "attempt {attempt} placed");
            assert!(rig.fake.starting(&job), "attempt {attempt} starting");
            assert!(rig.fake.run(&job), "attempt {attempt} running");
        }
    }
    let last = loop {
        let op = next(&mut executed).await;
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = &last.result else {
        panic!("a done operation without a response: {last:?}");
    };
    let answered = ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse");
    let status = answered.status.expect("the answer's status");
    assert_eq!(status.code, Code::Unavailable as i32, "{status:?}");
    assert!(
        rig.frontend.child.try_wait().expect("try_wait").is_none(),
        "the frontend exited with two answers being written"
    );

    held.store(false, Ordering::SeqCst);
    let reading_batch = tokio::spawn(read_to_end(batch_answer, "BatchReadBlobs"));
    let reading_stream = tokio::spawn(read_to_end(read_answer, "ByteStream Read"));
    let (messages, status) = reading_batch.await.expect("the read's task");
    assert_eq!(status, "0", "BatchReadBlobs");
    assert_eq!(messages.len(), 1, "BatchReadBlobs is one message");
    let answer = BatchReadBlobsResponse::decode(messages[0].clone()).expect("an answer");
    assert_eq!(answer.responses.len(), 1);
    let blob = &answer.responses[0];
    assert_eq!(blob.digest.as_ref(), Some(&batch.digest));
    assert_eq!(
        blob.status.as_ref().map_or(0, |s| s.code),
        0,
        "{:?}",
        blob.status
    );
    assert!(
        blob.data == batch.data,
        "BatchReadBlobs: not the blob's bytes"
    );
    let (messages, status) = reading_stream.await.expect("the read's task");
    assert_eq!(status, "0", "ByteStream Read");
    let mut data = Vec::with_capacity(LARGE);
    for message in messages {
        let chunk = ReadResponse::decode(message).expect("a ReadResponse");
        data.extend_from_slice(&chunk.data);
    }
    assert!(
        data == streamed.data,
        "ByteStream Read: not the blob's bytes"
    );

    let exit = rig.frontend.exited(PROMPT).await;
    assert!(exit.success(), "kbf exited with {exit}");
    drop((send, executed));
}

/// Asserts that when the operator's bound runs out (`--shutdown-timeout-secs=1` after SIGTERM),
/// the calls whose handlers are still at work, an upload whose request has not all come (a
/// `BatchUpdateBlobs` whose request has not ended and a ByteStream write with its first chunk
/// sent, both counted as open), are answered UNAVAILABLE with the close's message, in headers alone,
/// and the frontend exits 0.
///
/// Catches a frontend whose hard stop leaves the calls it finds to the exit, which closes their
/// connection: the client sees a cancelled call, which it does not make again, in place of
/// UNAVAILABLE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_open_when_the_bound_runs_out_are_answered_unavailable() {
    let mut rig = Rig::start("bound", &["--shutdown-timeout-secs=1"]).await;
    let held = Arc::new(AtomicBool::new(false));
    let send = slow_client(rig.reapi, &held, usize::MAX).await;
    let blob = Blob::raw(pattern(64 << 10, 3));
    let (batch_answer, _batch_rest) = open(
        &send,
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
        &BatchUpdateBlobsRequest {
            instance_name: "main".to_owned(),
            requests: vec![batch_update_blobs_request::Request {
                digest: Some(blob.digest.clone()),
                data: blob.data.clone(),
                ..Default::default()
            }],
            ..Default::default()
        },
        // The message without the end of the request, which the call waits for.
        false,
    )
    .await;
    let (write_answer, _write_rest) = open(
        &send,
        "/google.bytestream.ByteStream/Write",
        &WriteRequest {
            resource_name: format!(
                "uploads/u/blobs/{}/{}",
                blob.digest.hash, blob.digest.size_bytes
            ),
            data: blob.data[..1024].to_vec(),
            ..Default::default()
        },
        false,
    )
    .await;
    // Both calls are counted, so the drain waits for them.
    let deadline = tokio::time::Instant::now() + PROMPT;
    while gauge(&rig, "kbf_front_open_calls").await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the uploads are not counted"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    rig.frontend.terminate();
    for (what, answer) in [
        ("BatchUpdateBlobs", batch_answer),
        ("ByteStream Write", write_answer),
    ] {
        let head = tokio::time::timeout(PROMPT, answer)
            .await
            .unwrap_or_else(|_| panic!("{what}: no answer in time"))
            .unwrap_or_else(|e| panic!("{what}: no answer, the connection ended: {e}"));
        let headers = head.headers();
        assert_eq!(
            headers.get("grpc-status").and_then(|v| v.to_str().ok()),
            Some("14"),
            "{what}: {headers:?}"
        );
        assert_eq!(
            headers.get("grpc-message").and_then(|v| v.to_str().ok()),
            Some("the server is shutting down; this call was not finished, make it again"),
            "{what}"
        );
    }
    let exit = rig.frontend.exited(PROMPT).await;
    assert!(exit.success(), "kbf exited with {exit}");
    drop(send);
}

/// The value of the gauge `name` on the frontend's `/metrics` page.
async fn gauge(rig: &Rig, name: &str) -> u64 {
    let (code, page) = rig.get("/metrics").await;
    assert_eq!(code, 200, "{page}");
    let prefix = format!("{name} ");
    page.lines()
        .find_map(|line| {
            line.strip_prefix(&prefix)
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or_else(|| panic!("no gauge {name} on the page"))
}
