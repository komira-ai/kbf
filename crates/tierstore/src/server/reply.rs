//! The answers: the plain ones, and S3's error answers with their status codes and codes.

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};

use crate::Error;

use super::uploads::Rejected;
use super::xml::error_body;

/// What the server sends back.
pub(crate) type Answer = Response<Full<Bytes>>;

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
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    for (name, value) in headers {
        let value = HeaderValue::from_str(&header_text(value)).expect("printable ASCII");
        response
            .headers_mut()
            .insert(HeaderName::from_static(*name), value);
    }
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
    #[cfg(test)]
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
            Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable"),
            Error::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<Rejected> for S3Error {
    fn from(rejected: Rejected) -> Self {
        match rejected {
            Rejected::NoSuchUpload => Self::no_such_upload(),
            Rejected::Full => {
                let text = "Too many bytes of unfinished multipart uploads; retry later.";
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "SlowDown", text)
            }
            Rejected::TooLarge => {
                let text = "The parts of the upload are more than this server can hold at once.";
                Self::new(StatusCode::BAD_REQUEST, "EntityTooLarge", text)
            }
        }
    }
}
