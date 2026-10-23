//! The proxy's cold start: no frontend child runs (a reboot, or a first start), so the
//! release the root half calls installed is started, made to take the generation and routed
//! to (FP sections 2, 6 and 8).
//!
//! - **Which release.** `installed_commit` of `status.json`, through [`Source::installed`]; the
//!   binary run is `releases/<sha>/kbf` ([`Source::runnable`]), never `releases/current`.
//!   Nothing else is ever picked: a missing `installed_commit`, a missing or non-executable
//!   binary, or a release without its `VERIFIED` marker is a [`Reason`], not a guess.
//! - **Which slot.** Always [`BOOT_SLOT`]: a proxy that finds no live child has no slot
//!   memory to read, and a reboot has freed both ports.
//! - **How.** The steps of [`Cold`], shared with the restart in the middle of an update
//!   ([`super::recover`]): migrate forward ([`Migrator`]), spawn as a candidate (the child
//!   port writes the pid record before it returns, so a proxy that dies before the wait ends
//!   adopts the process instead of starting a second), wait for `/readyz`, take the
//!   generation (no live holder; the child keeps the epoch).
//! - **Retries.** An attempt that fails is reported with its [`Reason`] and tried again after
//!   a wait that doubles from [`Retry::first_wait`] to [`Retry::max_wait`], at most
//!   [`Retry::attempts`] times in all. A process an attempt left behind is stopped first: SIGKILL
//!   only for one that never answered ready (it cannot have served), SIGTERM for one a takeover
//!   may have reached. After the last attempt the boot gives up with the last reason and the
//!   streams stay held until the proxy is restarted.
//! - **What is reported.** Every state change goes to a [`Report`] with a stable word and no
//!   value taken from the host; the verb writes it to `boot.json` and the log.

use std::time::Duration;

use serde::Serialize;

use super::cold::{Cold, ColdError};
use super::model::{PortError, ProcIdent, Slot};
use super::ports::{Child, Migrator};
use crate::CommitId;

/// How long the boot waits for a process it told to stop to be gone, before the next attempt.
const STOP_WITHIN: Duration = Duration::from_secs(10);

/// The slot a first child is started in.
pub(super) const BOOT_SLOT: Slot = Slot::A;

/// Why an attempt failed. [`Reason::word`] is the stable word the report carries; it names no
/// value from the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `status.json` names no installed release.
    NoInstalledRelease,
    /// `status.json` cannot be read; with `--kbf-binary`, also the kbf the package installed
    /// cannot be adopted (the log line says why).
    StatusUnreadable,
    /// `releases/<sha>/kbf` is not a file.
    BinaryMissing,
    /// `releases/<sha>/kbf` is not executable.
    BinaryNotExecutable,
    /// `releases/<sha>` has no `VERIFIED` marker: the installer did not install it.
    NotVerified,
    /// The forward migration of the release failed.
    MigrationFailed,
    /// The process could not be started (or the slot still holds a process).
    SpawnFailed,
    /// The process exited before it was ready.
    ExitedBeforeReady,
    /// The process was not ready in time.
    NotReady,
    /// The process refused the takeover.
    TakeoverFailed,
    /// The takeover landed and the proxy could not route to the slot.
    ReleaseFailed,
}

impl Reason {
    /// The word, stable across releases.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Reason::NoInstalledRelease => "no-installed-release",
            Reason::StatusUnreadable => "status-unreadable",
            Reason::BinaryMissing => "binary-missing",
            Reason::BinaryNotExecutable => "binary-not-executable",
            Reason::NotVerified => "release-not-verified",
            Reason::MigrationFailed => "migration-failed",
            Reason::SpawnFailed => "spawn-failed",
            Reason::ExitedBeforeReady => "exited-before-ready",
            Reason::NotReady => "not-ready",
            Reason::TakeoverFailed => "takeover-failed",
            Reason::ReleaseFailed => "release-failed",
        }
    }

    /// What an operator does about it, in one line, when it is theirs to fix.
    #[must_use]
    pub const fn fix(self) -> Option<&'static str> {
        match self {
            Reason::NoInstalledRelease => Some(
                "record the release this host runs in the installer state \
                 (docs/design/frontend-proxy.md, hand-placed releases), then restart kbf-proxy",
            ),
            Reason::StatusUnreadable => Some(
                "start the installer service so it writes status.json (with --kbf-binary: check \
                 that the binary runs and prints commit_full, and read the log line), then \
                 restart kbf-proxy",
            ),
            Reason::BinaryMissing | Reason::BinaryNotExecutable => Some(
                "restore releases/<installed commit>/kbf (0755), or roll back with \
                 kbf-installer rollback, then restart kbf-proxy",
            ),
            Reason::NotVerified => Some(
                "install the release with the installer so it carries its VERIFIED marker \
                 (a hand-placed one: docs/design/frontend-proxy.md), then restart kbf-proxy",
            ),
            Reason::MigrationFailed => Some(
                "the log line of the attempt starts with the refusal's word (database_error, \
                 database_busy, installer_too_old, not_additive, schema_unknown, ...); fix the \
                 database or update the kbf-installer package, then restart kbf-proxy",
            ),
            _ => None,
        }
    }

    fn from_cold(error: &ColdError) -> Option<Reason> {
        Some(match error {
            ColdError::Spawn(_) => Reason::SpawnFailed,
            ColdError::Exited(_) => Reason::ExitedBeforeReady,
            ColdError::NotReady(_) => Reason::NotReady,
            ColdError::Takeover(_) => Reason::TakeoverFailed,
            ColdError::Release { .. } => Reason::ReleaseFailed,
            ColdError::Stopped(_) => return None,
        })
    }
}

/// What the boot reads from the host.
pub(super) trait Source {
    /// `installed_commit` of `status.json`: `None` when there is no file or it names none.
    fn installed(&self) -> Result<Option<CommitId>, PortError>;

    /// Whether `release` may be run: the binary is there and executable, and the release
    /// carries its `VERIFIED` marker.
    fn runnable(&self, release: &CommitId) -> Result<(), Reason>;
}

/// How the boot stands, for `boot.json` and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// `starting`, `retrying`, `serving` or `gave-up`.
    pub state: &'static str,
    /// The attempt this is about, from 1.
    pub attempt: u32,
    /// How many attempts there are.
    pub attempts: u32,
    /// Why the attempt failed (`retrying`, `gave-up`).
    pub reason: Option<&'static str>,
    /// What an operator does, when it is theirs to fix.
    pub fix: Option<&'static str>,
    /// How long until the next attempt (`retrying`).
    pub next_try_s: Option<u64>,
}

/// Where [`Progress`] goes.
pub trait Report {
    /// One state change. `detail` is the failing port's own text, for the log only: it may name
    /// a path.
    fn progress(&self, progress: &Progress, detail: Option<&str>);
}

/// The name of the file in the proxy's state directory that holds the last [`Progress`].
pub const BOOT_FILE: &str = "boot.json";

/// [`Report`] on a file and the standard error: `boot.json` in `dir` is the last
/// [`Progress`], replaced whole each time; the log line adds the fix and the port's detail.
pub struct FileReport {
    dir: std::path::PathBuf,
}

impl FileReport {
    /// A report that writes `boot.json` under `dir`.
    #[must_use]
    pub fn new(dir: impl Into<std::path::PathBuf>) -> FileReport {
        FileReport { dir: dir.into() }
    }
}

impl Report for FileReport {
    fn progress(&self, progress: &Progress, detail: Option<&str>) {
        use std::io::Write as _;
        // Every part is a plain value, so the report is the same code whichever parts exist.
        let part = |text: Option<String>| text.unwrap_or_default();
        let line = format!(
            "kbf-installer: proxy: cold start {} (attempt {} of {}){}{}{}{}",
            progress.state,
            progress.attempt,
            progress.attempts,
            part(progress.reason.map(|r| format!(": {r}"))),
            part(progress.next_try_s.map(|n| format!(", next try in {n} s"))),
            part(progress.fix.map(|f| format!("; to fix: {f}"))),
            part(detail.map(|d| format!(" [{d}]"))),
        );
        let temp = self.dir.join(format!("{BOOT_FILE}.tmp"));
        let mut bytes = serde_json::to_vec(progress).expect("plain fields serialize");
        bytes.push(b'\n');
        let written = std::fs::write(&temp, bytes)
            .and_then(|()| std::fs::rename(&temp, self.dir.join(BOOT_FILE)));
        // The second line only when the file was not written.
        let unwritten = written
            .err()
            .map(|e| format!("\nkbf-installer: proxy: {BOOT_FILE} was not written: {e}"));
        // A closed standard error must not stop the boot.
        let _ = writeln!(std::io::stderr(), "{line}{}", part(unwritten));
    }
}

/// How often and how long apart an attempt is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// How many attempts in all (at least 1).
    pub attempts: u32,
    /// The wait after the first failed attempt.
    pub first_wait: Duration,
    /// The wait stops doubling here.
    pub max_wait: Duration,
}

impl Retry {
    /// Ten attempts over a little over three minutes of waiting, on top of each attempt's own
    /// wait for ready.
    #[must_use]
    pub const fn boot() -> Retry {
        Retry {
            attempts: 10,
            first_wait: Duration::from_secs(2),
            max_wait: Duration::from_secs(30),
        }
    }
}

/// How the boot ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Booted {
    /// The release serves in `slot` and the streams are released into it.
    Serving(Slot),
    /// Every attempt failed; the last reason.
    GaveUp(Reason),
    /// The proxy is shutting down; whatever runs is left as it is.
    Stopped,
}

/// Everything a cold start needs.
pub(super) struct ColdBoot<'a> {
    pub cold: Cold<'a>,
    pub source: &'a dyn Source,
    pub migrator: &'a dyn Migrator,
    pub report: &'a dyn Report,
    pub retry: Retry,
    /// Keeps the startup hold in force until the release is routed to; `false` when it is
    /// gone already (the release then still ends it into the slot).
    pub hold: &'a dyn Fn() -> bool,
}

impl ColdBoot<'_> {
    /// Run the attempts. Never returns before the release serves, the attempts are spent or
    /// the proxy stops.
    pub fn run(&self) -> Booted {
        let attempts = self.retry.attempts.max(1);
        // Gone already: the release still ends the hold into its slot, which needs none.
        let _ = (self.hold)();
        let mut wait = self.retry.first_wait;
        let mut last = Reason::NoInstalledRelease;
        for attempt in 1..=attempts {
            self.report.progress(
                &Progress {
                    state: "starting",
                    attempt,
                    attempts,
                    reason: None,
                    fix: None,
                    next_try_s: None,
                },
                None,
            );
            let failed = match self.once() {
                Ok(()) => {
                    self.report.progress(
                        &Progress {
                            state: "serving",
                            attempt,
                            attempts,
                            reason: None,
                            fix: None,
                            next_try_s: None,
                        },
                        None,
                    );
                    return Booted::Serving(BOOT_SLOT);
                }
                Err(Stop::Halted) => return Booted::Stopped,
                Err(Stop::Failed(reason, detail)) => (reason, detail),
            };
            let (reason, detail) = failed;
            last = reason;
            // No branch on the last attempt: its wait is nothing and the loop runs out.
            let more = attempt < attempts;
            self.report.progress(
                &Progress {
                    state: ["gave-up", "retrying"][usize::from(more)],
                    attempt,
                    attempts,
                    reason: Some(reason.word()),
                    fix: reason.fix(),
                    next_try_s: more.then_some(wait.as_secs()),
                },
                detail.as_deref(),
            );
            if self.cold.clock.sleep(wait * u32::from(more)).is_err() {
                return Booted::Stopped;
            }
            wait = (wait * 2).min(self.retry.max_wait);
        }
        Booted::GaveUp(last)
    }

    fn once(&self) -> Result<(), Stop> {
        let release = match self.source.installed() {
            Ok(Some(release)) => release,
            Ok(None) => return Err(Stop::reason(Reason::NoInstalledRelease)),
            Err(e) => return Err(Stop::with(Reason::StatusUnreadable, e)),
        };
        self.source.runnable(&release).map_err(Stop::reason)?;
        self.migrator
            .migrate_forward(&release)
            .map_err(|e| Stop::with(Reason::MigrationFailed, e))?;
        let ident = self
            .cold
            .spawn(BOOT_SLOT, &release)
            .map_err(|e| Stop::cold(&e))?;
        let outcome = self
            .cold
            .await_ready(BOOT_SLOT, &ident)
            .and_then(|()| self.cold.take_over(BOOT_SLOT).map(drop));
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                self.stop_left_over(&ident, &e);
                Err(Stop::cold(&e))
            }
        }
    }

    /// Stop the process an attempt started and did not finish with. Unserved (it never
    /// answered ready) is the only case that may be killed; one a takeover reached gets SIGTERM.
    ///
    /// Returns once the process is gone, or after [`STOP_WITHIN`]: the next attempt needs the
    /// slot, and a process told to stop is in it until it has exited. A process still there
    /// after the limit makes the next spawn refuse, which is that attempt's failure.
    fn stop_left_over(&self, ident: &ProcIdent, error: &ColdError) {
        let child: &dyn Child = self.cold.child;
        let _ = match error {
            ColdError::NotReady(_) => child.kill_unserved(ident),
            ColdError::Takeover(_) | ColdError::Release { .. } => {
                let told = child.terminate(ident);
                if told.is_ok() {
                    child.await_exit(ident, STOP_WITHIN);
                }
                told
            }
            ColdError::Spawn(_) | ColdError::Exited(_) | ColdError::Stopped(_) => return,
        };
    }
}

/// Why `once` did not finish.
enum Stop {
    Failed(Reason, Option<String>),
    Halted,
}

impl Stop {
    fn reason(reason: Reason) -> Stop {
        Stop::Failed(reason, None)
    }

    fn with(reason: Reason, detail: PortError) -> Stop {
        Stop::Failed(reason, Some(detail.0))
    }

    fn cold(error: &ColdError) -> Stop {
        match Reason::from_cold(error) {
            Some(reason) => Stop::Failed(reason, Some(error.to_string())),
            None => Stop::Halted,
        }
    }
}

#[cfg(test)]
#[path = "boot_tests.rs"]
mod tests;
