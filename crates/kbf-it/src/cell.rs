//! What the integration cells (`kbf-cell m1`, `kbf-cell m2`) share: their error, the
//! processes they start, and the reading of the logs those processes write.

use std::fmt::Display;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process};

/// How long the server and the daemon have to start.
pub(crate) const START_WAIT: Duration = Duration::from_secs(10);

/// How long a `//:pause` build has to start its lease.
pub(crate) const LEASE_WAIT: Duration = Duration::from_secs(60);

/// How often a log is read while waiting for a line.
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// Why a cell test did not pass.
#[derive(Debug, thiserror::Error)]
pub enum CellError {
    /// The cell did not do what the test requires.
    #[error("FAIL: {0}")]
    Failed(String),
    /// The cell could not be set up or driven.
    #[error("{0}")]
    Broken(String),
}

impl CellError {
    /// The exit status `kbf-cell` ends with: 1 for a failed check, 2 otherwise.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Failed(_) => 1,
            Self::Broken(_) => 2,
        }
    }
}

/// A process the cell started. Dropped before it is waited for, it gets SIGINT (as a
/// terminal's Ctrl-C) and is then waited for.
pub(crate) struct Process {
    child: Child,
    reaped: bool,
}

impl Process {
    pub(crate) fn spawn(mut command: Command, what: &str) -> Result<Self, CellError> {
        let child = command.spawn();
        let child = child.or_broken(&format!("start {what}"))?;
        Ok(Self {
            child,
            reaped: false,
        })
    }

    pub(crate) fn wait(&mut self) -> Result<ExitStatus, CellError> {
        let status = self.child.wait();
        let status = status.or_broken(&"wait for a build")?;
        self.reaped = true;
        Ok(status)
    }

    /// Whether the process is still running (once it has ended, it is reaped).
    pub(crate) fn running(&mut self) -> Result<bool, CellError> {
        Ok(self.wait_within(Duration::ZERO)?.is_none())
    }

    /// Waits for the process for at most `limit`: its exit status, or `None` when it
    /// is still running then (it is stopped as it is dropped).
    pub(crate) fn wait_within(&mut self, limit: Duration) -> Result<Option<ExitStatus>, CellError> {
        let deadline = Instant::now() + limit;
        loop {
            let exited = self.child.try_wait();
            let exited = exited.or_broken(&"poll a build")?;
            self.reaped = exited.is_some();
            let late = Instant::now() >= deadline;
            if self.reaped | late {
                return Ok(exited);
            }
            std::thread::sleep(POLL);
        }
    }

    /// The first lease started after the first `before` in `daemon_log`, waited for
    /// while this build runs (and for at most [`LEASE_WAIT`]).
    pub(crate) fn lease(&mut self, daemon_log: &Path, before: usize) -> Result<String, CellError> {
        let deadline = Instant::now() + LEASE_WAIT;
        loop {
            // Its exit first: once the build has ended, the log read next holds every
            // lease the build got, since the daemon logs one before it runs it.
            let exited = self.child.try_wait();
            let exited = exited.or_broken(&"poll a build")?;
            self.reaped = exited.is_some();
            if let Some(lease) = leases_started(&read(daemon_log)?).into_iter().nth(before) {
                return Ok(lease);
            }
            let gave_up = self.reaped | (Instant::now() >= deadline);
            let why = "//:pause started no lease on the daemon".to_owned();
            ensure(!gave_up, why)?;
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped {
            // Errors are left: the process may have exited already.
            let _ = kill_process(Pid::from_child(&self.child), Signal::INT);
            let _ = self.child.wait();
        }
    }
}

/// The ids of the leases `log` (the daemon's, in colour) says started, in order.
pub(crate) fn leases_started(log: &str) -> Vec<String> {
    plain(log)
        .lines()
        .filter_map(|line| line.split_once("lease started lease=").map(|(_, id)| id))
        .map(|id| {
            id.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect()
        })
        .collect()
}

/// `text` without its terminal colour codes (`ESC [ ... m`).
pub(crate) fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            chars.by_ref().take_while(|&c| c != 'm').for_each(drop);
        } else {
            out.push(c);
        }
    }
    out
}

/// Waits up to [`START_WAIT`] for a line of the file at `path` that holds `needle`, and
/// returns it; fails with `failure` if none comes.
pub(crate) fn wait_for_line(path: &Path, needle: &str, failure: &str) -> Result<String, CellError> {
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(line) = read(path)?.lines().find(|l| l.contains(needle)) {
            return Ok(line.to_owned());
        }
        ensure(Instant::now() < deadline, failure.to_owned())?;
        std::thread::sleep(POLL);
    }
}

/// Copies the directory `from` to `to`, recursively.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), CellError> {
    std::fs::create_dir_all(to).or_broken(&to.display())?;
    for entry in std::fs::read_dir(from).or_broken(&from.display())? {
        let entry = entry.or_broken(&from.display())?;
        let (path, target) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().or_broken(&path.display())?.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).or_broken(&path.display())?;
        }
    }
    Ok(())
}

/// Sends `command`'s stdout and stderr to a new file at `log`.
pub(crate) fn to_log(command: &mut Command, log: &Path) -> Result<(), CellError> {
    let out = File::create(log).or_broken(&log.display())?;
    let err = out.try_clone().or_broken(&log.display())?;
    command.stdin(Stdio::null()).stdout(out).stderr(err);
    Ok(())
}

/// Runs `command` with its output in `log`; whether it exited 0.
pub(crate) fn logged(command: &mut Command, log: &Path) -> Result<bool, CellError> {
    to_log(command, log)?;
    status(command)
}

/// Runs `command`; whether it exited 0.
pub(crate) fn status(command: &mut Command) -> Result<bool, CellError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let status = command.status();
    let status = status.or_broken(&format!("run {program}"))?;
    Ok(status.success())
}

/// `n` random bytes as hex digits.
pub(crate) fn random_hex(n: usize) -> Result<String, CellError> {
    let mut bytes = vec![0u8; n];
    let urandom = Path::new("/dev/urandom");
    let filled = File::open(urandom).and_then(|mut f| f.read_exact(&mut bytes));
    filled.or_broken(&urandom.display())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The file at `path`, as text (lossily: logs may hold anything).
pub(crate) fn read(path: &Path) -> Result<String, CellError> {
    let bytes = std::fs::read(path).or_broken(&path.display())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn absolute(path: &Path) -> Result<PathBuf, CellError> {
    std::path::absolute(path).or_broken(&path.display())
}

/// A check: `why` fails the run unless `ok`.
pub(crate) fn ensure(ok: bool, why: String) -> Result<(), CellError> {
    if ok {
        Ok(())
    } else {
        Err(CellError::Failed(why))
    }
}

/// Turns any error into [`CellError::Broken`], after the name of what failed. One
/// mapping for every fallible call keeps the error paths few.
pub(crate) trait OrBroken<T> {
    fn or_broken(self, what: &dyn Display) -> Result<T, CellError>;
}

impl<T, E: Display> OrBroken<T> for Result<T, E> {
    fn or_broken(self, what: &dyn Display) -> Result<T, CellError> {
        self.map_err(|e| CellError::Broken(format!("{what}: {e}")))
    }
}

/// Prints the last `n` lines of the server's and the daemon's error logs in `logs`.
pub(crate) fn print_tails(logs: &Path, n: usize) {
    for name in ["server.err", "daemon.err"] {
        println!("--- last lines of {name}");
        if let Ok(bytes) = std::fs::read(logs.join(name)) {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            for line in &lines[lines.len().saturating_sub(n)..] {
                println!("{line}");
            }
        }
    }
}
