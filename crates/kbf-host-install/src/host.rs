//! One invocation: parse the request, check it against the allowlist, run the plan
//! through a [`Runner`], answer, and audit.
//!
//! Order matters. A request outside the grammar is refused before any file is read. An
//! `install` is checked against the allowlist and the source's signing before the audit
//! log is opened, and is not started at all if the log cannot be opened: no install goes
//! unrecorded. A refusal or a `query` is answered even when the log is unavailable (the
//! warning goes to stderr), since neither changes the host.

use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;

use crate::runner::Runner;
use crate::trust::Trust;

/// Where the files this program reads as root are, and who must own them.
#[derive(Debug, Clone)]
pub struct Host {
    pub(crate) config: PathBuf,
    pub(crate) audit: PathBuf,
    pub(crate) proc_locks: PathBuf,
    pub(crate) dpkg_lock: PathBuf,
    pub(crate) trust: Trust,
}

impl Host {
    /// The production paths, trusting root. The only constructor outside tests.
    pub fn production() -> Self {
        todo!()
    }

    /// Paths under `dir`, trusting `trust`, for tests.
    #[cfg(test)]
    pub fn for_test(dir: &std::path::Path, trust: Trust) -> Self {
        Self {
            config: dir.join("host-install.toml"),
            audit: dir.join("audit.log"),
            proc_locks: dir.join("proc-locks"),
            dpkg_lock: dir.join("lock-frontend"),
            trust,
        }
    }
}

/// Handle `command` (the `SSH_ORIGINAL_COMMAND`, none when unset): write the answer line
/// to `out`, detail to `err`, and return the exit status.
pub fn handle(
    host: &Host,
    runner: &dyn Runner,
    command: Option<&OsStr>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let _ = (host, runner, command, out, err);
    todo!()
}

#[cfg(test)]
mod tests;
