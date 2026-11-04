//! What can go wrong, typed by what the caller does next.
//!
//! A call fails in one of these ways:
//!
//! - [`Error::Unreachable`]: nothing was sent. The connection or its TLS session was never
//!   made, the job id is not one the service takes, or the request is over the size cap.
//!   Any call may be repeated.
//! - [`Error::Transport`]: the request may have been sent and the answer was lost: the
//!   connection broke, the call's deadline passed, or the status is one of the codes the
//!   table below marks "may have applied". The server may have acted.
//! - [`Error::Decode`]: the server answered OK with a body this client cannot decode, or
//!   one without the job or result it answers with. It has acted.
//! - [`Error::Refused`]: the status carries a [`Refusal`] in its details (a refused submit,
//!   whatever the status's code). The server did not act.
//! - [`Error::Status`]: any other status the table marks "definite". The server did not
//!   act; the code says why.
//! - [`Error::TooMany`]: a listing that follows its pages held more jobs than the cap.
//!
//! [`Error::may_have_applied`] is true for `Transport` and `Decode` only. `SubmitJob` is
//! idempotent on the job id and `CancelJob` on the job, so after either of those the caller
//! repeats the call with the same id; the read calls change nothing.
//!
//! # The class of each gRPC code
//!
//! A status from a connection that was never made is `Unreachable`, and one with a refusal
//! in its details is `Refused`, whatever their codes. Every other status is classed by its
//! code alone, by this table (a test reads it and holds the code to it):
//!
//! | code | class | when the client sees it |
//! |---|---|---|
//! | `OK` | may have applied | an error with this code is no answer the client can read |
//! | `CANCELLED` | may have applied | the call was cut off on its way |
//! | `UNKNOWN` | may have applied | a stream reset or a connection that closed mid-call |
//! | `INVALID_ARGUMENT` | definite | the server refused the request as written |
//! | `DEADLINE_EXCEEDED` | may have applied | the server gave up on a call it may have begun |
//! | `NOT_FOUND` | definite | no such job, or its retention window has passed |
//! | `ALREADY_EXISTS` | definite | the id is taken (with its refusal it is `Refused`) |
//! | `PERMISSION_DENIED` | definite | the token may not make the call |
//! | `RESOURCE_EXHAUSTED` | definite | the server is short of something and did not act |
//! | `FAILED_PRECONDITION` | definite | the job is not in the state the call needs |
//! | `ABORTED` | may have applied | the server gave up part way |
//! | `OUT_OF_RANGE` | may have applied | the answer is over this client's cap (4 MiB) |
//! | `UNIMPLEMENTED` | definite | an older server without the call |
//! | `INTERNAL` | may have applied | the server or the HTTP/2 stream failed mid-call |
//! | `UNAVAILABLE` | may have applied | the connection broke after the request may have left |
//! | `DATA_LOSS` | may have applied | the exchange was damaged |
//! | `UNAUTHENTICATED` | definite | the token was refused |

use std::path::PathBuf;
use std::time::Duration;

use js_proto::refusal::refusal_of;
use js_proto::v1::{Refusal, RefusalCode};
use tonic::{Code, Status};

/// A job id the service would not take: it is checked before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadId {
    /// The id is empty or longer than 63 bytes.
    #[error("a job id is 1 to 63 bytes, and this one is {0}")]
    Length(usize),
    /// The id holds a character other than a letter, a digit, `.`, `_` or `-`.
    #[error("a job id holds letters, digits, '.', '_' and '-' only, and this one holds {0:?}")]
    Character(char),
}

/// Why nothing was sent.
#[derive(Debug, thiserror::Error)]
pub enum Unsent {
    /// The connection, or its TLS session, could not be made.
    #[error("cannot connect: {}", .0.message())]
    Connect(#[source] Status),
    /// The job id is not one the service takes.
    #[error(transparent)]
    BadId(#[from] BadId),
    /// The request is over the cap of a message.
    #[error("the request is {len} bytes, over the cap of {cap} bytes")]
    TooLarge {
        /// The encoded size of the request.
        len: usize,
        /// The cap.
        cap: usize,
    },
}

/// Why the answer was lost after the request may have left.
#[derive(Debug, thiserror::Error)]
pub enum Broken {
    /// The call ended with a status this client cannot tell apart from a lost answer.
    #[error("{:?}: {}", .0.code(), .0.message())]
    Status(#[source] Status),
    /// The whole answer did not come within the request timeout.
    #[error("no whole answer within {0:?}")]
    Deadline(Duration),
}

/// Why an answer of status OK is not a value.
#[derive(Debug, thiserror::Error)]
pub enum Undecodable {
    /// The bytes are not the message the call answers.
    #[error(transparent)]
    Bytes(#[from] prost::DecodeError),
    /// The message lacks the field that is the answer (a job, a result).
    #[error("the answer has no {0}")]
    Missing(&'static str),
}

/// A call to the job service failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Nothing was sent; the call may be repeated.
    #[error("nothing was sent to the job service: {0}")]
    Unreachable(#[source] Unsent),
    /// The answer was lost after the request may have been sent; the server may have acted.
    #[error("the call to the job service failed after the request may have been sent: {0}")]
    Transport(#[source] Broken),
    /// The server answered OK with a body this client cannot decode; it has acted.
    #[error("the job service answered OK with a body this client cannot decode: {0}")]
    Decode(#[source] Undecodable),
    /// The server refused the submit, and recorded nothing.
    #[error("the job service refused the job: {} ({})", .0.code().as_str(), .0.detail)]
    Refused(Refusal),
    /// The server answered a definite status that is not a refusal; it did not act.
    #[error("the job service answered {code:?}: {message}")]
    Status {
        /// The status's code.
        code: Code,
        /// The status's message.
        message: String,
    },
    /// A listing that follows its pages holds more jobs than the cap
    /// ([`Client::MAX_LISTED`](crate::Client::MAX_LISTED)).
    #[error("the listing holds more than {0} jobs")]
    TooMany(usize),
}

impl Error {
    /// Whether the server may have acted on the request although the caller has no answer
    /// that says so: [`Error::Transport`] and [`Error::Decode`].
    #[must_use]
    pub fn may_have_applied(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Decode(_))
    }

    /// The server's refusal, if it refused the submit.
    #[must_use]
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }

    /// Whether the server refused the submit with `code`.
    #[must_use]
    pub fn is(&self, code: RefusalCode) -> bool {
        self.refusal().is_some_and(|refusal| refusal.code() == code)
    }
}

/// The class of a gRPC code, as the module's table gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// The server may have acted.
    MayHaveApplied,
    /// The server did not act.
    Definite,
}

/// The class of `code` (the module's table).
pub(crate) fn class_of(code: Code) -> Class {
    match code {
        Code::InvalidArgument
        | Code::NotFound
        | Code::AlreadyExists
        | Code::PermissionDenied
        | Code::ResourceExhausted
        | Code::FailedPrecondition
        | Code::Unimplemented
        | Code::Unauthenticated => Class::Definite,
        Code::Ok
        | Code::Cancelled
        | Code::Unknown
        | Code::DeadlineExceeded
        | Code::Aborted
        | Code::OutOfRange
        | Code::Internal
        | Code::Unavailable
        | Code::DataLoss => Class::MayHaveApplied,
    }
}

/// The error a call that ended with `status` is.
pub(crate) fn from_status(status: Status) -> Error {
    if never_connected(&status) {
        return Error::Unreachable(Unsent::Connect(status));
    }
    if let Some(refusal) = refusal_of(&status) {
        return Error::Refused(refusal);
    }
    match class_of(status.code()) {
        Class::MayHaveApplied => Error::Transport(Broken::Status(status)),
        Class::Definite => Error::Status {
            code: status.code(),
            message: status.message().to_owned(),
        },
    }
}

/// Whether `status` comes from a connection that was never made: tonic marks the failure of
/// its connector, and so of the TCP connect and the TLS handshake, as a
/// [`tonic::ConnectError`] in the status's sources.
fn never_connected(status: &Status) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(status);
    while let Some(error) = cause {
        if error.is::<tonic::ConnectError>() {
            return true;
        }
        cause = error.source();
    }
    false
}

/// A [`Config`](crate::Config) cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The origin is not `http(s)://host[:port]`.
    #[error("origin {origin:?}: {reason}")]
    Origin {
        /// The text as given.
        origin: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// An `http` origin off loopback would send the token in clear.
    #[error("origin {0:?} is plain http off loopback; the token would cross the network in clear")]
    PlainHttp(String),
    /// An `https` origin needs the pinned CA.
    #[error("an https origin needs the pinned CA certificate (PEM)")]
    MissingCa,
    /// A CA was given for an origin that does not use TLS.
    #[error("a CA certificate was given for an origin that does not use TLS")]
    UnusedCa,
    /// The CA text holds no certificate, or one that cannot be a trust anchor.
    #[error("the CA PEM is unusable: {0}")]
    BadCa(String),
    /// The token is not a bearer token that fits the metadata.
    #[error("the token is {0}")]
    BadToken(&'static str),
    /// The TLS library refused its own default settings.
    #[error("TLS setup: {0}")]
    Tls(#[from] rustls::Error),
    /// The token file cannot be read.
    #[error("token file {path}: {source}")]
    TokenFile {
        /// The file.
        path: PathBuf,
        /// Why it cannot be read.
        #[source]
        source: std::io::Error,
    },
    /// The token file is not a regular file only its owner can use.
    #[error("token file {path}: {problem}")]
    TokenFileMode {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        problem: &'static str,
    },
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
