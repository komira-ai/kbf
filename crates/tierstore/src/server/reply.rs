//! The answers: the plain ones, and S3's error answers with their status codes and codes.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::Full;
use hyper::body::{Body, Frame, SizeHint};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};

use crate::{Error, ReadBody};

use super::xml::error_body;

/// The header that has the hash of the body that the signature covers.
pub(crate) const PAYLOAD_HEADER: &str = "x-amz-content-sha256";

/// What the server sends back.
pub(crate) type Answer = Response<AnswerBody>;

/// The body of an answer: held whole, or the chunks of a read that is given as it goes (the GET
/// of an object a backend reads as a stream), so that the server holds one chunk of it at a time.
pub struct AnswerBody {
    kind: Kind,
}

enum Kind {
    Whole(Full<Bytes>),
    Streamed(Counted),
}

/// A streamed body that is complete only if it gives as many bytes as the answer says it has:
/// fewer, more, or an error of the stream is an error of the body, which ends the connection
/// without a clean end.
///
/// The length is in the answer's headers already, and a connection that has written that many
/// bytes completes the answer without asking the body again, so an error that comes after the
/// last byte would not be heard. The last chunk is therefore held until the stream has ended
/// without an error: a chunk is given only when the stream has given the next one (or its end).
/// The body holds one chunk of the stream's, as it holds none of its own.
struct Counted {
    chunks: ReadBody,
    /// The bytes the stream has still to give.
    owed: u64,
    /// The chunk the stream gave last, not given on yet.
    held: Option<Bytes>,
    /// Whether the body has ended, with an error or without.
    ended: bool,
}

impl Counted {
    fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, Error>>> {
        loop {
            if self.ended {
                return Poll::Ready(None);
            }
            let chunk = match self.chunks.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(chunk) => chunk,
            };
            let failure = match chunk {
                Some(Ok(bytes)) if bytes.is_empty() => continue,
                Some(Ok(bytes)) if bytes.len() as u64 <= self.owed => {
                    self.owed -= bytes.len() as u64;
                    match self.held.replace(bytes) {
                        Some(before) => return Poll::Ready(Some(Ok(before))),
                        None => continue,
                    }
                }
                Some(Ok(_)) => Error::Other("the read gave more bytes than were asked".to_owned()),
                Some(Err(error)) => error,
                None if self.owed == 0 => {
                    self.ended = true;
                    return Poll::Ready(self.held.take().map(Ok));
                }
                None => Error::Other(format!("the read ended {} bytes short", self.owed)),
            };
            self.ended = true;
            self.held = None;
            return Poll::Ready(Some(Err(failure)));
        }
    }

    /// The bytes of the body not given yet.
    fn left(&self) -> u64 {
        self.owed + self.held.as_ref().map_or(0, |held| held.len() as u64)
    }
}

impl AnswerBody {
    /// A body of `len` bytes that `chunks` gives.
    pub(crate) fn streamed(len: u64, chunks: ReadBody) -> Self {
        let kind = Kind::Streamed(Counted {
            chunks,
            owed: len,
            held: None,
            ended: false,
        });
        Self { kind }
    }
}

impl Body for AnswerBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        match &mut self.kind {
            Kind::Whole(whole) => Pin::new(whole)
                .poll_frame(cx)
                .map(|frame| frame.map(|frame| Ok(frame.unwrap_or_else(|never| match never {})))),
            Kind::Streamed(counted) => counted
                .poll_chunk(cx)
                .map(|chunk| chunk.map(|chunk| chunk.map(Frame::data))),
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.kind {
            Kind::Whole(whole) => whole.is_end_stream(),
            // The end is the stream's own: a stream that fails after the last byte must still be
            // heard.
            Kind::Streamed(_) => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.kind {
            Kind::Whole(whole) => whole.size_hint(),
            Kind::Streamed(counted) => SizeHint::with_exact(counted.left()),
        }
    }
}

/// `text` with every character that is not printable ASCII replaced by `?`, which makes it
/// a header value.
pub(crate) fn header_text(text: &str) -> String {
    text.chars()
        .map(|c| if matches!(c, ' '..='~') { c } else { '?' })
        .collect()
}

/// An answer with these headers (names in lower case) and this body.
pub(crate) fn answer(
    status: StatusCode,
    headers: &[(&'static str, String)],
    body: Bytes,
) -> Answer {
    let body = AnswerBody {
        kind: Kind::Whole(Full::new(body)),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in headers {
        let value = HeaderValue::from_str(&header_text(value)).expect("printable ASCII");
        response
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
    response
}

/// An answer with these headers whose body is the `len` bytes that `chunks` gives.
pub(crate) fn streamed_answer(
    status: StatusCode,
    headers: &[(&'static str, String)],
    len: u64,
    chunks: ReadBody,
) -> Answer {
    let mut response = answer(status, headers, Bytes::new());
    *response.body_mut() = AnswerBody::streamed(len, chunks);
    response
}

/// An answer whose body is an XML document.
pub(crate) fn xml_answer(status: StatusCode, body: String) -> Answer {
    let headers = [("content-type", "application/xml".to_owned())];
    answer(status, &headers, Bytes::from(body))
}

/// A failed request: the status and the S3 error code, and the text for the client.
#[derive(Debug)]
pub(crate) struct S3Error {
    status: StatusCode,
    code: &'static str,
    message: String,
    headers: Vec<(&'static str, String)>,
}

impl S3Error {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            headers: Vec::new(),
        }
    }

    /// The same error with one more header (its name in lower case).
    pub(crate) fn with(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    /// A request with an argument that is not valid.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "InvalidArgument", message)
    }

    /// A request for something this server does not do.
    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "NotImplemented", message)
    }

    /// A request after the drain began: refused, and the connection is closed.
    pub(crate) fn draining() -> Self {
        let text = "The server is shutting down; send the request to another endpoint.";
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", text)
            .with("connection", "close")
    }

    /// A request whose body took no byte for the idle timeout: the connection is closed.
    pub(crate) fn request_timeout(idle: Duration) -> Self {
        let text = format!("No byte of the request body arrived within {idle:?}.");
        Self::new(StatusCode::BAD_REQUEST, "RequestTimeout", text).with("connection", "close")
    }

    /// A request body that came in slower, on average, than the least rate allowed: the
    /// connection is closed.
    pub(crate) fn too_slow(min_bytes_per_sec: u64) -> Self {
        let text = format!(
            "The request body came in at an average of less than {min_bytes_per_sec} bytes a second."
        );
        Self::new(StatusCode::BAD_REQUEST, "RequestTimeout", text).with("connection", "close")
    }

    /// A streamed body whose length the request did not give, for a store that needs it.
    pub(crate) fn length_required() -> Self {
        let text = "A Content-Length is needed: this store does not take a body of unknown length.";
        Self::new(StatusCode::LENGTH_REQUIRED, "MissingContentLength", text)
    }

    /// A body that the request says is longer than the limit, before any of it was read.
    pub(crate) fn too_large(limit: usize) -> Self {
        let text = format!("The request body is over the limit of {limit} bytes.");
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge", text)
    }

    /// More streamed bodies than the server takes at once: try again after a moment.
    pub(crate) fn busy() -> Self {
        let text = "The server is taking as many uploads as it can; retry after a moment.";
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "SlowDown", text).with("retry-after", "1")
    }

    /// A write to a server that is a read cache.
    pub(crate) fn read_cache() -> Self {
        let text = "This tierstore is a read cache: it does not accept writes; send them to the \
                    storage endpoint it reads from.";
        Self::new(StatusCode::FORBIDDEN, "AccessDenied", text)
    }

    /// Whether this is the answer to a request that stalled.
    pub(crate) fn is_timeout(&self) -> bool {
        self.code == "RequestTimeout"
    }

    /// A body whose hash is not the one the request claimed in the header `header`.
    pub(crate) fn mismatch(header: &str) -> Self {
        if header == PAYLOAD_HEADER {
            let text = "The x-amz-content-sha256 header is not the SHA-256 of the body.";
            Self::new(StatusCode::BAD_REQUEST, "XAmzContentSHA256Mismatch", text)
        } else {
            let text = format!("The {header} you specified did not match what was received.");
            Self::new(StatusCode::BAD_REQUEST, "BadDigest", text)
        }
    }

    /// A checksum header whose value is not a hash of the right length, in base64.
    pub(crate) fn invalid_digest(header: &str) -> Self {
        let text = format!("The {header} you specified is not valid.");
        Self::new(StatusCode::BAD_REQUEST, "InvalidDigest", text)
    }

    /// A request for a bucket other than the one this server serves.
    pub(crate) fn no_such_bucket(bucket: &str) -> Self {
        let text = format!("The bucket {bucket:?} does not exist on this server.");
        Self::new(StatusCode::NOT_FOUND, "NoSuchBucket", text)
    }

    /// A request that names a multipart upload this server does not hold.
    pub(crate) fn no_such_upload() -> Self {
        let text = "The multipart upload does not exist, or was aborted or completed.";
        Self::new(StatusCode::NOT_FOUND, "NoSuchUpload", text)
    }

    /// A request whose path or query is not valid percent-encoded UTF-8.
    pub(crate) fn invalid_uri() -> Self {
        let text = "The path or the query is not valid percent-encoded UTF-8.";
        Self::new(StatusCode::BAD_REQUEST, "InvalidURI", text)
    }

    /// A request for a range that starts past the end of the object.
    pub(crate) fn invalid_range(size: u64) -> Self {
        let text = "The requested range is not satisfiable.";
        Self::new(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange", text)
            .with("content-range", &format!("bytes */{size}"))
    }

    /// The error as an answer. `resource` is the path of the request.
    pub(crate) fn into_answer(self, resource: &str, request_id: &str) -> Answer {
        let body = error_body(self.code, &self.message, resource, request_id);
        let mut headers = self.headers;
        headers.push(("content-type", "application/xml".to_owned()));
        answer(self.status, &headers, Bytes::from(body))
    }

    /// The status and the code, which the tests of other modules read.
    #[cfg(test)]
    pub(crate) fn kind(&self) -> (StatusCode, &'static str) {
        (self.status, self.code)
    }

    /// The text for the client.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl From<Error> for S3Error {
    fn from(error: Error) -> Self {
        let (status, code) = match &error {
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "NoSuchKey"),
            Error::AlreadyExists(_) | Error::PreconditionFailed(_) => {
                (StatusCode::PRECONDITION_FAILED, "PreconditionFailed")
            }
            Error::Full(_) => (StatusCode::SERVICE_UNAVAILABLE, "SlowDown"),
            Error::Reserved(_) => (StatusCode::FORBIDDEN, "AccessDenied"),
            Error::InvalidPart(_) => (StatusCode::BAD_REQUEST, "InvalidPart"),
            Error::BadDigest(_) => (StatusCode::BAD_REQUEST, "BadDigest"),
            Error::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, "NotImplemented"),
            Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable"),
            Error::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
        };
        let mut refusal = Self::new(status, code, error.to_string());
        if matches!(error, Error::Full(_)) {
            refusal.headers.push(("retry-after", "1".to_owned()));
        }
        refusal
    }
}
