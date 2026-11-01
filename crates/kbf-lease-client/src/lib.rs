//! A typed client of the job scheduler's lease server, for the host executor.
//!
//! The lease server hands out whole hosts (`POST /v1/leases`), and its interim run API
//! queues container runs on the host a lease holds (`POST /v1/leases/{id}/runs`). This
//! crate speaks those routes and nothing else of the server: acquire, renew and release a
//! lease, list leases (and find one's own by label), list hosts (lost and tainted ones show
//! there), and queue, read, list and stop runs. No farm code calls it yet.
//!
//! # Safety of the credential
//!
//! The bearer token comes from a file only its owner can use ([`Token::from_file`]) and is
//! sent to the one [`Origin`] the client was built for. A redirect is never followed, and
//! `http` is accepted only for loopback. For `https` the client trusts the one CA it is given
//! and no system root, over TLS 1.2 or 1.3 with the `ring` provider; the server asks for no
//! client certificate, so there is none to present. A [`RunSpec`] never prints its
//! arguments, which may carry a presigned URL.
//!
//! # Errors
//!
//! [`Error`] separates what the caller does next. [`Error::Unreachable`] means nothing was
//! sent. [`Error::Transport`] and [`Error::Decode`] mean the server may have acted
//! ([`Error::may_have_applied`]): for a queued run that is the ambiguous case, because
//! `POST .../runs` is not idempotent. A refusal is an [`ApiError`] with the HTTP status and
//! the server's code ([`ErrorCode`]); `409 no-capacity` on a run is a retry later, `410
//! run-forgotten` says the server restarted and lost the run, and `404 unknown-lease` says
//! the lease is gone.
//!
//! # The fake
//!
//! With the `fake` feature, [`fake::FakeLeaseServer`] is a lease server on loopback that
//! keeps the real one's admission rules (one lease per host, `sum(run limits) <= pool shape`
//! for a host, limits that come back only when a run has `ended`, 410 for the runs of an
//! earlier boot) and whose tests move every run through its states by hand. It exists so the
//! code above this crate is tested against the server's contract and not against a mock
//! that agrees with it. `tests/golden/*.json` holds the server's bodies; the client's types,
//! the fake's answers and the server's documented bodies are all tested against them.
#![forbid(unsafe_code)]

mod client;
mod config;
mod error;
#[cfg(feature = "fake")]
pub mod fake;
mod ids;
mod wire;

pub use client::Client;
pub use config::{Config, Origin, Token};
pub use error::{ApiError, ConfigError, Error, ErrorCode};
pub use ids::{BadLeaseId, BadRunId, LeaseId, RunId};
pub use wire::{
    AcquireRequest, Acquired, Granted, Host, HostLeaseClock, HostLeaseMinute, HostLoad, HostSource,
    HostState, Isolation, Lease, LeaseState, Mount, Outcome, PendingLease, ReleaseState, Released,
    Renewed, Resources, Run, RunAccepted, RunFile, RunLimits, RunSpec, RunState, Shape, Taint,
};
