//! Running the one program the installer needs, behind a trait so a test scripts the answer.
//!
//! A [`Spawn`] is an argument vector and an environment, never a command line: the real
//! [`SystemExec`] hands both to the operating system with the environment cleared first.

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a running child is looked at.
const POLL: Duration = Duration::from_millis(10);

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The whole environment of the child; nothing is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// The whole call: the child is killed when it has run this long.
    pub timeout: Duration,
    /// The most of stdout that is kept; a child that writes more is killed.
    pub stdout_cap: usize,
}

/// A child that ended on its own (or was killed for writing too much).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exited {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    /// Stdout went past [`Spawn::stdout_cap`]; `stdout` holds the first cap-plus-one bytes.
    pub stdout_overflow: bool,
    /// The head of stderr; for classifying only, never shown.
    pub stderr: Vec<u8>,
}

/// Why no answer came back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// The program could not be started.
    Spawn,
    /// The child ran past [`Spawn::timeout`] and was killed.
    Timeout,
}

/// Runs a program to its end.
pub trait Exec: Send + Sync {
    fn run(&self, spawn: &Spawn) -> Result<Exited, ExecError>;
}

/// The real thing: `std::process`, stdin closed, stdout and stderr read on threads.
pub struct SystemExec;

/// The most of stderr that is kept, in bytes.
const STDERR_KEEP: usize = 4096;

impl Exec for SystemExec {
    fn run(&self, spawn: &Spawn) -> Result<Exited, ExecError> {
        let mut child = Command::new(&spawn.program)
            .args(&spawn.args)
            .env_clear()
            .envs(spawn.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| ExecError::Spawn)?;
        let overflow = Arc::new(AtomicBool::new(false));
        let out = child
            .stdout
            .take()
            .map(|p| reader(p, spawn.stdout_cap, Some(overflow.clone())));
        let err = child.stderr.take().map(|p| reader(p, STDERR_KEEP, None));

        let deadline = Instant::now() + spawn.timeout;
        let mut timed_out = false;
        let status = loop {
            // The flag is looked at before the child: a writer that dies of the closed pipe
            // right after raising it is still seen as one that wrote too much.
            if overflow.load(Ordering::SeqCst) {
                break None;
            }
            if let Ok(Some(status)) = child.try_wait() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break None;
            }
            std::thread::sleep(POLL);
        };
        if status.is_none() {
            // Killing the child is the whole of "stop"; the host-side wrapper keeps an
            // install running in its own unit.
            let _ = child.kill();
        }
        let status = status.or_else(|| child.wait().ok());
        let stdout = out.map(join).unwrap_or_default();
        let stderr = err.map(join).unwrap_or_default();
        if timed_out {
            return Err(ExecError::Timeout);
        }
        Ok(Exited {
            code: status.and_then(|s| s.code()),
            stdout_overflow: stdout.len() > spawn.stdout_cap,
            stdout,
            stderr,
        })
    }
}

/// Read `pipe` to its end, keeping the first `keep + 1` bytes (one more than asked, so that
/// "too long" can be told from "exactly full"). With a flag, raise it once the cap is
/// passed so the waiter can kill the child; without one, drain the rest.
fn reader<R: Read + Send + 'static>(
    mut pipe: R,
    keep: usize,
    overflow: Option<Arc<AtomicBool>>,
) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let room = (keep + 1).saturating_sub(kept.len());
            kept.extend_from_slice(chunk.get(..n.min(room)).unwrap_or_default());
            if kept.len() > keep
                && let Some(flag) = &overflow
            {
                flag.store(true, Ordering::SeqCst);
                break;
            }
        }
        kept
    })
}

fn join(handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(program: &str, args: &[&str], timeout_ms: u64, cap: usize) -> Spawn {
        Spawn {
            program: PathBuf::from(program),
            args: args.iter().map(OsString::from).collect(),
            env: vec![(OsString::from("KBF_PROBE"), OsString::from("yes"))],
            timeout: Duration::from_millis(timeout_ms),
            stdout_cap: cap,
        }
    }

    /// Catches an environment that leaks: the child sees exactly the variables the spec
    /// names (the key and agent variables of the caller's own session stay out).
    #[test]
    fn the_child_sees_only_the_named_environment() {
        let done = SystemExec
            .run(&spawn("/usr/bin/env", &[], 10_000, 4096))
            .expect("env runs");
        assert_eq!(done.code, Some(0));
        assert_eq!(String::from_utf8_lossy(&done.stdout), "KBF_PROBE=yes\n");
        assert!(!done.stdout_overflow);
    }

    /// Catches a timeout that does not kill: a child that sleeps for a minute is stopped
    /// when the limit is up, and the call says so. The sleep is given a length no other
    /// process has (its own id as the decimals), so the test can look in `/proc` and see
    /// the child is gone, not only that the call returned. The call is in a thread so
    /// that a run which waits the child out fails the test instead of the suite's patience.
    #[test]
    fn a_slow_child_is_killed_at_the_timeout() {
        let length = format!("60.{}", std::process::id());
        let (tx, rx) = std::sync::mpsc::channel();
        let args = [length.clone()];
        std::thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let _ = tx.send(SystemExec.run(&spawn("/bin/sleep", &args, 300, 4096)));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the call returns without waiting the child out");
        assert_eq!(got, Err(ExecError::Timeout));
        let wanted = format!("sleep\0{length}\0");
        let alive = std::fs::read_dir("/proc")
            .expect("/proc")
            .flatten()
            .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
            .any(|c| String::from_utf8_lossy(&c).ends_with(&wanted));
        assert!(!alive, "the child is still running");
    }

    /// Catches stdout that is not capped: an endless writer is killed once it passes the cap
    /// and the call returns the first bytes with the overflow flag.
    #[test]
    fn an_endless_writer_is_cut_at_the_cap() {
        let done = SystemExec
            .run(&spawn("/usr/bin/yes", &[], 20_000, 100))
            .expect("returns");
        assert!(done.stdout_overflow);
        assert_eq!(done.stdout.len(), 101);
    }

    /// Catches an off-by-one in the cap: output of exactly the cap is not overflow.
    #[test]
    fn output_of_exactly_the_cap_is_not_overflow() {
        let done = SystemExec
            .run(&spawn("/usr/bin/printf", &["abcd"], 10_000, 4))
            .expect("runs");
        assert_eq!(done.stdout, b"abcd");
        assert!(!done.stdout_overflow);
    }

    /// Catches stderr that is not bounded, or a stderr reader that stops draining at its
    /// bound (the child would block on a full pipe): five thousand bytes of complaint come
    /// back as the first 4097 and the child still ends by itself.
    #[test]
    fn stderr_is_kept_to_its_bound_and_drained() {
        let long = format!("/{}", "x".repeat(5000));
        let done = SystemExec
            .run(&spawn("/bin/cat", &[&long], 10_000, 10))
            .expect("runs");
        assert_eq!(done.code, Some(1));
        assert_eq!(done.stderr.len(), STDERR_KEEP + 1);
    }

    /// Catches a missing program reported as something else.
    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let got = SystemExec.run(&spawn("/nonexistent/ssh", &[], 1000, 10));
        assert_eq!(got, Err(ExecError::Spawn));
    }

    /// Catches an exit status lost: the child's own code comes back, and stderr is kept for
    /// the classifier.
    #[test]
    fn the_exit_code_and_stderr_come_back() {
        let done = SystemExec
            .run(&spawn(
                "/usr/bin/env",
                &["/nonexistent/program"],
                10_000,
                10,
            ))
            .expect("runs");
        assert_eq!(done.code, Some(127));
        assert!(done.stdout.is_empty());
        assert!(!done.stderr.is_empty());
    }
}
