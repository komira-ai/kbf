//! Running a program: an `execve` with an explicit argument vector and a cleared
//! environment, behind a trait so tests record the vectors instead of running them.
//!
//! No shell is involved anywhere: a [`Cmd`] is a program path and its arguments, and the
//! only environment a child sees is [`PATH`], [`LANG`] and [`DEBIAN_FRONTEND`].

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
    /// caller's own. A root caller is needed (see [`launch`]). On macOS the command starts in
    /// `/`, because `chroot` sets the working directory.
    pub run_as: Option<Identity>,
    /// Bytes written to the child's standard input, which is then closed; none for an
    /// input that is closed at once.
    pub stdin: Vec<u8>,
}

/// How a child ended and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// The exit status, or none when a signal ended it.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Exec {
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

/// A thread that reads `stream` to its end and sends what it read.
fn drain<R: Read + Send + 'static>(stream: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        // A read that fails part way keeps what came before it.
        stream.into_iter().for_each(|mut stream| {
            let _ = stream.read_to_end(&mut bytes);
        });
        let _ = send.send(bytes);
    });
    receive
}

/// Wait for `child` for at most `limit`; none when it is still running then.
fn wait_within(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        thread::sleep(POLL.min(left));
    }
}

/// macOS's `chroot(8)`, which sets the supplementary groups itself before it drops to a user.
const CHROOT: &str = "/usr/sbin/chroot";

/// What is executed for a command.
#[derive(Debug, PartialEq, Eq)]
struct Launch {
    program: String,
    args: Vec<String>,
    /// The ids this process sets on the child (`Command::uid` and `gid`), for a drop it makes
    /// itself.
    set_ids: Option<Identity>,
}

/// How `cmd` is started. A drop to another user is made by this process (`Command::uid` and
/// `gid`) unless `through_chroot`, because the standard library empties the supplementary
/// groups with `setgroups(0)` before it sets the gid, and on macOS that leaves the old
/// effective gid (group 0, `wheel`, for root) as the child's only supplementary group;
/// `CommandExt::groups` would set them explicitly but is not stable. `chroot -u -g -G /`
/// sets the groups to the one gid, drops, and runs the program; the root it enters is `/`.
/// That path refuses a target of uid 0 or gid 0 (`PermissionDenied`), which `refuse_setuid_drop`
/// does not cover there: `chroot` would run the program as root, or in group 0, without a drop.
fn launch(cmd: &Cmd, through_chroot: bool) -> io::Result<Launch> {
    Ok(match cmd.run_as {
        Some(id) if through_chroot => {
            if id.uid == 0 || id.gid == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "a privilege drop through chroot needs a uid and a gid that are not root's",
                ));
            }
            let ids = [id.uid, id.gid, id.gid].map(|n| n.to_string());
            let mut args: Vec<String> = ["-u", &ids[0], "-g", &ids[1], "-G", &ids[2], "/"]
                .map(str::to_owned)
                .to_vec();
            args.push(cmd.program.clone());
            args.extend(cmd.args.iter().cloned());
            Launch {
                program: CHROOT.to_owned(),
                args,
                set_ids: None,
            }
        }
        other => Launch {
            program: cmd.program.clone(),
            args: cmd.args.clone(),
            set_ids: other,
        },
    })
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
        let launch = launch(cmd, cfg!(target_os = "macos"))?;
        refuse_setuid_drop(
            launch.set_ids,
            rustix::process::getuid().as_raw(),
            rustix::process::geteuid().as_raw(),
        )?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
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
        if let Some(id) = launch.set_ids {
            command.uid(id.uid).gid(id.gid);
        }
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
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = match cmd.timeout {
            None => Some(child.wait()?),
            Some(limit) => wait_within(&mut child, limit)?,
        };
        let Some(status) = status else {
            // Past its limit: kill it and reap it. A kill that fails means it just ended.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} ran past its limit and was killed", cmd.program),
            ));
        };
        let collect =
            |bytes: mpsc::Receiver<Vec<u8>>| bytes.recv_timeout(DRAIN_GRACE).unwrap_or_default();
        Ok(Exec {
            code: status.code(),
            stdout: collect(stdout),
            stderr: collect(stderr),
        })
    }
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
