//! The ports of the roll: everything the state machine asks of the outside world. A real
//! adapter is a later change (an HTTP client for safe-to-stop and health, ssh for the
//! installer); [`crate::fakes`] has one scripted fake per port.
//!
//! The traits are synchronous: a roll is one host at a time and waits on each answer, so
//! there is nothing to overlap, and a caller runs [`crate::Updater::drive`] on its own thread.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::inventory::Host;
use crate::version::Version;

/// What kind of failure a [`PortError`] reports. Only the one distinction the roll acts on:
/// a host that could not be reached at all is not a host that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortErrorKind {
    /// The port answered, or failed in a way that says nothing about reachability.
    Other,
    /// The host could not be reached: ssh could not connect, authenticate or verify the host
    /// key. The roll treats it like a query that timed out, not like a failed install.
    Unreachable,
}

/// A port could not be asked or did not answer: a refused connection, a timeout, a 5xx. One
/// line, never raw ssh or apt output.
///
/// Built with [`PortError::other`] or [`PortError::unreachable`]; the function
/// [`PortError`] (`PortError("text".into())`) is [`PortError::other`] under the name the
/// former tuple struct had, so callers written for it keep compiling.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{text}")]
pub struct PortError {
    kind: PortErrorKind,
    text: String,
}

impl PortError {
    /// An error that says nothing about reachability.
    pub fn other(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Other,
            text: text.into(),
        }
    }

    /// The host could not be reached.
    pub fn unreachable(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Unreachable,
            text: text.into(),
        }
    }

    pub fn kind(&self) -> PortErrorKind {
        self.kind
    }

    /// Whether the host could not be reached at all.
    pub fn is_unreachable(&self) -> bool {
        self.kind == PortErrorKind::Unreachable
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// [`PortError::other`], spelled as the tuple struct this type used to be.
#[allow(non_snake_case)]
pub fn PortError(text: String) -> PortError {
    PortError::other(text)
}

/// The answer of `GET /v1/members/{id}/safe-to-stop`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub safe: bool,
    pub reasons: Vec<String>,
    pub planned_down: bool,
}

/// What safe-to-stop said about a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The cluster does not know the host (404).
    Unknown,
    Answer(Verdict),
}

/// Whether a host may be stopped now.
pub trait SafeToStop: Send + Sync {
    fn check(&self, host: &Host) -> Result<Stop, PortError>;
}

/// What the host reports as installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The package version installed (any text the package manager reports).
    Version(String),
    /// An install is running now (the package lock is held).
    InProgress,
}

/// The host-side install (the forced-command wrapper over ssh, later).
pub trait Installer: Send + Sync {
    /// Install `version` and return when the package manager is done. `Err` carries one line
    /// for a person: the wrapper's refusal or the failed install.
    fn install(&self, host: &Host, version: &Version) -> Result<(), PortError>;
    /// What is installed, without changing anything.
    fn query(&self, host: &Host) -> Result<Installed, PortError>;
}

/// Whether a host runs the target yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// The frontend's active child runs the target and the swap is complete; or the node is
    /// Active and its `running_commit` is the target's.
    Healthy,
    /// Not yet; the text says what is missing.
    Waiting(String),
    /// The host will not get there: an abandoned swap. Fails the roll without waiting out the
    /// bound.
    Abandoned(String),
}

/// The health of a host, by its role (frontend or node).
pub trait HealthCheck: Send + Sync {
    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError>;
}

/// Time. A roll waits only through this port, so a test runs hours of timeouts in no time.
pub trait Clock: Send + Sync {
    /// A monotonic reading; only differences mean anything.
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The real clock: monotonic time since it was made, and a thread sleep.
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a system clock that does not move or does not wait: the roll's bounds would
    /// never expire, or would expire at once.
    #[test]
    fn the_system_clock_moves_and_sleeps() {
        let clock = SystemClock::default();
        let before = clock.now();
        clock.sleep(Duration::from_millis(5));
        assert!(clock.now() - before >= Duration::from_millis(5));
    }

    /// Catches a verdict that does not read the #1489 body, which is what the safe-to-stop
    /// adapter will deserialize.
    #[test]
    fn a_verdict_reads_the_safe_to_stop_body() {
        let v: Verdict = serde_json::from_str(
            "{\"safe\":false,\"reasons\":[\"m2 is behind\"],\"planned_down\":true}",
        )
        .expect("body");
        assert_eq!(
            v,
            Verdict {
                safe: false,
                reasons: vec!["m2 is behind".into()],
                planned_down: true
            }
        );
        assert_eq!(PortError("down".into()).to_string(), "down");
    }

    /// Catches an unreachable error that is not told apart from any other (the roll would
    /// treat a dead host as a failed install), a constructor that sets the wrong kind, and a
    /// kind that is lost on clone or ignored by equality.
    #[test]
    fn unreachable_is_a_kind_of_its_own() {
        let down = PortError::unreachable("unreachable: connect");
        assert!(down.is_unreachable());
        assert_eq!(down.kind(), PortErrorKind::Unreachable);
        assert_eq!(down.text(), "unreachable: connect");
        assert_eq!(down.to_string(), "unreachable: connect");
        assert!(down.clone().is_unreachable());
        for other in [PortError::other("x"), PortError("x".into())] {
            assert!(!other.is_unreachable());
            assert_eq!(other.kind(), PortErrorKind::Other);
        }
        assert_eq!(PortError("x".into()), PortError::other("x"));
        assert_ne!(PortError::unreachable("x"), PortError::other("x"));
    }
}
