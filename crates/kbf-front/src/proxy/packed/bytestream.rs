//! ByteStream calls over the packs, for the raw path of the proxy, which never decodes a
//! message: a `Read` it has collected whole and a `Write` it hands over as a body. The gRPC
//! messages are cut and made here by hand, a message being a flag byte, a four-byte length
//! and the protobuf.
//!
//! A `Read` is served from the blob as the packs hold it, hashed, cut to
//! `[read_offset, read_offset + read_limit)` and sent in messages of at most
//! [`READ_CHUNK_BYTES`](crate::READ_CHUNK_BYTES), each made as it is polled. A `Write` is
//! received whole, message by message, under the rules of the cache's ByteStream (the same
//! resource, the next offset, no more than the digest's size, `finish_write` to end), and
//! packed.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body as _, Frame};
use kbf_proto::google::bytestream::{ReadRequest, ReadResponse, WriteRequest, WriteResponse};
use kbf_storage::in_flight;
use kbf_types::Digest;
use prost::Message as _;
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
/// `digest` named `name`: received whole, packed, and answered with the blob's size.
///
/// # Errors
/// INVALID_ARGUMENT for a stream that breaks the rules of an upload or whose bytes are not the
/// digest; UNAVAILABLE when the packs could not be written; or the client's own error if its
/// request broke.
pub(in crate::proxy) async fn write(
    packs: &Packs,
    body: Body,
    name: &str,
    digest: Digest,
) -> Result<http::Response<Body>, Status> {
    let received = receive(body, name, &digest).await?;
    packs.put(vec![(digest, received)]).await?;
    let committed = WriteResponse {
        committed_size: i64::try_from(digest.size_bytes).unwrap_or(i64::MAX),
    };
    let frames = VecDeque::from([
        Frame::data(framed(&committed.encode_to_vec())),
        Frame::trailers(trailers_of(&Status::ok(""))),
    ]);
    Ok(grpc(Body::new(Framed(frames))))
}

/// The bytes of the blob `digest` that the messages of `body` carry, counted as held from the
/// allocation (`kbf_bytes_in_flight`) until the packs are done with them.
async fn receive(mut body: Body, name: &str, digest: &Digest) -> Result<Bytes, Status> {
    let size = digest.size_bytes;
    // The size is within the bound the caller checked (`applies`): allocated at once.
    let capacity = usize::try_from(size).unwrap_or_default();
    let held = in_flight::hold(capacity as u64);
    let mut received: Vec<u8> = Vec::with_capacity(capacity);
    let mut pending = Messages::default();
    loop {
        while let Some(message) = pending.take_message()? {
            check(&message, name, received.len() as u64, size)?;
            received.extend_from_slice(&message.data);
            if message.finish_write {
                return Ok(in_flight::counted_with(received, held));
            }
        }
        let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
        let frame = frame
            .ok_or_else(|| Status::invalid_argument("write stream ended without finish_write"))??;
        pending.push(frame.data_ref().map_or(&[][..], Bytes::as_ref));
    }
}

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

    /// The next whole `WriteRequest`, if the bytes so far hold one.
    ///
    /// # Errors
    /// INVALID_ARGUMENT for a compressed message or one that is not a `WriteRequest`;
    /// RESOURCE_EXHAUSTED for a message over the largest a call may carry.
    fn take_message(&mut self) -> Result<Option<WriteRequest>, Status> {
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
            let taken = messages.take_message().expect("well formed");
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
        let refused = compressed.take_message().expect_err("compressed");
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert!(refused.message().contains("compress"), "{refused}");

        let mut huge = Messages::default();
        huge.push(&[0, 0xff, 0xff, 0xff, 0xff]);
        let refused = huge.take_message().expect_err("over the bound");
        assert_eq!(refused.code(), Code::ResourceExhausted);

        let mut garbage = Messages::default();
        garbage.push(&wire(0, &[0xff, 0xff]));
        let refused = garbage.take_message().expect_err("not protobuf");
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert!(refused.message().contains("WriteRequest"), "{refused}");
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
        let create_only = kbf_objstore::Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        let store = std::sync::Arc::new(kbf_objstore::MemoryStore::new(create_only));
        let clock = std::sync::Arc::new(kbf_state::ManualClock::default());
        let index = kbf_state::pack_index::memory::MemoryPackIndex::new(clock);
        let index: std::sync::Arc<dyn kbf_state::PackIndex> = std::sync::Arc::new(index);
        let keys = kbf_pack::PackKeys::new(kbf_pack::WriterKind::Frontend, "fe1", "boot1");
        let keys = keys.expect("keys");
        let made = Packs::new(store, index, keys, crate::PackSettings::default());
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
}
