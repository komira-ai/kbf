//! The ports of the roll: everything the state machine asks of the outside world. The real
//! adapters are [`crate::safe_http`] (safe-to-stop), [`crate::ssh_installer`] (install),
//! [`crate::node_drain`] (drain), [`crate::node_health`] (health) and
//! [`crate::frontend_http`] (a frontend member's own API). [`crate::fakes`] has one scripted
//! fake per port.
//!
//! The traits are synchronous: a roll is one host at a time and waits on each answer, so
//! there is nothing to overlap, and a caller runs [`crate::Updater::drive`] on its own thread.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::inventory::Host;
use crate::version::Version;

/// Why a port call failed, in the one distinction the roll acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortErrorKind {
    /// The host or service could not be reached or did not answer (a refused connection, a
    /// timeout, a 5xx): no answer, as opposed to an answer that says no.
    Unreachable,
    /// Anything else: the far side answered and refused, or failed.
    Other,
}

/// A port could not be asked or did not answer: a refused connection, a timeout, a 5xx. One
/// line, never raw ssh or apt output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortError {
    kind: PortErrorKind,
    text: String,
    refused: bool,
}

impl PortError {
    /// No answer: the host could not be reached.
    pub fn unreachable(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Unreachable,
            text: text.into(),
            refused: false,
        }
    }

    /// No answer because nothing listens at the address: the connection was refused. It is an
    /// [`PortErrorKind::Unreachable`] error that [`PortError::is_refused`] tells from a timeout
    /// and from a 5xx, which a process that is still running can give.
    pub fn refused(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Unreachable,
            text: text.into(),
            refused: true,
        }
    }

    /// An answer that is a refusal or a failure.
    pub fn other(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Other,
            text: text.into(),
            refused: false,
        }
    }

    pub fn kind(&self) -> PortErrorKind {
        self.kind
    }

    pub fn is_unreachable(&self) -> bool {
        self.kind == PortErrorKind::Unreachable
    }

    /// Whether the connection was refused: nothing listens there.
    pub fn is_refused(&self) -> bool {
        self.refused
    }
}

/// `PortError("..".into())` builds an [`PortErrorKind::Other`] error, as the tuple struct this
/// type was did.
#[allow(non_snake_case)]
pub fn PortError(text: String) -> PortError {
    PortError::other(text)
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            PortErrorKind::Unreachable => write!(f, "unreachable: {}", self.text),
            PortErrorKind::Other => f.write_str(&self.text),
        }
    }
}

impl std::error::Error for PortError {}

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
    /// The package is not installed: the query's `installed_version` is `null`. A fresh host.
    /// There is nothing to be older than, so the roll installs on it like on any other that
    /// is not at the target.
    NotInstalled,
}

/// What the host's `query` reported: the installed package, and who the host says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub installed: Installed,
    /// The commit of the kbf build on the host (`running_commit` of the query), if it says.
    pub running_commit: Option<String>,
    /// The node id from the host's kbf configuration (`node_id` of the query), if it has one.
    pub node_id: Option<String>,
    /// How the host's last apply stands (`apply` of a Mac's query); none on a Linux host and
    /// on a Mac that has had no apply.
    pub apply: Option<Apply>,
}

/// How a Mac's last apply stands: the `apply` of its `query`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Apply {
    Running,
    Done,
    /// `failed:<reason>`: the reason as the host spells it (lower case letters, `_` and `:`;
    /// the tail says what became of the old release).
    Failed(String),
}

/// What the host said to an install that it accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallAnswer {
    /// The package manager finished: the target is installed (a Linux host).
    Installed,
    /// The target was already installed and running; nothing ran.
    AlreadyInstalled,
    /// The release is checked and its apply is launched, and goes on by itself (a Mac): the
    /// host's `query` says how it ends.
    ApplyStarted,
}

/// The host-side install (the forced-command wrapper over ssh, later).
pub trait Installer: Send + Sync {
    /// Install `version`. A Linux host answers when the package manager is done; a Mac
    /// answers [`InstallAnswer::ApplyStarted`] and finishes on its own. `Err` carries one line
    /// for a person: the wrapper's refusal or the failed install. An older version than the
    /// installed one is installed too: a rollback is an install.
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError>;
    /// What the host reports, without changing anything.
    fn report(&self, host: &Host) -> Result<Report, PortError>;
    /// What is installed, without changing anything.
    fn query(&self, host: &Host) -> Result<Installed, PortError> {
        self.report(host).map(|r| r.installed)
    }
    /// Restart the frontend unit `name` on a Linux host and return when it is active: `-` is
    /// the plain frontend unit, anything else an instance name. A package install restarts
    /// no `kbf` unit, so this is asked for after one. An installer that cannot do it says so,
    /// and `Err` carries one line for a person otherwise.
    fn restart_frontend(&self, _host: &Host, _name: &str) -> Result<(), PortError> {
        Err(PortError::other("unsupported: restart_frontend"))
    }
}

/// A shared installer is an installer: the health check asks the host what it runs through
/// the same one the roll installs with.
impl<T: Installer + ?Sized> Installer for std::sync::Arc<T> {
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError> {
        (**self).install(host, version)
    }

    fn report(&self, host: &Host) -> Result<Report, PortError> {
        (**self).report(host)
    }

    fn restart_frontend(&self, host: &Host, name: &str) -> Result<(), PortError> {
        (**self).restart_frontend(host, name)
    }
}

/// Whether a node was in placement (taking new leases) when a roll came to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// It was serving, or there is no node to ask about: the roll's drain is the only
    /// thing keeping it out, so the roll gives it back.
    In,
    /// It was out already (cordoned, draining or drained by somebody else): the roll still
    /// drains it, so that a restart cuts no lease off, but does not give it back.
    Out,
}

/// How the frontend holds a node out of placement, as far as telling the updater's own cordon
/// from an operator's needs: the updater's cordon is always a drain, never a plain cordon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// In placement.
    Serving,
    /// Out of placement as a plain cordon (no drain): an operator's, never the updater's.
    Cordoned,
    /// Out of placement in a drain (draining, drained or paused at its deadline).
    Draining,
}

/// Taking a daemon host's node out of placement at the frontend, and giving it back.
pub trait Drain: Send + Sync {
    /// Whether the node of `host` is in placement now, or was put out by this caller. A caller
    /// reads this before [`Drain::drain`] and calls [`Drain::undrain`] only for a node that was
    /// [`Placed::In`]. A drain that can remember its own cordon (the agent's) reads a node it
    /// cordoned earlier as [`Placed::In`]; one that cannot reads it as out.
    fn placed(&self, host: &Host) -> Result<Placed, PortError>;
    /// Cordon the node of `host` and ask the frontend to wait for its leases for `deadline`,
    /// or with no deadline (`None`) when the caller waits however long it takes.
    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError>;
    /// How many leases the node of `host` holds now.
    fn leases(&self, host: &Host) -> Result<usize, PortError>;
    /// Return the node of `host` to placement. This is unconditional: it is the caller's job
    /// not to call it for a node that [`Drain::placed`] said was [`Placed::Out`], because an
    /// uncordon cannot tell the roll's cordon from an operator's.
    /// A storage host is never drained, even one that also runs a daemon, and needs no node id.
    fn undrain(&self, host: &Host) -> Result<(), PortError>;
    /// As [`Drain::placed`], but `None` when the frontend knows no node for `host` (a storage
    /// host that runs no daemon): [`Drain::placed`] reads such a host as in placement, and a
    /// caller that records a cordon must tell that from a node that is there. The default
    /// has no such case.
    fn placed_if_known(&self, host: &Host) -> Result<Option<Placed>, PortError> {
        self.placed(host).map(Some)
    }
    /// As [`Drain::placed_if_known`], but telling a plain cordon from a drain. The default
    /// reads a node that is out as draining.
    fn seen_if_known(&self, host: &Host) -> Result<Option<Seen>, PortError> {
        Ok(self.placed_if_known(host)?.map(|placed| match placed {
            Placed::In => Seen::Serving,
            Placed::Out => Seen::Draining,
        }))
    }
    /// Look at the node the caller cordoned earlier, outside any update, so that a drain that
    /// remembers its own cordon can forget one that is gone. The default does nothing.
    fn observe(&self, _host: &Host) -> Result<(), PortError> {
        Ok(())
    }
}

/// What a frontend says of its own drain: `GET /v1/frontend/drain`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct DrainStatus {
    /// A drain has begun. It is not undone: the frontend exits when it has ended.
    pub draining: bool,
    /// Execute and WaitExecution streams open on it now.
    pub open_executions: u64,
    /// The frontend has ended its drain and is closing its streams, so that it exits: its
    /// `phase` is `closing`. A frontend that reports no phase, or another one, is taken to be
    /// draining until its operator API stops answering.
    #[serde(default, rename = "phase", deserialize_with = "phase_is_closing")]
    pub closing: bool,
}

/// Whether a drain `phase` (`serving`, `lb_wait`, `draining` or `closing`) is `closing`.
fn phase_is_closing<'de, D: serde::Deserializer<'de>>(phase: D) -> Result<bool, D::Error> {
    let phase: Option<String> = Option::deserialize(phase)?;
    Ok(phase.as_deref() == Some("closing"))
}

/// A frontend pool member's own operator API, reached on loopback at the member's ports.
pub trait FrontendControl: Send + Sync {
    /// The commit the member's binary was built from.
    fn commit(&self, host: &Host) -> Result<String, PortError>;
    /// Begin the member's own drain (`POST /v1/frontend/drain`): it answers `/readyz` 503,
    /// refuses new work, waits for the executions open on it and exits. Doing it again changes
    /// nothing. A member that is not running answers
    /// with an error for which [`PortError::is_refused`] is true.
    fn drain(&self, host: &Host) -> Result<(), PortError>;
    /// The member's drain as it is now (`GET /v1/frontend/drain`). A member that has exited
    /// answers with an error for which [`PortError::is_refused`] is true.
    fn drain_status(&self, host: &Host) -> Result<DrainStatus, PortError>;
    /// Whether the member's `GET /readyz` answers 200 (`true`) or 503 (`false`).
    fn ready(&self, host: &Host) -> Result<bool, PortError>;
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
    /// Why `host` must not be installed on at all, if the adapter could not judge it after:
    /// asked before the installer is called for the host, and a reason fails the roll there.
    fn refusal(&self, host: &Host) -> Option<String>;

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError>;

    /// [`HealthCheck::check`] for a host whose node the roll holds drained: the node is
    /// not asked to be serving, which it cannot be until the roll gives it back.
    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError>;
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

    /// Catches a shared installer that does not reach the one it holds: install and report
    /// through an `Arc` are the inner installer's, and its default `query` is the report's
    /// installed part.
    #[test]
    fn a_shared_installer_is_the_installer_it_holds() {
        let log = crate::fakes::Log::default();
        let inner = crate::fakes::FakeInstaller::new(&log);
        let shared: std::sync::Arc<dyn Installer> = std::sync::Arc::new(inner.clone());
        let host = Host {
            alias: "a".to_owned(),
            address: "h.example".to_owned(),
            role: crate::inventory::Role::Daemon,
            host_key: "ssh-ed25519 AAAA".to_owned(),
            node_id: None,
            member: None,
        };
        let v = Version::parse("2.0").expect("v");
        shared.install(&host, &v).expect("install");
        assert_eq!(inner.installed("a"), Installed::Version("2.0".into()));
        let report = shared.report(&host).expect("report");
        assert_eq!(report.installed, Installed::Version("2.0".into()));
        assert_eq!(report.node_id.as_deref(), Some("a-node"));
        assert_eq!(shared.query(&host), Ok(Installed::Version("2.0".into())));
    }

    /// An installer that does nothing, and keeps the default of the restart.
    struct Plain;

    impl Installer for Plain {
        fn install(&self, _host: &Host, _version: &Version) -> Result<InstallAnswer, PortError> {
            Err(PortError::other("install"))
        }

        fn report(&self, _host: &Host) -> Result<Report, PortError> {
            Err(PortError::other("report"))
        }
    }

    /// An installer that does only the restart, and answers success to it.
    struct Restarts;

    impl Installer for Restarts {
        fn install(&self, _host: &Host, _version: &Version) -> Result<InstallAnswer, PortError> {
            Err(PortError::other("install"))
        }

        fn report(&self, _host: &Host) -> Result<Report, PortError> {
            Err(PortError::other("report"))
        }

        fn restart_frontend(&self, _host: &Host, _name: &str) -> Result<(), PortError> {
            Ok(())
        }
    }

    /// Asserts that an installer that does not implement `restart_frontend` answers that it is
    /// unsupported, and that a shared installer passes the call on to the one it holds, whether
    /// that one answers the default or its own answer.
    #[test]
    fn restarting_a_frontend_is_unsupported_unless_the_installer_does_it() {
        let shared: std::sync::Arc<dyn Installer> = std::sync::Arc::new(Plain);
        let host = Host {
            alias: "a".to_owned(),
            address: "h.example".to_owned(),
            role: crate::inventory::Role::Daemon,
            host_key: "ssh-ed25519 AAAA".to_owned(),
            node_id: None,
            member: None,
        };
        assert_eq!(
            shared.restart_frontend(&host, "-"),
            Err(PortError::other("unsupported: restart_frontend"))
        );
        let v = Version::parse("2.0").expect("v");
        fn refused<T>(what: &str) -> Result<T, PortError> {
            Err(PortError::other(what))
        }
        assert_eq!(shared.install(&host, &v), refused("install"));
        assert_eq!(shared.report(&host), refused("report"));
        let own: std::sync::Arc<dyn Installer> = std::sync::Arc::new(Restarts);
        assert_eq!(own.restart_frontend(&host, "-"), Ok(()));
        assert_eq!(own.install(&host, &v), refused("install"));
        assert_eq!(own.report(&host), refused("report"));
    }

    /// Catches a kind that is lost or shown wrongly: the tuple-style call is `Other`, the
    /// constructors set their kind, and only `Unreachable` is announced in the text.
    #[test]
    fn a_port_error_has_a_kind() {
        let other = PortError("refused".into());
        assert_eq!(other.kind(), PortErrorKind::Other);
        assert!(!other.is_unreachable());
        assert_eq!(other, PortError::other("refused"));
        let gone = PortError::unreachable("ssh: timed out");
        assert_eq!(gone.kind(), PortErrorKind::Unreachable);
        assert!(gone.is_unreachable());
        assert_eq!(gone.to_string(), "unreachable: ssh: timed out");
        assert_ne!(gone, PortError::other("ssh: timed out"));
    }

    /// Asserts that a refused connection is an unreachable error that says it was refused, that
    /// it reads like any unreachable error, and that the other constructors are not refused: a
    /// timeout, a 5xx and an answer that says no are a process that may be running.
    #[test]
    fn a_refused_connection_is_unreachable_and_is_told_from_a_timeout() {
        let closed = PortError::refused("the operator API: connection refused");
        assert_eq!(closed.kind(), PortErrorKind::Unreachable);
        assert!(closed.is_unreachable());
        assert!(closed.is_refused());
        assert_eq!(
            closed.to_string(),
            "unreachable: the operator API: connection refused"
        );
        assert!(!PortError::unreachable("timed out").is_refused());
        assert!(!PortError::other("no").is_refused());
        assert!(!PortError("no".into()).is_refused());
        assert_ne!(closed, PortError::unreachable(closed.text.clone()));
    }

    /// Asserts that a drain status reads the fields the frontend answers with, ignores the ones
    /// it does not know, and takes a frontend that does not report the `closing` phase to be not closing.
    #[test]
    fn a_drain_status_reads_the_frontends_answer() {
        let plain = r#"{"draining":true,"open_executions":3,"queued_operations":1}"#;
        let status: DrainStatus = serde_json::from_str(plain).expect("status");
        assert_eq!(
            status,
            DrainStatus {
                draining: true,
                open_executions: 3,
                closing: false
            }
        );
        let closing = r#"{"draining":true,"open_executions":0,"phase":"closing"}"#;
        let status: DrainStatus = serde_json::from_str(closing).expect("status");
        assert!(status.closing);
        let waiting = r#"{"draining":true,"open_executions":2,"phase":"draining"}"#;
        let status: DrainStatus = serde_json::from_str(waiting).expect("status");
        assert!(!status.closing);
        assert!(serde_json::from_str::<DrainStatus>("{}").is_err());
    }
}
