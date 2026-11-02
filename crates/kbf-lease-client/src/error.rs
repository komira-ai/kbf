//! What can go wrong, typed by what the caller does next.

use std::error::Error as StdError;
use std::fmt;
use std::path::PathBuf;

/// The reason in a server error body that this client reads. The server's codes are
/// kebab-case words (`no-capacity`, `run-forgotten`); a code this version does not know
/// is [`ErrorCode::Other`], and an answer with no readable body is [`ErrorCode::Unspecified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// 409: no free host fits (or the run does not fit the host's budget yet).
    NoCapacity,
    /// 409: nothing in the pool can ever fit the request.
    NoFit,
    /// 409: the class is not served.
    UnsupportedClass,
    /// 409: the lease being started has already ended.
    LeaseEnded,
    /// 409: no host has the capabilities asked for.
    NoCapableHost,
    /// 409: a whole-node lease is not ready.
    ExclusiveNotReady,
    /// 422: the wall time asked for is zero.
    WallTimeNotPositive,
    /// 422: a label key is reserved.
    ReservedLabel,
    /// 422: a label is not valid.
    InvalidLabel,
    /// 422: the class is not one the server has.
    UnknownClass,
    /// 422: the request is below the class's minimum.
    BelowClassMinimum,
    /// 503: the server has issued its last lease id.
    IdsExhausted,
    /// 500: the lease file could not be written; a retry is safe.
    StateWriteFailed,
    /// 400: the body or a value in it is wrong; the reason says where.
    BadRequest,
    /// 404: no such lease (it ended, or the server forgot it).
    UnknownLease,
    /// 404: the lease has no such run.
    UnknownRun,
    /// 404: no such route.
    NotFound,
    /// 410: the run was minted before the server's last restart, which lost it.
    RunForgotten,
    /// 409: the lease is not held (pending, releasing or expired).
    NotHeld,
    /// 409: the lease has expired.
    Expired,
    /// 409: the host stopped answering.
    HostLost,
    /// 409: the host has not said hello since the server restarted.
    HostNotRegistered,
    /// 409: the host failed a check and takes no new run.
    HostTainted,
    /// 409: the host is not ready for runs yet.
    NotReady,
    /// The host's scratch disk quota is out of order and it starts no run. Not a taint: the
    /// host recovers by itself.
    QuotaUnavailable,
    /// 409: a run with this id exists.
    RunExists,
    /// 422: the job breaks a rule of the run contract.
    InvalidJob,
    /// 422: the image reference is not valid.
    InvalidImage,
    /// 422: a mount is not valid.
    InvalidMount,
    /// 422: the limits are above the pool's shape on their own.
    LimitsExceedShape,
    /// 422: a run asked to change the host.
    HostJobsNotAllowed,
    /// 422: the image is not pinned by digest.
    ImageDigestRequired,
    /// 409: the host does not deliver run files.
    FilesUnsupported,
    /// 400: a run file is not valid.
    InvalidFile,
    /// The answer had no `{"error": ...}` body (a 401 has none).
    Unspecified,
    /// A code this version does not know.
    Other(String),
}

/// Every code with its text, in one place: `from_text` and `as_str` read it both ways.
const CODES: &[(&str, ErrorCode)] = &[
    ("no-capacity", ErrorCode::NoCapacity),
    ("no-fit", ErrorCode::NoFit),
    ("unsupported-class", ErrorCode::UnsupportedClass),
    ("lease-ended", ErrorCode::LeaseEnded),
    ("no-capable-host", ErrorCode::NoCapableHost),
    ("exclusive-not-ready", ErrorCode::ExclusiveNotReady),
    ("wall-time-not-positive", ErrorCode::WallTimeNotPositive),
    ("reserved-label", ErrorCode::ReservedLabel),
    ("invalid-label", ErrorCode::InvalidLabel),
    ("unknown-class", ErrorCode::UnknownClass),
    ("below-class-minimum", ErrorCode::BelowClassMinimum),
    ("ids-exhausted", ErrorCode::IdsExhausted),
    ("state-write-failed", ErrorCode::StateWriteFailed),
    ("bad-request", ErrorCode::BadRequest),
    ("unknown-lease", ErrorCode::UnknownLease),
    ("unknown-run", ErrorCode::UnknownRun),
    ("not-found", ErrorCode::NotFound),
    ("run-forgotten", ErrorCode::RunForgotten),
    ("not-held", ErrorCode::NotHeld),
    ("expired", ErrorCode::Expired),
    ("host-lost", ErrorCode::HostLost),
    ("host-not-registered", ErrorCode::HostNotRegistered),
    ("host-tainted", ErrorCode::HostTainted),
    ("not-ready", ErrorCode::NotReady),
    ("quota-unavailable", ErrorCode::QuotaUnavailable),
    ("run-exists", ErrorCode::RunExists),
    ("invalid-job", ErrorCode::InvalidJob),
    ("invalid-image", ErrorCode::InvalidImage),
    ("invalid-mount", ErrorCode::InvalidMount),
    ("limits-exceed-shape", ErrorCode::LimitsExceedShape),
    ("host-jobs-not-allowed", ErrorCode::HostJobsNotAllowed),
    ("image-digest-required", ErrorCode::ImageDigestRequired),
    ("files-unsupported", ErrorCode::FilesUnsupported),
    ("invalid-file", ErrorCode::InvalidFile),
];

impl ErrorCode {
    pub(crate) fn from_text(text: &str) -> Self {
        CODES
            .iter()
            .find(|(known, _)| *known == text)
            .map_or_else(|| Self::Other(text.to_owned()), |(_, code)| code.clone())
    }

    /// The code as the server writes it (`-` for [`ErrorCode::Unspecified`]).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unspecified => "-",
            Self::Other(text) => text,
            known => CODES
                .iter()
                .find(|(_, code)| code == known)
                .map_or("-", |(text, _)| text),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The server answered, and refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the lease server answered {status} {code}{}", reason_suffix(.reason))]
pub struct ApiError {
    /// The HTTP status.
    pub status: u16,
    /// What the body said.
    pub code: ErrorCode,
    /// The 400 body's `reason`, which names the kind of fault and where the parser stopped
    /// and never repeats a value of the request.
    pub reason: Option<String>,
}

fn reason_suffix(reason: &Option<String>) -> String {
    reason
        .as_ref()
        .map_or_else(String::new, |reason| format!(" ({reason})"))
}

/// A call to the lease server failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The exchange failed before the request was sent: no connection, no TLS session. The
    /// server cannot have acted on it, so the caller may repeat it.
    #[error("cannot reach the lease server: {0}")]
    Unreachable(#[source] Box<dyn StdError + Send + Sync>),
    /// The connection broke, or timed out, after the request may have been sent. The server
    /// may have acted: an acquire may have booked a host and a queued run may be running.
    #[error("the exchange with the lease server failed after the request was sent: {0}")]
    Transport(#[source] Box<dyn StdError + Send + Sync>),
    /// The server answered with a success status and a body this client cannot read. It has
    /// acted.
    #[error("the lease server answered {status} with a body this client cannot read")]
    Decode {
        /// The HTTP status.
        status: u16,
        /// Why the body was refused.
        #[source]
        source: serde_json::Error,
    },
    /// The server refused.
    #[error(transparent)]
    Api(#[from] ApiError),
}

impl Error {
    /// The server's refusal, if it refused.
    #[must_use]
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Self::Api(api) => Some(api),
            _ => None,
        }
    }

    /// Whether the server refused with `code`.
    #[must_use]
    pub fn is(&self, code: &ErrorCode) -> bool {
        self.api().is_some_and(|api| api.code == *code)
    }

    /// Whether the server may have acted on the request although the caller has no answer
    /// that says so. For a queued run this is the ambiguous case: it may be running, and
    /// asking again may run it twice.
    #[must_use]
    pub fn may_have_applied(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Decode { .. })
    }
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
    /// The token is not a bearer token that fits a header.
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
