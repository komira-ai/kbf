//! Running a program: an `execve` with an explicit argument vector and a cleared
//! environment, behind a trait so tests record the vectors instead of running them.
//!
//! No shell is involved anywhere: a [`Cmd`] is a program path and its arguments, and the
//! only environment a child sees is [`PATH`], [`LANG`] and [`DEBIAN_FRONTEND`].

use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::ids;

/// The only `PATH` a child sees.
pub const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The only locale a child sees.
pub const LANG: &str = "C";

/// The only front end `debconf` and `dpkg` see: they never ask a question.
pub const DEBIAN_FRONTEND: &str = "noninteractive";

/// The user and group a command runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

/// A limit on what a command may write: the files directly in `dir` hold at most `max` bytes
/// together, or the command is killed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteCap {
    pub dir: PathBuf,
    pub max: u64,
}

impl WriteCap {
    /// Whether the files directly in the directory hold more than the limit. A directory that
    /// cannot be read holds nothing.
    fn exceeded(&self) -> bool {
        let held = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .fold(0_u64, |total, meta| total.saturating_add(meta.len()));
        held > self.max
    }
}

/// A program (an absolute path) and its arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// How long it may run before it is killed; none to wait for it however long it takes.
    pub timeout: Option<Duration>,
    /// Variables the child sees besides the fixed three.
    pub env: Vec<(String, String)>,
    /// The user and group the child runs as, with no supplementary group; none for the
    /// caller's own. A root caller is needed (see [`drop_ids`]). On macOS the command starts in
    /// `/`, because the user may not be able to enter the caller's directory.
    pub run_as: Option<Identity>,
    /// Bytes written to the child's standard input, which is then closed; none for an
    /// input that is closed at once.
    pub stdin: Vec<u8>,
    /// What the command may write, watched while it runs; none for no limit.
    pub write_cap: Option<WriteCap>,
    /// How much of its standard output is kept: past this, one byte more is kept (so the
    /// caller can see the output was longer) and the rest is read and dropped; none to keep
    /// it all.
    pub stdout_max: Option<usize>,
}

/// How a child ended and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// The exit status, or none when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it, when one did (then `code` is none).
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Exec {
    /// How it ended, in words: `exit status 3`, `signal 13 (SIGPIPE)`, or, for a status that is
    /// neither (not seen on Unix), `no exit status`.
    pub fn ended(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exit status {code}"),
            (None, Some(signal)) => format!("signal {signal} ({})", signal_name(signal)),
            (None, None) => "no exit status".to_owned(),
        }
    }

    /// Whether it exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Stdout as text, invalid bytes replaced.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as text, invalid bytes replaced.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// The signals that end a process, by number, on this platform. The numbers of `SIGBUS` and
/// `SIGSYS` differ between macOS and Linux, so those are listed per platform.
const SIGNALS: &[(i32, &str)] = &[
    (1, "SIGHUP"),
    (2, "SIGINT"),
    (3, "SIGQUIT"),
    (4, "SIGILL"),
    (5, "SIGTRAP"),
    (6, "SIGABRT"),
    #[cfg(not(target_os = "macos"))]
    (7, "SIGBUS"),
    #[cfg(target_os = "macos")]
    (10, "SIGBUS"),
    #[cfg(target_os = "macos")]
    (12, "SIGSYS"),
    (8, "SIGFPE"),
    (9, "SIGKILL"),
    (11, "SIGSEGV"),
    (13, "SIGPIPE"),
    (14, "SIGALRM"),
    (15, "SIGTERM"),
    (24, "SIGXCPU"),
    (25, "SIGXFSZ"),
];

/// The name of a signal that ends a process on this platform, or `unnamed`.
pub fn signal_name(signal: i32) -> &'static str {
    SIGNALS
        .iter()
        .find(|(number, _)| *number == signal)
        .map_or("unnamed", |(_, name)| name)
}

/// Something that runs a [`Cmd`] and waits for it.
pub trait Runner {
    /// Run `cmd` to its end.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, cmd: &Cmd) -> io::Result<Exec>;
}

/// The real runner: `std::process::Command` with `env_clear`, stdin closed, stdout and
/// stderr captured, and the command killed when it outlives its limit.
pub struct SystemRunner;

/// How often a command with a limit is looked at.
const POLL: Duration = Duration::from_millis(10);

/// How long to wait for the output of a command that was killed (a grandchild may hold
/// the pipe open); what has not arrived by then is dropped.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// How much of a child's standard error is kept: it is only ever shown in a message.
const STDERR_MAX: usize = 1024 * 1024;

/// A thread that reads `stream` to its end and sends the first `keep` bytes and one more;
/// the rest is read and dropped, so the child never blocks on a full pipe and memory stays
/// bounded.
fn drain<R: Read + Send + 'static>(stream: Option<R>, keep: usize) -> mpsc::Receiver<Vec<u8>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        // A read that fails part way keeps what came before it.
        stream.into_iter().for_each(|mut stream| {
            let limit = u64::try_from(keep).map_or(u64::MAX, |k| k.saturating_add(1));
            let _ = stream.by_ref().take(limit).read_to_end(&mut bytes);
            let _ = io::copy(&mut stream, &mut io::sink());
        });
        let _ = send.send(bytes);
    });
    receive
}

/// How waiting for a child ended.
enum Waited {
    Exited(ExitStatus),
    /// Still running at its time limit.
    TimedOut,
    /// Wrote more than its [`WriteCap`] allows.
    OverCap,
}

/// Wait for `child`, for at most `limit` when there is one, killing nothing: a child that
/// outlives the limit, or writes past `cap` (also by the time it exits), is reported and
/// left to the caller.
fn wait_watching(
    child: &mut Child,
    limit: Option<Duration>,
    cap: Option<&WriteCap>,
) -> io::Result<Waited> {
    if limit.is_none() && cap.is_none() {
        return child.wait().map(Waited::Exited);
    }
    let deadline = limit.map(|limit| Instant::now() + limit);
    loop {
        if let Some(status) = child.try_wait()? {
            // A child that wrote past the limit between two looks has still written it.
            let over = cap.is_some_and(WriteCap::exceeded);
            return Ok(if over {
                Waited::OverCap
            } else {
                Waited::Exited(status)
            });
        }
        if cap.is_some_and(WriteCap::exceeded) {
            return Ok(Waited::OverCap);
        }
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if left.is_some_and(|left| left.is_zero()) {
            return Ok(Waited::TimedOut);
        }
        thread::sleep(left.map_or(POLL, |left| POLL.min(left)));
    }
}

/// How the child gets its ids. A drop to another user is made by this process: by
/// `Command::uid` and `gid` where the standard library's `setgroups(0)` empties the groups
/// (Linux), and, with `exact_groups`, in the child just before it executes the program, which
/// sets the supplementary groups to the one gid, then the gid, then the uid ([`ids::switch_to`]).
/// The second is for macOS, where the standard library's `setgroups(0)` leaves the old
/// effective gid (group 0, `wheel`, for root) as the child's only supplementary group.
/// (`chroot(8)` would set the groups too, but a process that calls `chroot` and then executes
/// a program is killed by the system on macOS, `SIGKILL` at once, even for `/` and
/// `/usr/bin/true`.) The second refuses a target of uid 0 or gid 0 (`PermissionDenied`): it
/// would otherwise run the program as root, or in group 0, without a drop.
fn drop_ids(command: &mut Command, run_as: Option<Identity>, exact_groups: bool) -> io::Result<()> {
    let Some(id) = run_as else { return Ok(()) };
    if !exact_groups {
        command.uid(id.uid).gid(id.gid);
        return Ok(());
    }
    if id.uid == 0 || id.gid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a privilege drop with exact groups needs a uid and a gid that are not root's",
        ));
    }
    // The child starts in `/`: the caller's directory may be one the new user cannot enter.
    command.current_dir("/");
    ids::switch_to(command, id);
    Ok(())
}

/// Refuse a privilege drop made by this process when its effective uid is root and its real
/// uid is not (a setuid-style launch): the standard library empties the supplementary groups
/// only when the REAL uid is root, so such a drop would keep the caller's groups.
fn refuse_setuid_drop(set_ids: Option<Identity>, uid: u32, euid: u32) -> io::Result<()> {
    if set_ids.is_some() && euid == 0 && uid != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a privilege drop needs a real uid of root: the supplementary groups would stay",
        ));
    }
    Ok(())
}

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        refuse_setuid_drop(
            cmd.run_as,
            rustix::process::getuid().as_raw(),
            rustix::process::geteuid().as_raw(),
        )?;
        let mut command = Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .env_clear()
            .env("PATH", PATH)
            .env("LANG", LANG)
            .env("DEBIAN_FRONTEND", DEBIAN_FRONTEND)
            .envs(cmd.env.iter().map(|(k, v)| (k, v)))
            .stdin(if cmd.stdin.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        drop_ids(&mut command, cmd.run_as, cfg!(target_os = "macos"))?;
        let mut child = command.spawn()?;
        // The input is written from a thread, or a child that writes before it reads
        // blocks on a full pipe while this waits for it.
        if let Some(mut input) = child.stdin.take() {
            let bytes = cmd.stdin.clone();
            thread::spawn(move || {
                // A child that stops reading closes the pipe; there is nobody to tell.
                let _ = input.write_all(&bytes);
            });
        }
        // Both streams are read while the child runs, or it blocks on a full pipe.
        let stdout = drain(child.stdout.take(), cmd.stdout_max.unwrap_or(usize::MAX));
        let stderr = drain(child.stderr.take(), STDERR_MAX);
        let status = match wait_watching(&mut child, cmd.timeout, cmd.write_cap.as_ref())? {
            Waited::Exited(status) => status,
            // Past its limit: kill it and reap it. A kill that fails means it just ended.
            Waited::TimedOut => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} ran past its limit and was killed", cmd.program),
                ));
            }
            Waited::OverCap => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    format!("{} wrote past its size limit and was killed", cmd.program),
                ));
            }
        };
        let collect =
            |bytes: mpsc::Receiver<Vec<u8>>| bytes.recv_timeout(DRAIN_GRACE).unwrap_or_default();
        Ok(Exec {
            code: status.code(),
            signal: status.signal(),
            stdout: collect(stdout),
            stderr: collect(stderr),
        })
    }
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
