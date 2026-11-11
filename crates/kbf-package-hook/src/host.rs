//! What the hook asks of the host: the staged binary's check, the unit's state, stop, start and
//! restart, and the version of the installed package. [`crate::Systemd`] is the real host;
//! [`crate::fake`] keeps one in a directory.

use std::path::Path;

/// What `kbf --mode=<mode> --check` of the staged release said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Exit 0: the release can run here.
    Ready,
    /// Exit 75: not ready yet (the old process still holds the port). Passes: the old process
    /// is stopped next.
    NotReady,
    /// Exit 2: the release refuses this configuration.
    Refused,
    /// Any other end (another code, a signal, a binary that does not start): the host's fault.
    Failed(String),
}

/// What systemd says of a unit (`systemctl is-active`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitState {
    Active,
    Activating,
    Inactive,
    Failed,
    /// Any other word (`deactivating`, `reloading`, an empty answer).
    Other(String),
}

impl UnitState {
    /// The state a line of `systemctl is-active` names.
    pub fn parse(text: &str) -> UnitState {
        match text.trim() {
            "active" => UnitState::Active,
            "activating" => UnitState::Activating,
            "inactive" => UnitState::Inactive,
            "failed" => UnitState::Failed,
            other => UnitState::Other(other.to_owned()),
        }
    }

    /// The word systemd uses for the state.
    pub fn word(&self) -> &str {
        match self {
            UnitState::Active => "active",
            UnitState::Activating => "activating",
            UnitState::Inactive => "inactive",
            UnitState::Failed => "failed",
            UnitState::Other(word) => word,
        }
    }
}

/// A call to the host that did not work: the text is for the step's log.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);

/// The host a step acts on.
pub trait Host {
    /// Runs the staged `kbf` (`kbf`) with `--mode=<mode> --check`.
    fn check(&self, kbf: &Path, mode: &str) -> Result<Check, HostError>;
    /// The state of `unit`.
    fn state(&self, unit: &str) -> Result<UnitState, HostError>;
    /// Stops `unit`, and returns when it has stopped.
    fn stop(&self, unit: &str) -> Result<(), HostError>;
    /// Starts `unit`.
    fn start(&self, unit: &str) -> Result<(), HostError>;
    /// Restarts `unit`: it runs the binary that is installed now.
    fn restart(&self, unit: &str) -> Result<(), HostError>;
    /// The native version of the installed `kbf` ([`crate::native_version`] of what
    /// `kbf --version --json` prints).
    fn installed_version(&self) -> Result<String, HostError>;
}
