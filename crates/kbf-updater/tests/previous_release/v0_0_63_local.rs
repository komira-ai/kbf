//! The updater's end of `kbf-host-install`'s local socket: one request line in, one JSON line
//! out (`kbf-host-install --serve`, `docs/deploy/host-install.md`).
//!
//! The requests are built from a [`Version`] and a channel name, both checked against the
//! helper's own grammar before they are written, so nothing but a version and a configured
//! channel ever reaches the socket: no path, no URL. The answers are read by the fields the
//! updater uses and the rest is ignored (the helper may add a field). An answer that is not a
//! line of the shapes below is a protocol error, and so is one whose `result` does not fit the
//! request.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::inventory::Host;
use crate::ports::{InstallAnswer, Installed, Installer, PortError, Report};
use crate::version::Version;
use crate::wire::Outcome;

/// The most bytes of an answer that are read.
const ANSWER_MAX: u64 = 16 * 1024;

/// How long each kind of request may take: an install fetches and verifies a release, a revert
/// restarts the units and waits for them, the rest are a file or two.
///
/// A switch has no limit by default. The helper drains the host's frontend before it switches
/// and waits for the drain however long it takes, and it serves one request at a time, so the
/// agent can neither poll it nor cut the wait short without reading a switch that is still
/// going as a helper that is gone. A helper that dies closes the socket, which ends the read
/// at once as [`LocalError::Unreachable`]; only a helper that is alive and silent is waited
/// for, and the switch's own steps (stop, start, readiness) are bounded by the helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub install: Duration,
    /// `None`: wait for the answer as long as the helper is there.
    pub switch: Option<Duration>,
    pub revert: Duration,
    pub other: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            install: Duration::from_secs(20 * 60),
            switch: None,
            revert: Duration::from_secs(10 * 60),
            other: Duration::from_secs(60),
        }
    }
}

/// Why a request to the helper did not work, in the one distinction the update acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalError {
    /// The helper could not be reached or did not answer in time (nothing may have run).
    Unreachable(String),
    /// The helper refused, and nothing ran. The closed code it gave.
    Refused(String),
    /// The helper started the request and it failed. The closed code it gave.
    Failed(String),
    /// What came back is not an answer to the request.
    Protocol(String),
}

impl std::fmt::Display for LocalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the host helper is unreachable: {why}"),
            Self::Refused(code) => write!(f, "the host helper refused: {code}"),
            Self::Failed(code) => write!(f, "the host helper failed: {code}"),
            Self::Protocol(why) => write!(f, "the host helper's answer is not one: {why}"),
        }
    }
}

impl std::error::Error for LocalError {}

/// A release on trial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trial {
    /// The version it was staged as, when the helper knows it.
    pub version: Option<Version>,
    pub commit: String,
    /// When the helper goes back by itself (Unix seconds).
    pub deadline_unix: Option<u64>,
}

/// How the last update the helper ran ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub target: String,
    pub outcome: Outcome,
    pub reason: Option<String>,
}

/// What `query` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostState {
    /// The release that runs (all 40 digits).
    pub commit: Option<String>,
    /// The version text that release was installed as, when the helper knows it.
    pub version: Option<Version>,
    pub node_id: Option<String>,
    pub pending: Option<Trial>,
    pub last: Option<Ended>,
    /// An install, a switch or a restart is running.
    pub busy: bool,
}

/// What an install did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub commit: String,
    /// The host runs this release already; nothing was fetched.
    pub current: bool,
}

/// The helper's side of an update, as the updater uses it.
pub trait Local: Send + Sync {
    fn query(&self) -> Result<HostState, LocalError>;
    /// Fetch, verify and lay out `version` from `channel` beside the running release.
    fn install(&self, version: &Version, channel: &str) -> Result<Staged, LocalError>;
    /// Put the laid-out `version` in place on trial; the deadline (Unix seconds) by which it
    /// must be confirmed.
    fn switch(&self, version: &Version) -> Result<u64, LocalError>;
    fn confirm(&self, version: &Version) -> Result<(), LocalError>;
    /// Go back to the release that ran before the one on trial.
    fn revert(&self) -> Result<(), LocalError>;
}

/// The real helper, over its unix socket.
#[derive(Clone, Debug)]
pub struct SocketLocal {
    socket: PathBuf,
    limits: Limits,
}

#[derive(Deserialize)]
struct Line {
    result: String,
    reason: Option<String>,
    version: Option<String>,
    commit: Option<String>,
    current: Option<bool>,
    deadline_unix: Option<u64>,
    node_id: Option<String>,
    pending: Option<RawTrial>,
    last: Option<RawEnded>,
    busy: Option<bool>,
}

#[derive(Deserialize)]
struct RawTrial {
    version: Option<String>,
    commit: String,
    deadline_unix: Option<u64>,
}

#[derive(Deserialize)]
struct RawEnded {
    target: String,
    outcome: String,
    reason: Option<String>,
}

fn protocol(why: impl Into<String>) -> LocalError {
    LocalError::Protocol(why.into())
}

impl SocketLocal {
    pub fn new(socket: &Path, limits: Limits) -> Self {
        Self {
            socket: socket.to_owned(),
            limits,
        }
    }

    /// One request, one answer line, parsed. `refused` and `failed` lines are the errors of
    /// the same names.
    fn call(
        &self,
        request: &str,
        limit: Option<Duration>,
        expect: &str,
    ) -> Result<Line, LocalError> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|e| LocalError::Unreachable(format!("{}: {e}", self.socket.display())))?;
        let io = |e: std::io::Error| LocalError::Unreachable(e.to_string());
        stream.set_read_timeout(limit).map_err(io)?;
        stream.set_write_timeout(limit).map_err(io)?;
        stream
            .write_all(format!("{request}\n").as_bytes())
            .map_err(io)?;
        let mut text = String::new();
        BufReader::new(stream.take(ANSWER_MAX))
            .read_line(&mut text)
            .map_err(io)?;
        let Some(line) = text.strip_suffix('\n') else {
            return Err(LocalError::Unreachable(
                "the helper closed the socket without an answer".to_owned(),
            ));
        };
        let answer: Line =
            serde_json::from_str(line).map_err(|_| protocol("not a JSON line of the helper"))?;
        let code = answer.reason.clone().unwrap_or_default();
        match answer.result.as_str() {
            "refused" => Err(LocalError::Refused(code)),
            "failed" => Err(LocalError::Failed(code)),
            result if result == expect => Ok(answer),
            other => Err(protocol(format!("{other:?} answers {expect:?}"))),
        }
    }
}

/// `version`, as the helper wrote it: a version text, or none.
fn version_of(text: Option<&str>) -> Result<Option<Version>, LocalError> {
    text.map(|t| Version::parse(t).map_err(|_| protocol("a version that is not one")))
        .transpose()
}

impl Local for SocketLocal {
    fn query(&self) -> Result<HostState, LocalError> {
        let line = self.call("query", Some(self.limits.other), "query")?;
        let pending = line
            .pending
            .map(|p| {
                Ok::<_, LocalError>(Trial {
                    version: version_of(p.version.as_deref())?,
                    commit: p.commit,
                    deadline_unix: p.deadline_unix,
                })
            })
            .transpose()?;
        let last = line
            .last
            .map(|l| {
                let outcome = match l.outcome.as_str() {
                    "succeeded" => Outcome::Succeeded,
                    "failed" => Outcome::Failed,
                    other => return Err(protocol(format!("an update that {other}"))),
                };
                Ok(Ended {
                    target: l.target,
                    outcome,
                    reason: l.reason,
                })
            })
            .transpose()?;
        Ok(HostState {
            version: version_of(line.version.as_deref())?,
            commit: line.commit,
            node_id: line.node_id,
            pending,
            last,
            busy: line.busy.ok_or_else(|| protocol("a query without busy"))?,
        })
    }

    fn install(&self, version: &Version, channel: &str) -> Result<Staged, LocalError> {
        let request = format!("install kbf={} from {channel}", version.as_str());
        let line = self.call(&request, Some(self.limits.install), "staged")?;
        match (line.commit, line.current) {
            (Some(commit), Some(current)) if line.version.as_deref() == Some(version.as_str()) => {
                Ok(Staged { commit, current })
            }
            _ => Err(protocol("a staged release that is not the one asked for")),
        }
    }

    fn switch(&self, version: &Version) -> Result<u64, LocalError> {
        let request = format!("switch kbf={}", version.as_str());
        let line = self.call(&request, self.limits.switch, "switched")?;
        match line.deadline_unix {
            Some(deadline) if line.version.as_deref() == Some(version.as_str()) => Ok(deadline),
            _ => Err(protocol(
                "a switch to a release that is not the one asked for",
            )),
        }
    }

    fn confirm(&self, version: &Version) -> Result<(), LocalError> {
        let request = format!("confirm kbf={}", version.as_str());
        let line = self.call(&request, Some(self.limits.other), "confirmed")?;
        if line.version.as_deref() == Some(version.as_str()) {
            Ok(())
        } else {
            Err(protocol(
                "a confirm of a release that is not the one asked for",
            ))
        }
    }

    fn revert(&self) -> Result<(), LocalError> {
        self.call("revert", Some(self.limits.revert), "reverted")
            .map(drop)
    }
}

/// The helper as the installer port of the health check: what the host runs, from `query`.
/// The health of a daemon host asks `running_commit` of it; nothing else of the port is used.
pub struct LocalInstaller(pub Arc<dyn Local>);

impl Installer for LocalInstaller {
    fn install(&self, _host: &Host, _version: &Version) -> Result<InstallAnswer, PortError> {
        Err(PortError::other(
            "unsupported: the agent installs through its own steps",
        ))
    }

    fn report(&self, _host: &Host) -> Result<Report, PortError> {
        let state = self.0.query().map_err(|e| match e {
            LocalError::Unreachable(_) => PortError::unreachable(e.to_string()),
            _ => PortError::other(e.to_string()),
        })?;
        let installed = if state.busy {
            Installed::InProgress
        } else {
            state.version.map_or(Installed::NotInstalled, |v| {
                Installed::Version(v.to_string())
            })
        };
        Ok(Report {
            installed,
            running_commit: state.commit,
            node_id: state.node_id,
            apply: None,
        })
    }
}
