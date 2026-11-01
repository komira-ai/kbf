//! What the integration cells (`kbf-cell m1`, `kbf-cell m2` and `kbf-cell
//! cross-version`) share: their error, the processes they start, and the reading of the
//! logs those processes write.

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

/// How long a process has to end after the SIGINT it gets as it is dropped, before it
/// is killed.
pub(crate) const STOP_WAIT: Duration = Duration::from_secs(30);

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
/// terminal's Ctrl-C) and is then waited for, for at most [`STOP_WAIT`]: a process that
/// has not ended by then is killed, so one that ignores SIGINT cannot hold the run.
pub(crate) struct Process {
    child: Child,
    reaped: bool,
    stop_wait: Duration,
}

impl Process {
    pub(crate) fn spawn(mut command: Command, what: &str) -> Result<Self, CellError> {
        let child = command.spawn();
        let child = child.or_broken(&format!("start {what}"))?;
        Ok(Self {
            child,
            reaped: false,
            stop_wait: STOP_WAIT,
        })
    }

    /// This process, with `limit` in place of [`STOP_WAIT`] (so a test need not wait out
    /// the real one).
    #[cfg(test)]
    fn stop_within(mut self, limit: Duration) -> Self {
        self.stop_wait = limit;
        self
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
            let ended = matches!(self.wait_within(self.stop_wait), Ok(Some(_)));
            if !ended {
                eprintln!(
                    "process {} did not end within {:?} of SIGINT: killed",
                    self.child.id(),
                    self.stop_wait
                );
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
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

/// Calls `look` every `every` until it says it is done (its second value) or `limit` has
/// passed, and returns what the last call saw. Every call is made at least once, so a
/// `limit` of zero is one look. A call that fails ends the wait with its error.
pub(crate) fn poll_until<T>(
    limit: Duration,
    every: Duration,
    mut look: impl FnMut() -> Result<(T, bool), CellError>,
) -> Result<T, CellError> {
    let deadline = Instant::now() + limit;
    loop {
        let (seen, done) = look()?;
        if done | (Instant::now() >= deadline) {
            return Ok(seen);
        }
        std::thread::sleep(every);
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
        print_tail(logs, name, n);
    }
}

/// Prints the last `n` lines of the log `name` in `logs` under a heading of its own.
pub(crate) fn print_tail(logs: &Path, name: &str, n: usize) {
    println!("--- last lines of {name}");
    if let Ok(bytes) = std::fs::read(logs.join(name)) {
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(n)..] {
            println!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rustix::process::test_kill_process;

    use super::*;

    /// Held while this process's SIGINT disposition is changed, so two tests do not
    /// hand each other's.
    static DISPOSITION: Mutex<()> = Mutex::new(());

    /// `sleep 20`, started with SIGINT ignored if `ignore` (an ignored disposition
    /// survives the exec) and with the default action otherwise. This process has the
    /// disposition it had before again as the child is running. (A `pre_exec` closure
    /// would set it in the child alone, but the coverage of a closure that runs after
    /// the fork is lost with the child.)
    fn sleeper(ignore: bool) -> Process {
        let _held = DISPOSITION.lock().expect("no test panics holding it");
        let set = if ignore { libc::SIG_IGN } else { libc::SIG_DFL };
        // SAFETY: `signal` with `SIG_IGN` or `SIG_DFL` installs no handler of ours.
        let before = unsafe { libc::signal(libc::SIGINT, set) };
        let mut command = Command::new("sleep");
        command.arg("20");
        let started = Process::spawn(command, "sleep");
        // SAFETY: `before` is what `signal` returned for this signal, a disposition.
        unsafe { libc::signal(libc::SIGINT, before) };
        started.expect("sleep starts")
    }

    /// Catches a drop that sends no SIGINT and waits out the stop wait, or one that
    /// kills at once instead of asking first: the process here ends on SIGINT, so the
    /// drop must return long before the (long) stop wait.
    #[test]
    fn a_process_that_ends_on_sigint_is_not_waited_for_longer() {
        let process = sleeper(false).stop_within(Duration::from_secs(10));
        let started = Instant::now();
        drop(process);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "the drop took {took:?}");
    }

    /// Catches a drop that waits for a process without end (this one ignores SIGINT and
    /// would run for 20 s), one that kills before the stop wait has passed, and one that
    /// leaves the process running.
    #[test]
    fn a_process_that_ignores_sigint_is_killed_after_the_stop_wait() {
        let process = sleeper(true).stop_within(Duration::from_millis(300));
        let pid = Pid::from_child(&process.child);
        let started = Instant::now();
        drop(process);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(300), "the drop took {took:?}");
        assert!(took < Duration::from_secs(10), "the drop took {took:?}");
        assert!(test_kill_process(pid).is_err(), "the process still runs");
    }

    /// The arm of the drop for a process that was waited for: it sends no signal (the
    /// pid may be another process's by then) and returns at once. This test sees only a
    /// drop that blocks; it cannot see the signal.
    #[test]
    fn a_process_that_was_waited_for_is_dropped_at_once() {
        let mut done = Process::spawn(Command::new("true"), "true").expect("true starts");
        let status = done.wait().expect("true ends");
        assert!(status.success(), "{status}");
        let started = Instant::now();
        drop(done);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "the drop took {took:?}");
    }

    /// Catches: a wait that returns before `look` says it is done while time remains
    /// (it would take the first look for the answer), looks again after it is done, or
    /// returns something other than what the last look saw.
    #[test]
    fn polling_stops_at_the_first_look_that_is_done() {
        let mut looks = 0;
        let seen = poll_until(Duration::from_secs(600), Duration::ZERO, || {
            looks += 1;
            Ok((looks, looks == 3))
        });
        assert_eq!(seen.expect("done"), 3);
        assert_eq!(looks, 3);
    }

    /// Catches: a wait that never ends when `look` never says it is done (the cell would
    /// hang rather than fail), that gives up before one look, or that returns the first
    /// look instead of the last: a limit of zero is exactly one look, and its answer
    /// comes back though it was not done; a limit of 30 ms is not up before 30 ms.
    #[test]
    fn polling_gives_up_at_the_deadline_with_what_the_last_look_saw() {
        let mut looks = 0;
        let seen = poll_until(Duration::ZERO, Duration::ZERO, || {
            looks += 1;
            Ok(("still waiting", false))
        });
        assert_eq!(seen.expect("late"), "still waiting");
        assert_eq!(looks, 1);

        let started = Instant::now();
        let mut looks = 0;
        let seen = poll_until(Duration::from_millis(30), Duration::from_millis(1), || {
            looks += 1;
            Ok((looks, false))
        });
        assert!(
            started.elapsed() >= Duration::from_millis(30),
            "gave up early"
        );
        assert_eq!(seen.expect("late"), looks, "not the last look's answer");
    }

    /// Catches: a failed look swallowed (the wait goes on, or ends with a stale answer)
    /// instead of ending the wait with that error at once.
    #[test]
    fn polling_ends_with_the_error_of_a_look_that_fails() {
        let mut looks = 0;
        let seen = poll_until(Duration::from_secs(600), Duration::ZERO, || {
            looks += 1;
            if looks == 2 {
                Err(CellError::Broken("the log is gone".to_owned()))
            } else {
                Ok((looks, false))
            }
        });
        let error = seen.expect_err("the failed look ends the wait");
        assert_eq!(error.to_string(), "the log is gone");
        assert_eq!(error.exit_code(), 2);
        assert_eq!(looks, 2);
    }
}
