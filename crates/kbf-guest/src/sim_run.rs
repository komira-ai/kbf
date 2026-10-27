//! Running one command for [`crate::sim`]: no shell, its own process group, its output
//! captured up to a cap, and a deadline after which the whole group is killed.
//!
//! The deadline is judged by an injected [`Clock`], so the kill path runs in tests without a
//! real wait: a manual clock whose `sleep` moves time forward by itself.

use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::sim::{Ended, MAX_CAPTURE_BYTES, Outcome, Streamed, Wanted};

/// How often a running command is looked at.
pub const POLL: Duration = Duration::from_millis(50);

/// Time, for the deadline.
pub trait Clock {
    /// Milliseconds since some fixed moment.
    fn now_ms(&self) -> u64;
    fn sleep(&self, d: Duration);
}

/// The wall clock.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep(&self, d: Duration) {
        thread::sleep(d);
    }
}

/// Runs commands with a deadline judged by `C`.
pub struct ProcessRunner<C> {
    clock: C,
    /// Environment added to the command's (HOME and PATH: a launch daemon has neither).
    env: Vec<(String, String)>,
}

/// What a capture thread has read so far.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    /// More than [`MAX_CAPTURE_BYTES`] came and was dropped.
    cut: bool,
}

/// How long to wait, after the command has ended, for its output pipes to close. A daemon
/// the command started (`simctl boot` starts the simulator service) can keep a pipe open
/// for as long as it lives; what was read by then is kept and the run goes on.
pub const DRAIN: Duration = Duration::from_secs(2);

/// The most reads of a stream handed to its sink before the deadline is looked at.
const BATCH: usize = 512;

/// A thread reading one pipe into a shared buffer.
struct Capture {
    shared: Arc<Mutex<Captured>>,
    done: mpsc::Receiver<()>,
}

impl Capture {
    fn start<R: Read + Send + 'static>(pipe: Option<R>) -> Self {
        let shared = Arc::new(Mutex::new(Captured::default()));
        let (tx, done) = mpsc::channel();
        let into = Arc::clone(&shared);
        thread::spawn(move || {
            // The pipe is there when the child was started with one; a missing one reads as empty.
            pipe.into_iter().for_each(|mut r| {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut c = into.lock().unwrap_or_else(PoisonError::into_inner);
                            let room = MAX_CAPTURE_BYTES - c.bytes.len();
                            c.bytes.extend_from_slice(&buf[..n.min(room)]);
                            c.cut |= n > room;
                        }
                    }
                }
            });
            let _ = tx.send(());
        });
        Self { shared, done }
    }

    /// What was read, once the pipe closed or [`DRAIN`] passed.
    fn finish(&self) -> (Vec<u8>, bool) {
        let _ = self.done.recv_timeout(DRAIN);
        let c = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        (c.bytes.clone(), c.cut)
    }
}

impl<C: Clock> ProcessRunner<C> {
    #[must_use]
    pub fn new(clock: C, env: Vec<(String, String)>) -> Self {
        Self { clock, env }
    }

    /// Runs `argv[0]` with the rest as arguments and waits for it, at most `deadline`
    /// (by this runner's clock). A command still running then is killed with its process
    /// group.
    pub fn run(&self, argv: &[String], deadline: Duration) -> Outcome {
        let began = self.clock.now_ms();
        let elapsed = |clock: &C| clock.now_ms().saturating_sub(began);
        let Some((program, args)) = argv.split_first() else {
            return Outcome {
                ended: Ended::SpawnFailed("an empty command".to_owned()),
                stdout: Vec::new(),
                stderr: Vec::new(),
                cut: false,
                ms: 0,
            };
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so that the deadline can end the command and what it started.
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Outcome {
                    ended: Ended::SpawnFailed(format!("{program}: {e}")),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    cut: false,
                    ms: elapsed(&self.clock),
                };
            }
        };
        let (out, err) = (
            Capture::start(child.stdout.take()),
            Capture::start(child.stderr.take()),
        );
        let deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        let ended = loop {
            // A wait that fails reads as "still running": the deadline then ends the command.
            match child.try_wait().unwrap_or(None) {
                Some(status) => break ended_of(status),
                None if elapsed(&self.clock) >= deadline_ms => {
                    kill_group(&mut child);
                    break Ended::TimedOut;
                }
                None => self.clock.sleep(POLL),
            }
        };
        let ms = elapsed(&self.clock);
        let ((stdout, cut_out), (stderr, cut_err)) = (out.finish(), err.finish());
        Outcome {
            ended,
            stdout,
            stderr,
            cut: cut_out || cut_err,
            ms,
        }
    }

    /// Like [`run`](Self::run), but the command's stdout is handed to `sink` as it is read
    /// and kept nowhere, and its stderr is discarded (never read): for a command whose
    /// output is large, or must not be kept. `sink` runs on the calling thread. A command
    /// still running at `deadline` is killed with its process group.
    pub fn stream(
        &self,
        argv: &[String],
        deadline: Duration,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Streamed {
        let began = self.clock.now_ms();
        let elapsed = |clock: &C| clock.now_ms().saturating_sub(began);
        let failed = |why: String, ms: u64| Streamed {
            ended: Ended::SpawnFailed(why),
            ms,
        };
        let Some((program, args)) = argv.split_first() else {
            return failed("an empty command".to_owned(), 0);
        };
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return failed(format!("{program}: {e}"), elapsed(&self.clock)),
        };
        // A bounded queue: a reader ahead of the sink waits, so memory stays small.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(256);
        let pipe = child.stdout.take();
        thread::spawn(move || {
            pipe.into_iter().for_each(|mut r| {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        // A send fails only once the stream has returned; the loop then
                        // ends at the end of the pipe (the command's group was killed).
                        Ok(n) => {
                            let _ = tx.send(buf[..n].to_vec());
                        }
                    }
                }
            });
        });
        let deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        let ended = loop {
            // At most [`BATCH`] reads per turn, so that a command that writes faster than the
            // sink counts still meets its deadline; a full batch lets the clock move.
            let mut taken = 0;
            while taken < BATCH {
                let Ok(chunk) = rx.try_recv() else { break };
                sink(&chunk);
                taken += 1;
            }
            match child.try_wait().unwrap_or(None) {
                Some(status) => break ended_of(status),
                None if elapsed(&self.clock) >= deadline_ms => {
                    kill_group(&mut child);
                    break Ended::TimedOut;
                }
                None if taken > 0 && taken < BATCH => {}
                None => self.clock.sleep(POLL),
            }
        };
        let ms = elapsed(&self.clock);
        // What is left in the pipe: until it closes, or until [`DRAIN`] has passed in all (a
        // daemon the command started can hold the pipe open).
        let end = Instant::now() + DRAIN;
        while let Ok(chunk) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            sink(&chunk);
        }
        Streamed { ended, ms }
    }
}

/// How a command ended, from its exit status.
#[must_use]
pub fn ended_of(status: std::process::ExitStatus) -> Ended {
    match (status.code(), status.signal()) {
        (Some(code), _) => Ended::Exited(code),
        (None, Some(sig)) => Ended::Signaled(sig),
        // Stopped, not ended; a command is not run under a debugger.
        (None, None) => Ended::Exited(-1),
    }
}

/// Kills the child's process group and reaps the child.
fn kill_group(child: &mut Child) {
    // A process id fits `pid_t`; the largest value names no group, so a failed conversion
    // kills nothing but the child itself below.
    let pid = libc::pid_t::try_from(child.id()).unwrap_or(libc::pid_t::MAX);
    // SAFETY: `killpg` takes a process group id and a signal; the child was started as the
    // leader of its own group, so this reaches it and what it started, nothing else.
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

/// What the file `path` (the guest's `user-wanted`) says: no file is [`Wanted::No`]; a file
/// of any other kind or content than a small regular file whose first line is a valid user
/// name (a link, a directory, a large or unreadable file, a bad name) is
/// [`Wanted::Unreadable`], never [`Wanted::No`].
#[must_use]
pub fn read_wanted(path: &std::path::Path) -> Wanted {
    if std::fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Wanted::No;
    }
    let bytes = match read_regular(path, 256) {
        Ok(b) => b,
        Err(e) => return Wanted::Unreadable(e),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Wanted::Unreadable("not text".to_owned());
    };
    let name = text.lines().next().unwrap_or("").trim();
    match crate::mkuser::check_name(name) {
        Ok(()) => Wanted::User(name.to_owned()),
        Err(e) => Wanted::Unreadable(e),
    }
}

/// Reads the file at `path` when it is a regular file of at most `cap` bytes; a link, a
/// pipe, a device or a larger file is an error (at most `cap` + 1 bytes are read).
///
/// # Errors
/// Why the file was not read.
pub fn read_regular(path: &std::path::Path, cap: usize) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let read = || -> std::io::Result<Option<Vec<u8>>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        // One byte past the cap tells a file over it from one exactly at it.
        file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    };
    match read().map_err(|e| format!("read {}: {e}", path.display()))? {
        None => Err(format!("{} is not a regular file", path.display())),
        Some(bytes) if bytes.len() > cap => {
            Err(format!("{} is over the {cap} byte cap", path.display()))
        }
        Some(bytes) => Ok(bytes),
    }
}
