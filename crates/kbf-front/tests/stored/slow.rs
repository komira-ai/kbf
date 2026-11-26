//! A ByteStream Write whose message is slow to arrive, as a client of a frontend with no storage
//! node sees it: the upload idle timeout counts the bytes of the body, not whole messages, so an
//! upload that keeps sending is stored however long one message takes, and one that stops is
//! still ended DEADLINE_EXCEEDED. Each upload is sent as raw HTTP/2 data frames that the test
//! lets through one at a time, so that one message arrives a few bytes at a time. The clock is
//! the runtime's, paused, so the gaps between frames and the timeout pass when nothing else can
//! run, not by waiting.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::Frame;
use kbf_front::PackSettings;
use kbf_proto::google::bytestream::{WriteRequest, WriteResponse};
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::codegen::{Service as _, http};
use tonic::{Code, Status};

use crate::bytestream::{read, read_name, status, upload_name};
use crate::cas::missing;
use crate::rig::{Rig, SMALL, blob_of};

/// The idle timeout of these tests: not the default, so that a receive that ignored the setting
/// would be seen.
const IDLE: Duration = Duration::from_secs(20);

/// The time between two frames of a slow upload: well inside the idle timeout.
const GAP: Duration = Duration::from_secs(10);

/// The bytes of a slow upload's message that each frame carries.
const PIECE: usize = 4;

/// Packs that take blobs of at most [`SMALL`] bytes, share `room` bytes among the uploads, and
/// end an upload that gets no byte for [`IDLE`].
async fn rig_with_room(room: u32) -> Rig {
    let settings = PackSettings {
        max_blob_bytes: SMALL,
        inflight_bytes: room,
        upload_idle_timeout: IDLE,
        ..PackSettings::default()
    };
    Rig::with(settings).await
}

/// A blob of 100 bytes, `text` over and over, and its digest: each test's text is its own.
fn hundred_bytes(text: &[u8]) -> (Digest, Bytes) {
    let bytes: Vec<u8> = text.iter().copied().cycle().take(100).collect();
    blob_of(&bytes)
}

/// A request body the test feeds: the frames sent arrive in order, and the body ends when the
/// sender is dropped.
struct Fed(mpsc::UnboundedReceiver<Bytes>);

impl http_body::Body for Fed {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        self.0
            .poll_recv(cx)
            .map(|bytes| bytes.map(|bytes| Ok(Frame::data(bytes))))
    }
}

/// `message` as it goes on the wire: gRPC's flag byte and length, then the message.
fn wire(message: &WriteRequest) -> Vec<u8> {
    let encoded = message.encode_to_vec();
    let mut out = vec![0];
    out.extend_from_slice(&u32::try_from(encoded.len()).expect("short").to_be_bytes());
    out.extend_from_slice(&encoded);
    out
}

/// The first message of an upload of `digest`: its name and the first `upto` bytes of `bytes`.
fn first(digest: &Digest, bytes: &[u8], upto: usize) -> WriteRequest {
    WriteRequest {
        resource_name: upload_name(digest),
        write_offset: 0,
        finish_write: false,
        data: bytes[..upto].to_vec(),
    }
}

/// The message that finishes an upload: the bytes of `bytes` from `from`.
fn rest(bytes: &[u8], from: usize) -> WriteRequest {
    WriteRequest {
        resource_name: String::new(),
        write_offset: i64::try_from(from).expect("small"),
        finish_write: true,
        data: bytes[from..].to_vec(),
    }
}

/// Starts a Write whose body the returned sender feeds, and the task that says how it ended:
/// the size it was acknowledged with, or the status that ended it.
fn start(
    rig: &Rig,
) -> (
    mpsc::UnboundedSender<Bytes>,
    JoinHandle<Result<i64, Status>>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut request = http::Request::new(tonic::body::Body::new(Fed(receiver)));
    *request.method_mut() = http::Method::POST;
    *request.uri_mut() = http::Uri::from_static("/google.bytestream.ByteStream/Write");
    let headers = request.headers_mut();
    headers.insert(
        "content-type",
        http::HeaderValue::from_static("application/grpc"),
    );
    headers.insert("te", http::HeaderValue::from_static("trailers"));
    let mut channel = rig.front.clone();
    let call = tokio::spawn(async move {
        poll_fn(|cx| channel.poll_ready(cx))
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let response = channel
            .call(request)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        ended(response).await
    });
    (sender, call)
}

/// How a Write's response ended: the committed size of its message, or the status in its
/// headers (a call refused before it answered) or its trailers.
async fn ended(response: http::Response<tonic::body::Body>) -> Result<i64, Status> {
    let (head, mut body) = response.into_parts();
    if let Some(status) = Status::from_header_map(&head.headers) {
        return Err(status);
    }
    let mut answer = Vec::new();
    while let Some(frame) = poll_fn(|cx| http_body::Body::poll_frame(Pin::new(&mut body), cx)).await
    {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            answer.extend_from_slice(data);
        }
        if let Some(status) = frame.trailers_ref().and_then(Status::from_header_map)
            && status.code() != Code::Ok
        {
            return Err(status);
        }
    }
    let message = answer.get(5..).expect("one framed message");
    let answer = WriteResponse::decode(message).expect("a WriteResponse");
    Ok(answer.committed_size)
}

/// Sends `bytes` as frames of [`PIECE`] bytes, [`GAP`] apart.
async fn trickle(sender: &mpsc::UnboundedSender<Bytes>, bytes: &[u8]) {
    for piece in bytes.chunks(PIECE) {
        sender
            .send(Bytes::copy_from_slice(piece))
            .expect("the call reads on");
        tokio::time::sleep(GAP).await;
    }
}

/// Asserts that an upload whose second message arrives [`PIECE`] bytes every [`GAP`] (inside
/// the idle timeout), and so takes more than ten times the idle timeout to arrive in full, is
/// not cut: it is acknowledged with its size, held, and read back in the bytes that were sent.
/// Catches an idle timeout counted from whole message to whole message.
#[tokio::test(start_paused = true)]
async fn a_message_slower_than_the_idle_timeout_that_keeps_coming_is_stored() {
    let rig = rig_with_room(1024).await;
    let (digest, bytes) = hundred_bytes(b"keeps coming ");
    let (sender, call) = start(&rig);
    sender
        .send(Bytes::from(wire(&first(&digest, &bytes, 10))))
        .expect("sent");

    let slow = wire(&rest(&bytes, 10));
    assert!(GAP * u32::try_from(slow.len().div_ceil(PIECE)).expect("few") > IDLE * 10);
    trickle(&sender, &slow).await;
    let written = call.await.expect("joined");

    assert_eq!(written.expect("acknowledged"), 100);
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
    let back = read(&rig, &read_name(&digest), 0, 0).await.expect("read");
    assert_eq!(back, bytes.to_vec());
}

/// Asserts that an upload whose second message comes a few bytes at a time and then stops,
/// halfway, is ended DEADLINE_EXCEEDED with the message that names the blob and the idle
/// timeout, and holds nothing: bytes that came before do not keep a silent upload open.
#[tokio::test(start_paused = true)]
async fn a_message_that_stops_coming_partway_is_ended_deadline_exceeded() {
    let rig = rig_with_room(1024).await;
    let (digest, bytes) = hundred_bytes(b"stops partway ");
    let (sender, call) = start(&rig);
    sender
        .send(Bytes::from(wire(&first(&digest, &bytes, 10))))
        .expect("sent");

    let slow = wire(&rest(&bytes, 10));
    trickle(&sender, &slow[..slow.len() / 2]).await;
    let ended = call.await.expect("joined").expect_err("ended");

    assert_eq!(ended.code(), Code::DeadlineExceeded);
    assert_eq!(
        ended.message(),
        format!("no byte of {digest} arrived for {IDLE:?}")
    );
    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), vec![digest]);
    drop(sender);
}

/// Asserts that the time an upload waits for room is not idle time: with room for one upload,
/// a second upload waits while the first holds it, the first goes silent and is ended at the
/// idle timeout, and the second, which got no byte from its client for longer than the idle
/// timeout but had not been reading, is stored when its last message comes half the idle
/// timeout after it got the room.
#[tokio::test(start_paused = true)]
async fn the_time_an_upload_waits_for_room_is_not_idle() {
    let rig = rig_with_room(100).await;
    let (holder, holder_bytes) = hundred_bytes(b"holds the room ");
    let (waiter, waiter_bytes) = hundred_bytes(b"waits for room ");
    let (holding, held) = start(&rig);
    holding
        .send(Bytes::from(wire(&first(&holder, &holder_bytes, 10))))
        .expect("sent");
    while status(&rig, &upload_name(&holder)).await.expect("asked") != (10, false) {
        tokio::task::yield_now().await;
    }
    let (waiting, stored) = start(&rig);
    waiting
        .send(Bytes::from(wire(&first(&waiter, &waiter_bytes, 10))))
        .expect("sent");

    let silent = held.await.expect("joined").expect_err("ended");
    assert_eq!(silent.code(), Code::DeadlineExceeded);
    tokio::time::sleep(IDLE / 2).await;
    waiting
        .send(Bytes::from(wire(&rest(&waiter_bytes, 10))))
        .expect("sent");
    let written = stored.await.expect("joined");

    assert_eq!(written.expect("acknowledged"), 100);
    assert!(missing(&rig, &[waiter]).await.expect("asked").is_empty());
    drop(holding);
}
