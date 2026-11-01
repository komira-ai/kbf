//! ByteStream calls over the packs, for the raw path of the proxy, which never decodes a
//! message: a `Read` it has collected whole and a `Write` it hands over as a body. The gRPC
//! messages are cut and made here by hand, a message being a flag byte, a four-byte length
//! and the protobuf.
//!
//! A `Read` is served from the blob as the packs hold it, hashed, cut to
//! `[read_offset, read_offset + read_limit)` and sent in messages of at most
//! [`READ_CHUNK_BYTES`](crate::READ_CHUNK_BYTES), each made as it is polled. A `Write` takes
//! the bytes its resource name claims from the room that uploads share (waiting for it, before
//! it reads on past the frames that carry the name), is received whole, message by message,
//! under the rules of the cache's ByteStream (the same resource, the next offset, no more than
//! the digest's size, `finish_write` to end), and packed. The buffer grows with the bytes that
//! arrive, a message longer than the rest of the blob, the name and the protobuf's own tags can
//! make it is refused at its length prefix, before any of it is buffered, and an upload that
//! gets no byte for [`Packs::upload_idle_timeout`] after the name ends DEADLINE_EXCEEDED.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body as _, Frame};
use kbf_proto::google::bytestream::{ReadRequest, ReadResponse, WriteRequest, WriteResponse};
use kbf_storage::in_flight;
use kbf_types::Digest;
use prost::Message as _;
use tokio::time::{Instant, timeout_at};
use tonic::Status;
use tonic::body::Body;
use tonic::codegen::http;

use super::Packs;
use crate::proxy::body::{compressed, trailers_of};
use crate::{MAX_MESSAGE_BYTES, READ_CHUNK_BYTES};

/// Whether a ByteStream call named `name` of the blob `digest` is answered by the packs: the
/// blob is within [`Packs::max_blob_bytes`] and the name is not that of compressed bytes.
pub(in crate::proxy) fn applies(packs: &Packs, name: &str, digest: &Digest) -> bool {
    let fits = digest.size_bytes <= packs.max_blob_bytes();
    let plain = !is_compressed(name);
    matches!((fits, plain), (true, true))
}

/// Whether `name` names compressed bytes (a `compressed-blobs/` segment), which the packs do
/// not hold: they hold the blobs as they are.
fn is_compressed(name: &str) -> bool {
    name.split('/').any(|part| part == "compressed-blobs")
}

/// A `Read` whose request is the gRPC message `request`, for the blob `digest` named `name`,
/// answered from the packs; `None` when the packs do not serve the blob and the storage
/// nodes are to be asked.
///
/// # Errors
/// INVALID_ARGUMENT for a request that is not a `ReadRequest` or a negative `read_limit`,
/// OUT_OF_RANGE for a negative `read_offset` or one past the end of the blob, and as the packs
/// answer.
pub(in crate::proxy) async fn read(
    packs: &Packs,
    name: &str,
    digest: &Digest,
    request: &[u8],
) -> Result<Option<http::Response<Body>>, Status> {
    if is_compressed(name) {
        return Ok(None);
    }
    let asked = ReadRequest::decode(message(request)?)
        .map_err(|error| Status::invalid_argument(format!("a ReadRequest: {error}")))?;
    let offset = u64::try_from(asked.read_offset).map_err(|_| {
        Status::out_of_range(format!("read_offset {} is negative", asked.read_offset))
    })?;
    let limit = u64::try_from(asked.read_limit).map_err(|_| {
        Status::invalid_argument(format!("read_limit {} is negative", asked.read_limit))
    })?;
    if offset > digest.size_bytes {
        let why = format!("read_offset {offset} is past the end of {digest}");
        return Err(Status::out_of_range(why));
    }
    let Some(blob) = packs.read(digest).await? else {
        return Ok(None);
    };
    let len = blob.len() as u64;
    let end = match limit {
        0 => len,
        n => len.min(offset.saturating_add(n)),
    };
    let wanted = blob.slice(offset as usize..end as usize);
    let body = ReadBody {
        rest: wanted,
        ended: false,
    };
    Ok(Some(grpc(Body::new(body))))
}

/// A `Write` whose request is `body` (all of it, from the first message), for the blob
/// `digest` named `name`: received whole, packed, and answered with the blob's size. The
/// bytes the name claims are taken from the room that uploads share before `body` is read on
/// past the frames that carry the name (which the caller has read), and given back when the
/// call ends, however it ends.
///
/// # Errors
/// INVALID_ARGUMENT for a stream that breaks the rules of an upload or whose bytes are not the
/// digest; UNAVAILABLE when the packs could not be written; RESOURCE_EXHAUSTED when the blob
/// alone is over all of the room; DEADLINE_EXCEEDED when no byte of `body` arrived for
/// [`Packs::upload_idle_timeout`] once the room was taken; or the client's own error if its
/// request broke.
pub(in crate::proxy) async fn write(
    packs: &Packs,
    body: Body,
    name: &str,
    digest: Digest,
) -> Result<http::Response<Body>, Status> {
    let room = packs.reserve(digest.size_bytes).await?;
    let received = receive(packs, body, name, &digest).await?;
    packs.put_in(vec![(digest, received)], room).await?;
    let committed = WriteResponse {
        committed_size: i64::try_from(digest.size_bytes).unwrap_or(i64::MAX),
    };
    let frames = VecDeque::from([
        Frame::data(framed(&committed.encode_to_vec())),
        Frame::trailers(trailers_of(&Status::ok(""))),
    ]);
    Ok(grpc(Body::new(Framed(frames))))
}

/// The bytes of the blob `digest` that the messages of `body` carry, counted as held
/// (`kbf_bytes_in_flight`) from the allocation until the packs are done with them. The buffer
/// grows with the bytes that arrive ([`grow`]), so an upload that claims a large blob and sends
/// nothing holds none of it here. The upload is given up if no byte of `body` arrives for the
/// idle timeout of `packs`: the time is counted from the last frame that carried a byte, so a
/// slow upload that keeps sending is never cut, and a frame without a byte (an empty data
/// frame, trailers) is not progress.
async fn receive(
    packs: &Packs,
    mut body: Body,
    name: &str,
    digest: &Digest,
) -> Result<Bytes, Status> {
    let size = digest.size_bytes;
    // The size is within the bound the caller checked (`applies`).
    let limit = usize::try_from(size).unwrap_or(usize::MAX);
    let idle = packs.upload_idle_timeout();
    let mut held = in_flight::hold(0);
    let mut received: Vec<u8> = Vec::new();
    let mut pending = Messages::default();
    let mut deadline = Instant::now() + idle;
    loop {
        while let Some(message) = pending.take_message(name, received.len() as u64, size)? {
            check(&message, name, received.len() as u64, size)?;
            grow(&mut received, message.data.len(), limit);
            held.add(received.capacity() as u64 - held.bytes());
            received.extend_from_slice(&message.data);
            if message.finish_write {
                return Ok(in_flight::counted_with(received, held));
            }
        }
        let next = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
        let Ok(frame) = timeout_at(deadline, next).await else {
            return Err(stalled(packs, digest, idle));
        };
        let frame = frame
            .ok_or_else(|| Status::invalid_argument("write stream ended without finish_write"))??;
        let data = frame.data_ref().map_or(&[][..], Bytes::as_ref);
        if !data.is_empty() {
            deadline = Instant::now() + idle;
        }
        pending.push(data);
    }
}

/// Makes room in `buf` for `more` bytes more, without ever holding room for more than `limit`
/// bytes (the size of the blob, which the bytes were checked to fit in): the capacity doubles
/// when it is short, so that a blob sent in many messages is copied a few times, not once per
/// message.
fn grow(buf: &mut Vec<u8>, more: usize, limit: usize) {
    let needed = buf.len() + more;
    if needed > buf.capacity() {
        let doubled = buf.capacity().saturating_mul(2);
        let target = doubled.max(needed).min(limit);
        buf.reserve_exact(target - buf.len());
    }
}

/// The end of an upload of `digest` that got no byte of its body for `idle`, counted and
/// logged: DEADLINE_EXCEEDED.
fn stalled(packs: &Packs, digest: &Digest, idle: Duration) -> Status {
    packs.metrics().upload_idle_timeouts.add(1);
    tracing::warn!(%digest, ?idle, "an upload into packs got no byte of its body in time: ended");
    Status::deadline_exceeded(format!(
        "no byte of the upload of {digest} arrived within {idle:?}"
    ))
}

/// The most the protobuf of a `WriteRequest` adds to its name and its data: the tags and
/// lengths of its four fields (at most 25 bytes), with room to spare.
const MESSAGE_TAGS_BYTES: u64 = 32;

/// Whether `message`, which comes after `received` bytes of an upload of `size` bytes named
/// `name`, keeps the rules of an upload.
fn check(message: &WriteRequest, name: &str, received: u64, size: u64) -> Result<(), Status> {
    if ![name, ""].contains(&message.resource_name.as_str()) {
        return Err(Status::invalid_argument(format!(
            "write stream changed resource from {name:?} to {:?}",
            message.resource_name
        )));
    }
    if u64::try_from(message.write_offset).ok() != Some(received) {
        return Err(Status::invalid_argument(format!(
            "write_offset {} but {received} bytes were received; uploads start at 0 and do not \
             resume",
            message.write_offset
        )));
    }
    if received + message.data.len() as u64 > size {
        let why = format!("more than the {size} bytes the resource names");
        return Err(Status::invalid_argument(why));
    }
    Ok(())
}

/// The bytes of a request body, cut into the gRPC messages they hold as they arrive.
#[derive(Default)]
struct Messages {
    buf: Vec<u8>,
}

impl Messages {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The next whole `WriteRequest`, if the bytes so far hold one, of the upload of `size`
    /// bytes named `name` that has received `received` of them. The length in the prefix is
    /// checked as soon as the prefix is there, so that no more is buffered for a message than
    /// the rest of the blob, the name and the protobuf's tags can make it.
    ///
    /// # Errors
    /// INVALID_ARGUMENT for a compressed message, one that is not a `WriteRequest`, or one
    /// whose length is over what the rest of the upload can carry; RESOURCE_EXHAUSTED for a
    /// message over the largest a call may carry.
    fn take_message(
        &mut self,
        name: &str,
        received: u64,
        size: u64,
    ) -> Result<Option<WriteRequest>, Status> {
        let Some(&[flag, a, b, c, d]) = self.buf.first_chunk::<5>() else {
            return Ok(None);
        };
        if flag != 0 {
            return Err(compressed("the message's compressed flag is set"));
        }
        let len = u32::from_be_bytes([a, b, c, d]) as usize;
        if len > MAX_MESSAGE_BYTES {
            return Err(Status::resource_exhausted(format!(
                "a message of {len} bytes is over the {MAX_MESSAGE_BYTES} a call may carry"
            )));
        }
        let rest = size.saturating_sub(received);
        let allowed = rest.saturating_add(name.len() as u64 + MESSAGE_TAGS_BYTES);
        if len as u64 > allowed {
            return Err(Status::invalid_argument(format!(
                "a message of {len} bytes carries more than the {rest} bytes left of the {size} \
                 the resource names"
            )));
        }
        let Some(proto) = self.buf.get(5..5 + len) else {
            return Ok(None);
        };
        let message = WriteRequest::decode(proto)
            .map_err(|error| Status::invalid_argument(format!("a WriteRequest: {error}")))?;
        self.buf.drain(..5 + len);
        Ok(Some(message))
    }
}

/// The protobuf of the first gRPC message of `request`, which its five-byte prefix gives the
/// length of.
fn message(request: &[u8]) -> Result<&[u8], Status> {
    let cut = || Status::invalid_argument("the request message is shorter than its length says");
    let head = request.first_chunk::<5>().ok_or_else(cut)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    request.get(5..5 + len).ok_or_else(cut)
}

/// `proto` as a gRPC message: no compression, its length, and the bytes.
fn framed(proto: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(5 + proto.len());
    out.push(0);
    out.extend_from_slice(&(proto.len() as u32).to_be_bytes());
    out.extend_from_slice(proto);
    Bytes::from(out)
}

/// A gRPC response with `body`, which ends in the trailers that say OK.
fn grpc(body: Body) -> http::Response<Body> {
    let mut response = http::Response::new(body);
    let content_type = http::HeaderValue::from_static("application/grpc");
    response.headers_mut().insert("content-type", content_type);
    response
}

/// The body of a `Read`: the blob in `ReadResponse` messages of at most
/// [`READ_CHUNK_BYTES`], each cut and made when it is polled so that the call holds the blob
/// and one message, then the trailers that say OK.
struct ReadBody {
    rest: Bytes,
    ended: bool,
}

impl http_body::Body for ReadBody {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        if !self.rest.is_empty() {
            let take = self.rest.len().min(READ_CHUNK_BYTES);
            let chunk = self.rest.split_to(take);
            let message = ReadResponse {
                data: chunk.to_vec(),
            };
            let frame = Frame::data(framed(&message.encode_to_vec()));
            return Poll::Ready(Some(Ok(frame)));
        }
        if std::mem::replace(&mut self.ended, true) {
            return Poll::Ready(None);
        }
        let trailers = trailers_of(&Status::ok(""));
        Poll::Ready(Some(Ok(Frame::trailers(trailers))))
    }
}

/// A body of frames made in advance.
struct Framed(VecDeque<Frame<Bytes>>);

impl http_body::Body for Framed {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::PackSettings;

    /// One gRPC message of `proto` with the flag byte `flag`.
    fn wire(flag: u8, proto: &[u8]) -> Vec<u8> {
        let mut out = vec![flag];
        out.extend_from_slice(&(proto.len() as u32).to_be_bytes());
        out.extend_from_slice(proto);
        out
    }

    fn write_request(name: &str, offset: i64, data: &[u8], finish: bool) -> WriteRequest {
        WriteRequest {
            resource_name: name.to_owned(),
            write_offset: offset,
            finish_write: finish,
            data: data.to_vec(),
        }
    }

    /// The next message of `messages` whatever the size of the upload: the only limit is the
    /// largest message a call may carry.
    fn take_any(messages: &mut Messages) -> Result<Option<WriteRequest>, Status> {
        messages.take_message("", 0, u64::MAX)
    }

    /// Asserts that a message is taken only when whole, however the bytes arrive: a cut
    /// before the end of the length prefix or of the message yields nothing, and the rest of
    /// the bytes are kept for the next message.
    #[test]
    fn a_message_is_taken_when_its_bytes_are_all_there() {
        let first = write_request("a/uploads/u/blobs/h/3", 0, b"abc", false);
        let second = write_request("", 3, b"de", true);
        let mut bytes = wire(0, &first.encode_to_vec());
        bytes.extend(wire(0, &second.encode_to_vec()));
        let whole = wire(0, &first.encode_to_vec()).len();
        let ends = [whole, bytes.len()];
        let mut messages = Messages::default();
        for (cut, byte) in bytes.iter().enumerate() {
            messages.push(&[*byte]);
            let taken = take_any(&mut messages).expect("well formed");
            assert_eq!(taken.is_some(), ends.contains(&(cut + 1)));
        }
        assert!(messages.buf.is_empty());
    }

    /// Asserts that the messages of a body are refused the way the cache's ByteStream would
    /// refuse them: a compressed flag, a length over what a call may carry, and protobuf
    /// that is not a `WriteRequest`.
    #[test]
    fn a_message_that_cannot_be_read_is_refused() {
        let mut compressed = Messages::default();
        compressed.push(&wire(1, b"x"));
        let refused = take_any(&mut compressed).expect_err("compressed");
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert!(refused.message().contains("compress"), "{refused}");

        let mut huge = Messages::default();
        huge.push(&[0, 0xff, 0xff, 0xff, 0xff]);
        let refused = take_any(&mut huge).expect_err("over the bound");
        assert_eq!(refused.code(), Code::ResourceExhausted);

        let mut garbage = Messages::default();
        garbage.push(&wire(0, &[0xff, 0xff]));
        let refused = take_any(&mut garbage).expect_err("not protobuf");
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert!(refused.message().contains("WriteRequest"), "{refused}");
    }

    /// Asserts that a message whose length prefix says it is longer than the rest of the blob,
    /// the name and the protobuf's tags can make it is refused INVALID_ARGUMENT as soon as the
    /// prefix is read, with none of its body there, and that a prefix of the longest message
    /// that can be made is not refused (nothing is taken until the body is there).
    #[test]
    fn a_message_longer_than_the_rest_of_the_blob_is_refused_at_its_prefix() {
        let mut declared = Messages::default();
        declared.push(&[0, 0, 0x50, 0, 0]);
        let taken = declared.take_message(NAME, 0, 10);
        let refused = taken.expect_err("5 MiB for 10 bytes");
        assert_eq!(refused.code(), Code::InvalidArgument);
        let said = refused.message();
        assert!(said.contains("more than the 10 bytes"), "{said}");

        let longest = u32::try_from(10 + NAME.len() + 32).expect("small");
        for (len, is_refused) in [(longest, false), (longest + 1, true)] {
            let mut prefix = Messages::default();
            prefix.push(&[0]);
            prefix.push(&len.to_be_bytes());
            let taken = prefix.take_message(NAME, 0, 10);
            assert_eq!(taken.is_err(), is_refused, "{len}");
        }
    }

    /// Asserts that the first message of a request is the protobuf its prefix says, and that
    /// a request cut short of its prefix or of its message is refused.
    #[test]
    fn the_first_message_of_a_request_is_cut_by_its_prefix() {
        let request = ReadRequest {
            resource_name: "x".to_owned(),
            read_offset: 1,
            read_limit: 2,
        };
        let proto = request.encode_to_vec();
        let mut bytes = wire(0, &proto);
        bytes.extend_from_slice(b"trailing");
        assert_eq!(message(&bytes).expect("whole"), proto.as_slice());
        for short in [&bytes[..3], &bytes[..bytes.len() - 9]] {
            let refused = message(short).expect_err("cut short");
            assert_eq!(refused.code(), Code::InvalidArgument);
        }
    }

    /// Packs over an in-memory store and index.
    fn packs() -> Packs {
        packs_with(PackSettings::default())
    }

    /// Packs over an in-memory store and index, as `settings` say.
    fn packs_with(settings: PackSettings) -> Packs {
        let create_only = kbf_objstore::Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        let store = std::sync::Arc::new(kbf_objstore::MemoryStore::new(create_only));
        let clock = std::sync::Arc::new(kbf_state::ManualClock::default());
        let index = kbf_pack_index::memory::MemoryPackIndex::new(clock);
        let index: std::sync::Arc<dyn kbf_pack_index::PackIndex> = std::sync::Arc::new(index);
        let keys = kbf_pack::PackKeys::new(kbf_pack::WriterKind::Frontend, "fe1", "boot1");
        let keys = keys.expect("keys");
        let made = Packs::new(store, index, keys, settings);
        made.expect("packs")
    }

    /// Asserts that a Read whose message is not a `ReadRequest` is refused INVALID_ARGUMENT
    /// before the packs are asked.
    #[tokio::test]
    async fn a_read_request_that_does_not_decode_is_refused() {
        let digest = kbf_segments::sha256(b"x");
        let garbage = wire(0, &[0xff, 0xff]);

        let refused = read(&packs(), "main/blobs/h/1", &digest, &garbage).await;

        let refused = refused.expect_err("refused");
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert!(refused.message().contains("ReadRequest"), "{refused}");
    }

    /// Asserts that a resource name is of compressed bytes when one of its segments is
    /// `compressed-blobs`, wherever it is, and not when the word is only part of a segment.
    #[test]
    fn a_resource_name_is_compressed_by_a_segment() {
        assert!(is_compressed("i/compressed-blobs/zstd/h/1"));
        assert!(is_compressed("compressed-blobs/zstd/h/1"));
        assert!(!is_compressed("i/blobs/h/1"));
        assert!(!is_compressed("i/blobsx/compressed-blobsx/1"));
    }

    /// Asserts that the body of a Read cuts the blob into messages of at most the chunk size
    /// as it is polled, then ends with trailers that say OK, then ends.
    #[tokio::test]
    async fn a_read_body_is_messages_then_ok_trailers() {
        let blob = Bytes::from(vec![7_u8; READ_CHUNK_BYTES + 5]);
        let mut body = ReadBody {
            rest: blob,
            ended: false,
        };
        let mut lens = Vec::new();
        let mut trailers = None;
        while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            let frame = frame.expect("no error");
            match frame.data_ref() {
                Some(data) => {
                    let proto = message(data).expect("a message");
                    let response = ReadResponse::decode(proto).expect("a ReadResponse");
                    lens.push(response.data.len());
                }
                None => trailers = frame.trailers_ref().and_then(Status::from_header_map),
            }
        }
        assert_eq!(lens, [READ_CHUNK_BYTES, 5]);
        assert_eq!(trailers.expect("trailers").code(), Code::Ok);
        let after = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        assert!(after.is_none());
    }

    /// Asserts that a body of frames made in advance gives them in order and ends.
    #[tokio::test]
    async fn a_framed_body_gives_its_frames_and_ends() {
        let frames = VecDeque::from([Frame::data(framed(b"ab"))]);
        let mut body = Framed(frames);
        let first = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        let first = first.expect("a frame").expect("no error");
        let data = first.into_data().expect("data");
        assert_eq!(data.as_ref(), [0, 0, 0, 0, 2, b'a', b'b']);
        let second = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        assert!(second.is_none());
    }

    /// The name every upload of these tests is made under.
    const NAME: &str = "main/uploads/u1/blobs/h/1";

    /// The idle timeout of these tests: not the default, so that a receive that ignored the
    /// timeout of its packs would be seen.
    const IDLE: Duration = Duration::from_secs(45);

    type Sender = tokio::sync::mpsc::UnboundedSender<Frame<Bytes>>;
    type Upload = tokio::task::JoinHandle<Result<(), Status>>;

    /// A request body fed by the test: the frames sent arrive in order, and the body ends when
    /// the sender is dropped.
    struct Fed(tokio::sync::mpsc::UnboundedReceiver<Frame<Bytes>>);

    impl http_body::Body for Fed {
        type Data = Bytes;
        type Error = Status;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
            self.0.poll_recv(cx).map(|frame| frame.map(Ok))
        }
    }

    /// The sender that feeds a body, and the body.
    fn fed() -> (Sender, Body) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (sender, Body::new(Fed(receiver)))
    }

    /// A blob of `len` bytes of `fill`, and its digest.
    fn blob_of(len: usize, fill: u8) -> (Digest, Bytes) {
        let bytes = Bytes::from(vec![fill; len]);
        (kbf_segments::sha256(&bytes), bytes)
    }

    /// Sends the message of a Write that puts `data` at `offset`, and ends the upload if
    /// `finish`.
    fn send(tx: &Sender, offset: i64, data: &[u8], finish: bool) {
        let message = write_request(NAME, offset, data, finish);
        let frame = Frame::data(Bytes::from(wire(0, &message.encode_to_vec())));
        // A body whose upload has ended takes nothing more.
        let _ = tx.send(frame);
    }

    /// A Write of `digest` over `body`, run as a task.
    fn upload(packs: &Packs, body: Body, digest: Digest) -> Upload {
        let packs = packs.clone();
        tokio::spawn(async move { write(&packs, body, NAME, digest).await.map(|_| ()) })
    }

    /// Lets every task that can run run until it waits: the paused clock moves on only when
    /// none can.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// Packs that hold `inflight` bytes of uploads at once and cut an upload that sends no byte
    /// for [`IDLE`].
    fn packs_for(inflight: u32) -> Packs {
        packs_with(PackSettings {
            inflight_bytes: inflight,
            upload_idle_timeout: IDLE,
            ..PackSettings::default()
        })
    }

    /// Asserts that an upload that stops sending is ended DEADLINE_EXCEEDED once the idle
    /// timeout has passed since its last byte, and not before; that it stores and records
    /// nothing, is counted, and gives back its bytes, so that an upload that was waiting for
    /// the room is admitted then and is stored. Asserts also that the upload counts the bytes it
    /// holds as the 10 that arrived, not the 60 it claimed: exactly under cargo-nextest, where
    /// this test has the process to itself; from below under `cargo test`, where other tests in
    /// the process hold bytes in the same gauge.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_upload_is_cut_and_its_room_goes_to_the_one_waiting() {
        let packs = packs_for(100);
        let (stalled, stalled_bytes) = blob_of(60, b'a');
        let (waiting, waiting_bytes) = blob_of(60, b'b');
        let (stalled_tx, body) = fed();
        let stalled_upload = upload(&packs, body, stalled);
        send(&stalled_tx, 0, &stalled_bytes[..10], false);
        let (waiting_tx, body) = fed();
        let waiting_upload = upload(&packs, body, waiting);
        send(&waiting_tx, 0, &waiting_bytes, true);
        let started = Instant::now();

        settle().await;
        let metrics = packs.metrics();
        assert_eq!(metrics.inflight_bytes(), 60);
        assert_eq!(metrics.upload_waits.get(), 1);
        let held = in_flight::process().current();
        // Alone in the process (cargo-nextest) the gauge holds only this test's bytes.
        let most = std::env::var_os("NEXTEST").map_or(u64::MAX, |_| 10);
        let arrived = 10..=most;
        assert!(
            arrived.contains(&held),
            "{held}: the 10 that arrived, not the 60 claimed"
        );
        assert!(!waiting_upload.is_finished());

        tokio::time::advance(IDLE - Duration::from_secs(1)).await;
        settle().await;
        assert!(!stalled_upload.is_finished(), "cut before the timeout");
        assert!(!waiting_upload.is_finished());

        let cut = stalled_upload.await.expect("joined").expect_err("cut");
        assert_eq!(cut.code(), Code::DeadlineExceeded);
        assert!(cut.message().contains("45s"), "{}", cut.message());
        assert!(started.elapsed() >= IDLE);
        waiting_upload.await.expect("joined").expect("stored");
        assert_eq!(metrics.upload_idle_timeouts.get(), 1);
        assert_eq!(metrics.inflight_bytes(), 0);
        assert_eq!(metrics.inflight_peak_bytes(), 60);
        let found = packs.read(&waiting).await.expect("read");
        assert_eq!(found, Some(waiting_bytes));
        let lost = packs.read(&stalled).await.expect("read");
        assert!(lost.is_none());
    }

    /// Asserts that an upload that sends a byte every 10 seconds short of the idle timeout, and
    /// so takes far longer in all than the idle timeout, is not cut and is stored: only a pause
    /// is.
    #[tokio::test(start_paused = true)]
    async fn an_upload_that_keeps_sending_is_never_cut() {
        let packs = packs_for(100);
        let (digest, bytes) = blob_of(10, b'x');
        let (tx, body) = fed();
        let running = upload(&packs, body, digest);
        let started = Instant::now();
        let pace = IDLE - Duration::from_secs(10);
        let last = bytes.len() - 1;
        let sent = bytes.clone();
        let feeder = tokio::spawn(async move {
            for (at, byte) in sent.iter().enumerate() {
                tokio::time::sleep(pace).await;
                let offset = i64::try_from(at).expect("small");
                send(&tx, offset, &[*byte], at == last);
            }
        });

        running.await.expect("joined").expect("not cut");

        feeder.await.expect("joined");
        assert!(started.elapsed() >= IDLE * 6);
        assert_eq!(packs.metrics().upload_idle_timeouts.get(), 0);
        let found = packs.read(&digest).await.expect("read");
        assert_eq!(found, Some(bytes));
    }

    /// Asserts that only bytes are progress: an upload that sends one byte and then empty data
    /// frames is cut the idle timeout after the byte, not after the last empty frame.
    #[tokio::test(start_paused = true)]
    async fn frames_without_a_byte_do_not_postpone_the_idle_timeout() {
        let packs = packs_for(100);
        let (digest, bytes) = blob_of(10, b'y');
        let (tx, body) = fed();
        let running = upload(&packs, body, digest);
        let started = Instant::now();
        send(&tx, 0, &bytes[..1], false);
        tokio::spawn(async move {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_secs(20)).await;
                let _ = tx.send(Frame::data(Bytes::new()));
            }
        });

        let cut = running.await.expect("joined").expect_err("cut");

        assert_eq!(cut.code(), Code::DeadlineExceeded);
        assert!(started.elapsed() >= IDLE);
        assert!(started.elapsed() < IDLE + Duration::from_secs(5));
        assert_eq!(packs.metrics().upload_idle_timeouts.get(), 1);
        let found = packs.read(&digest).await.expect("read");
        assert!(found.is_none());
    }

    /// Asserts that an upload of a blob larger than all the room is refused RESOURCE_EXHAUSTED
    /// at once, without a byte of its body read, and counted, and that it takes nothing.
    #[tokio::test]
    async fn an_upload_larger_than_all_the_room_is_refused_unread() {
        let packs = packs_for(10);
        let (digest, _) = blob_of(11, b'z');
        let (_tx, body) = fed();

        let refused = write(&packs, body, NAME, digest).await;

        let refused = refused.expect_err("refused");
        assert_eq!(refused.code(), Code::ResourceExhausted);
        let metrics = packs.metrics();
        assert_eq!(metrics.upload_refusals.get(), 1);
        assert_eq!(metrics.inflight_bytes(), 0);
        assert!(packs.read(&digest).await.expect("read").is_none());
    }

    /// Asserts that four uploads of 40 bytes into a room of 100 all succeed: the first two take
    /// 80 bytes, the others wait (counted) and are admitted as the earlier ones finish, and the
    /// bytes reserved at once never pass the room.
    #[tokio::test(start_paused = true)]
    async fn uploads_that_together_are_over_the_room_all_succeed() {
        let packs = packs_for(100);
        let blobs: Vec<(Digest, Bytes)> = (0..4).map(|i| blob_of(40, b'a' + i)).collect();
        let mut senders = Vec::new();
        let mut uploads = Vec::new();
        for (digest, _) in &blobs {
            let (tx, body) = fed();
            senders.push(tx);
            uploads.push(upload(&packs, body, *digest));
        }

        settle().await;
        let metrics = packs.metrics();
        assert_eq!(metrics.inflight_bytes(), 80);
        assert_eq!(metrics.upload_waits.get(), 2);
        assert!(uploads.iter().all(|running| !running.is_finished()));

        for (tx, (_, bytes)) in senders.iter().zip(&blobs) {
            send(tx, 0, bytes, true);
        }
        for running in uploads {
            running.await.expect("joined").expect("stored");
        }

        assert_eq!(metrics.inflight_bytes(), 0);
        assert_eq!(metrics.inflight_peak_bytes(), 80);
        for (digest, bytes) in blobs {
            let found = packs.read(&digest).await.expect("read");
            assert_eq!(found, Some(bytes));
        }
    }

    /// Asserts that a receive buffer is given room for the bytes that arrive and not for the
    /// size claimed, grows by doubling (a few copies, not one per message), and is never larger
    /// than the size claimed.
    #[test]
    fn a_receive_buffer_grows_with_the_bytes_and_never_past_the_size() {
        let mut claimed: Vec<u8> = Vec::new();
        grow(&mut claimed, 5, 1000);
        assert!(claimed.capacity() >= 5);
        assert!(claimed.capacity() < 1000);

        let limit = 100;
        let mut buf = Vec::new();
        let mut capacities = Vec::new();
        for _ in 0..limit {
            grow(&mut buf, 1, limit);
            buf.push(7);
            assert!(buf.capacity() <= limit);
            capacities.push(buf.capacity());
        }
        capacities.dedup();
        assert!(capacities.len() <= 10, "{capacities:?}");
    }

    /// Asserts that the idle timeout is the default of 30 seconds unless given, and that one
    /// longer than a day is kept as a day, so that no deadline overflows.
    #[tokio::test]
    async fn the_idle_timeout_is_thirty_seconds_and_at_most_a_day() {
        assert_eq!(packs().upload_idle_timeout(), Duration::from_secs(30));
        let forever = packs_with(PackSettings {
            upload_idle_timeout: Duration::MAX,
            ..PackSettings::default()
        });
        let kept = forever.upload_idle_timeout();
        assert_eq!(kept, Duration::from_secs(86_400));
    }
}
