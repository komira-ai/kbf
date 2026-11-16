//! What can go wrong, typed by what the caller does next.
//!
//! A call fails in one of these ways:
//!
//! - [`Error::Unreachable`]: nothing was sent. The connection or its TLS session was never
//!   made, or the request is over the size cap. Any call may be repeated.
//! - [`Error::Transport`]: the request may have been sent and the answer was lost: the
//!   connection broke, the call's deadline passed, or the status is one the table below marks
//!   "may have applied" and names no refusal of the node. The node may have acted.
//! - [`Error::Decode`]: the node answered OK with a body this client cannot decode, or one
//!   without the tree digest or placement it answers with. It has acted.
//! - [`Error::Refused`]: the node refused the call by one of the names tierstore gives its
//!   refusals ([`Reason`]), with the code that stands for it. Whether a repeat can help is the
//!   reason's to say (`busy` and `upstream` may pass, `unknown child tree` will not); the
//!   client does not decide it.
//! - [`Error::Status`]: any other status the table marks "definite". The node did not act;
//!   the code says why (`UNIMPLEMENTED` from a node without the interface, for one).
//!
//! [`Error::may_have_applied`] is true for `Transport` and `Decode` only. A create is
//! idempotent by the tree and its locations, and a delete or a read of a placement changes
//! nothing a second time, so after either the caller repeats the same call.
//!
//! # The class of each gRPC code
//!
//! A status from a connection that was never made is `Unreachable`, and one whose message
//! is a refusal of the node, with the code tierstore answers that refusal with, is `Refused`,
//! whatever their codes. Every other status is classed by its code alone:
//!
//! | code | class | when the client sees it |
//! |---|---|---|
//! | `OK` | may have applied | an error with this code is no answer the client can read |
//! | `CANCELLED` | may have applied | the call was cut off on its way |
//! | `UNKNOWN` | may have applied | a stream reset or a connection that closed mid-call |
//! | `INVALID_ARGUMENT` | definite | the request is not one the node reads |
//! | `DEADLINE_EXCEEDED` | may have applied | the call was given up after it may have begun |
//! | `NOT_FOUND` | definite | the node names no such thing |
//! | `ALREADY_EXISTS` | definite | the node refused a conflict |
//! | `PERMISSION_DENIED` | definite | the key may not make the call |
//! | `RESOURCE_EXHAUSTED` | definite | the node is short of something and did not act |
//! | `FAILED_PRECONDITION` | definite | the node lacks what the call needs |
//! | `ABORTED` | may have applied | the node gave up part way |
//! | `OUT_OF_RANGE` | may have applied | the answer is over this client's cap |
//! | `UNIMPLEMENTED` | definite | a node without the trees interface or the call |
//! | `INTERNAL` | may have applied | the node or the HTTP/2 stream failed mid-call |
//! | `UNAVAILABLE` | may have applied | the connection broke after the request may have left |
//! | `DATA_LOSS` | may have applied | the exchange was damaged |
//! | `UNAUTHENTICATED` | definite | the signature was refused |

use std::time::Duration;

use tonic::{Code, Status};

/// The name of a refusal of a tierstore trees interface, as tierstore's `refusal.rs` writes
/// it in the message of the status (`<name>` or `<name>: <what it is about>`), and the code
/// it answers it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// The call is not signed by a key of the node, or the signature is wrong or stale.
    Unauthenticated,
    /// The key is the node's and lacks the right for the call.
    AccessDenied,
    /// The tree's version is not one the node reads.
    UnsupportedVersion,
    /// A name of an entry is empty, `.`, `..`, has a `/` or a NUL, or begins `.wh.`.
    UnsupportedEntryName,
    /// A digest is not 32 bytes.
    MalformedDigest,
    /// A path or a symlink target is too long.
    PathTooLong,
    /// Two entries have one path.
    DuplicatePath,
    /// A file path is also the directory prefix of another entry.
    FileAndDirectory,
    /// A source key is under a reserved prefix.
    ReservedKey,
    /// A source key is under a placement of the node.
    SourceUnderLocation,
    /// A location is not well formed.
    BadLocation,
    /// A mode is not 0 to 07777.
    BadMode,
    /// A span's offset and size do not fit in 64 bits.
    RangeOverflow,
    /// More entries or more file bytes than the node takes in one tree.
    TreeTooLarge,
    /// A path has more names than the node takes.
    TreeTooDeep,
    /// The request is longer than the node takes.
    RequestTooLarge,
    /// The client's own digest of the tree is not the node's.
    DigestMismatch,
    /// A child tree entry names a tree the node does not have.
    UnknownChild,
    /// The location is, is inside, or holds a placement of the node.
    LocationOverlaps,
    /// Another tree is placed at the location.
    LocationTaken,
    /// An ordinary object exists under an S3 location.
    LocationNotEmpty,
    /// The new bytes do not fit under the node's pin limit.
    TooLargeForCache,
    /// The node holds as many placements as it may.
    TooManyPlacements,
    /// The node's disk is full.
    OutOfSpace,
    /// A source object is missing.
    SourceMissing,
    /// A blob's bytes do not hash to its entry's `sha256`.
    Mismatch,
    /// A source object is shorter than the span.
    Short,
    /// The node's upstream failed or timed out.
    Upstream,
    /// The node's fill pool or queue is full.
    Busy,
    /// The node is a read-only instance.
    SlowDown,
    /// The node's create timeout passed.
    CreateTimeout,
    /// No placement has this id.
    UnknownPlacement,
    /// The node has no cache, or its trees are off.
    NotAvailable,
    /// The interface is up and its node has no tree store behind it yet.
    NotServing,
}

impl Reason {
    /// Every reason, in the order of tierstore's table.
    pub const ALL: [Self; 34] = [
        Self::Unauthenticated,
        Self::AccessDenied,
        Self::UnsupportedVersion,
        Self::UnsupportedEntryName,
        Self::MalformedDigest,
        Self::PathTooLong,
        Self::DuplicatePath,
        Self::FileAndDirectory,
        Self::ReservedKey,
        Self::SourceUnderLocation,
        Self::BadLocation,
        Self::BadMode,
        Self::RangeOverflow,
        Self::TreeTooLarge,
        Self::TreeTooDeep,
        Self::RequestTooLarge,
        Self::DigestMismatch,
        Self::UnknownChild,
        Self::LocationOverlaps,
        Self::LocationTaken,
        Self::LocationNotEmpty,
        Self::TooLargeForCache,
        Self::TooManyPlacements,
        Self::OutOfSpace,
        Self::SourceMissing,
        Self::Mismatch,
        Self::Short,
        Self::Upstream,
        Self::Busy,
        Self::SlowDown,
        Self::CreateTimeout,
        Self::UnknownPlacement,
        Self::NotAvailable,
        Self::NotServing,
    ];

    /// The name, as the node writes it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::AccessDenied => "access denied",
            Self::UnsupportedVersion => "unsupported tree version",
            Self::UnsupportedEntryName => "unsupported entry name",
            Self::MalformedDigest => "malformed digest",
            Self::PathTooLong => "path too long",
            Self::DuplicatePath => "duplicate path",
            Self::FileAndDirectory => "file and directory at one path",
            Self::ReservedKey => "reserved key",
            Self::SourceUnderLocation => "source under a tree location",
            Self::BadLocation => "bad location",
            Self::BadMode => "bad mode",
            Self::RangeOverflow => "range overflow",
            Self::TreeTooLarge => "tree too large",
            Self::TreeTooDeep => "tree too deep",
            Self::RequestTooLarge => "request too large",
            Self::DigestMismatch => "tree digest mismatch",
            Self::UnknownChild => "unknown child tree",
            Self::LocationOverlaps => "location overlaps",
            Self::LocationTaken => "location taken",
            Self::LocationNotEmpty => "location not empty",
            Self::TooLargeForCache => "tree too large for the cache",
            Self::TooManyPlacements => "too many placements",
            Self::OutOfSpace => "out of space",
            Self::SourceMissing => "source missing",
            Self::Mismatch => "mismatch",
            Self::Short => "short",
            Self::Upstream => "upstream",
            Self::Busy => "busy",
            Self::SlowDown => "slow down",
            Self::CreateTimeout => "create timeout",
            Self::UnknownPlacement => "unknown placement",
            Self::NotAvailable => "TreesNotAvailable",
            Self::NotServing => "NotServing",
        }
    }

    /// The code the node answers the reason with. `request too large` is also answered
    /// `OUT_OF_RANGE`, by the gRPC layer before the request is read whole (see
    /// [`Reason::answered_by`]).
    #[must_use]
    pub const fn code(self) -> Code {
        match self {
            Self::Unauthenticated => Code::Unauthenticated,
            Self::AccessDenied => Code::PermissionDenied,
            Self::UnsupportedVersion
            | Self::UnsupportedEntryName
            | Self::MalformedDigest
            | Self::PathTooLong
            | Self::DuplicatePath
            | Self::FileAndDirectory
            | Self::ReservedKey
            | Self::SourceUnderLocation
            | Self::BadLocation
            | Self::BadMode
            | Self::RangeOverflow
            | Self::TreeTooLarge
            | Self::TreeTooDeep
            | Self::RequestTooLarge
            | Self::DigestMismatch => Code::InvalidArgument,
            Self::UnknownChild | Self::Mismatch | Self::Short => Code::FailedPrecondition,
            Self::LocationOverlaps | Self::LocationTaken | Self::LocationNotEmpty => {
                Code::AlreadyExists
            }
            Self::TooLargeForCache | Self::TooManyPlacements | Self::OutOfSpace => {
                Code::ResourceExhausted
            }
            Self::SourceMissing | Self::UnknownPlacement => Code::NotFound,
            Self::Upstream | Self::Busy | Self::SlowDown | Self::NotServing => Code::Unavailable,
            Self::CreateTimeout => Code::DeadlineExceeded,
            Self::NotAvailable => Code::Unimplemented,
        }
    }

    /// Whether the node answers the reason with `code`.
    #[must_use]
    pub fn answered_by(self, code: Code) -> bool {
        code == self.code() || (self == Self::RequestTooLarge && code == Code::OutOfRange)
    }
}

/// A refusal of the node: its [`Reason`], what it is about (an entry's path, a key, a
/// location) when the node names one, and the code it came with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}{}", .reason.name(), .about.as_deref().map(|about| format!(": {about}")).unwrap_or_default())]
pub struct Refusal {
    reason: Reason,
    about: Option<String>,
    code: Code,
}

impl Refusal {
    /// The refusal `status` carries: a message that is a reason's name, alone or followed by
    /// `: ` and what it is about, with a code the node answers that reason with. Anything else
    /// is no refusal of the node (a broken connection's `UNAVAILABLE`, an older node's
    /// `UNIMPLEMENTED`).
    pub(crate) fn of(status: &Status) -> Option<Self> {
        let message = status.message();
        Reason::ALL.into_iter().find_map(|reason| {
            let about = match message.strip_prefix(reason.name())? {
                "" => None,
                rest => Some(rest.strip_prefix(": ")?.to_owned()),
            };
            reason.answered_by(status.code()).then(|| Self {
                reason,
                about,
                code: status.code(),
            })
        })
    }

    /// The reason.
    #[must_use]
    pub fn reason(&self) -> Reason {
        self.reason
    }

    /// What the refusal is about: the path, key or location the node named, if any.
    #[must_use]
    pub fn about(&self) -> Option<&str> {
        self.about.as_deref()
    }

    /// The code of the status.
    #[must_use]
    pub fn code(&self) -> Code {
        self.code
    }
}

/// Why nothing was sent.
#[derive(Debug, thiserror::Error)]
pub enum Unsent {
    /// The connection, or its TLS session, could not be made.
    #[error("cannot connect: {}", .0.message())]
    Connect(#[source] Status),
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
    /// The whole answer did not come within the call timeout.
    #[error("no whole answer within {0:?}")]
    Deadline(Duration),
}

/// Why an answer of status OK is not a value.
#[derive(Debug, thiserror::Error)]
pub enum Undecodable {
    /// The bytes are not the message the call answers.
    #[error(transparent)]
    Bytes(#[from] prost::DecodeError),
    /// The answer's tree digest is not 32 bytes.
    #[error("the answer's tree digest is {0} bytes, not 32")]
    Digest(usize),
    /// The answer names no placement.
    #[error("the answer names no placement")]
    NoPlacement,
}

/// A call to a node's trees interface failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Nothing was sent; the call may be repeated.
    #[error("nothing was sent to the trees interface: {0}")]
    Unreachable(#[source] Unsent),
    /// The answer was lost after the request may have been sent; the node may have acted.
    #[error("the call to the trees interface failed after the request may have been sent: {0}")]
    Transport(#[source] Broken),
    /// The node answered OK with a body this client cannot decode; it has acted.
    #[error("the trees interface answered OK with a body this client cannot decode: {0}")]
    Decode(#[source] Undecodable),
    /// The node refused the call, by name.
    #[error("the trees interface refused the call: {0}")]
    Refused(#[source] Refusal),
    /// The node answered a definite status that is no refusal of its own; it did not act.
    #[error("the trees interface answered {code:?}: {message}")]
    Status {
        /// The status's code.
        code: Code,
        /// The status's message.
        message: String,
    },
}

impl Error {
    /// Whether the node may have acted on the request although the caller has no answer that
    /// says so: [`Error::Transport`] and [`Error::Decode`].
    #[must_use]
    pub fn may_have_applied(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Decode(_))
    }

    /// The node's refusal, if it refused the call by name.
    #[must_use]
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }

    /// Whether the node refused the call for `reason`.
    #[must_use]
    pub fn is(&self, reason: Reason) -> bool {
        self.refusal()
            .is_some_and(|refusal| refusal.reason == reason)
    }
}

/// The class of a gRPC code, as the module's table gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// The node may have acted.
    MayHaveApplied,
    /// The node did not act.
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
    if let Some(refusal) = Refusal::of(&status) {
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
    /// The address is not `host:port`.
    #[error("trees address {address:?}: {reason}")]
    Address {
        /// The text as given.
        address: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A node off loopback with no CA would get its calls in clear, and plaintext was not
    /// allowed.
    #[error(
        "trees address {0:?} is off loopback and no CA is pinned; the calls would cross the network in clear"
    )]
    Plaintext(String),
    /// The CA text holds no certificate, or one that cannot be a trust anchor.
    #[error("the CA PEM is unusable: {0}")]
    BadCa(String),
    /// The key cannot sign: an id or a secret that is empty or not printable ASCII, or an id
    /// with a character the `Authorization` header cannot carry.
    #[error("the key's {0}")]
    BadKey(&'static str),
    /// The region is empty or not printable ASCII without `/`.
    #[error("the region {0:?} cannot be in a signature's scope")]
    BadRegion(String),
    /// The TLS library refused its own default settings.
    #[error("TLS setup: {0}")]
    Tls(#[from] rustls::Error),
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
