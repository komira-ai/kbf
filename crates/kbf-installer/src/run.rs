//! Running a program: the one place this crate starts a process, behind a trait so a
//! test plays the program.
//!
//! Every program the installer runs is run the same way: by absolute path, with an
//! environment of exactly the variables it is given (never the caller's), in a directory
//! it is given, with no standard input, for at most a time it is given, and, when it is
//! given a [`RunAs`], as that user with no supplementary groups. [`SystemRunner`] does
//! it; a test's runner records the [`Spawn`] it is handed (so a test reads the argv, the
//! environment and the user that would have been used) and answers as the test says.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A user and group a program runs as. Never root: the settings refuse that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
}

/// The `nobody` account: what a program that reads untrusted bytes runs as unless the
/// host's settings name another. Linux gives it 65534; on macOS it is -2, which a `uid_t`
/// holds as 4294967294 (UNVERIFIED on the farm's Macs: a Mac's settings may name it).
pub const fn nobody(platform: crate::layout::Platform) -> RunAs {
    match platform {
        crate::layout::Platform::Linux => RunAs {
            uid: 65534,
            gid: 65534,
        },
        crate::layout::Platform::MacOs => RunAs {
            uid: 4_294_967_294,
            gid: 4_294_967_294,
        },
    }
}

/// One program to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The whole environment: nothing else is passed.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub run_as: Option<RunAs>,
    /// The program is killed (with the processes it started) after this long.
    pub timeout: Duration,
}

/// How a program ended, and the first [`CAPTURE`] bytes it wrote to each output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// The exit status; `None` when a signal ended it (the timeout's kill included).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// The most of an output kept; the rest is read and dropped so the program never blocks
/// on a full pipe.
pub const CAPTURE: u64 = 64 * 1024;

pub trait Runner {
    /// Run `spawn` to its end. An error is a program that could not be started.
    fn run(&self, spawn: &Spawn) -> io::Result<Ran>;
}

/// Starts real processes.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, spawn: &Spawn) -> io::Result<Ran> {
        if !spawn.program.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an absolute path", spawn.program.display()),
            ));
        }
        let mut command = Command::new(&spawn.program);
        command
            .args(&spawn.args)
            .env_clear()
            .envs(spawn.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&spawn.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so the timeout can kill what it started too.
            .process_group(0);
        // SAFETY: the closure runs in the child between fork and exec; it makes system
        // calls on numbers copied in and allocates nothing.
        unsafe {
            command.pre_exec(before_exec(
                spawn.run_as.filter(needs_switch),
                FIRST_INHERITED,
            ));
        }
        let mut child = command.spawn()?;
        let out = capture(child.stdout.take().expect("stdout is piped"));
        let err = capture(child.stderr.take().expect("stderr is piped"));
        let end = Instant::now() + spawn.timeout;
        let (status, timed_out) = loop {
            // The program is looked at without being reaped, so that its pid (and with it
            // the process group's number) cannot be reused before the group is killed.
            if exited(child.id())? {
                kill_group(child.id());
                break (child.wait()?, false);
            }
            if Instant::now() >= end {
                kill_group(child.id());
                break (child.wait()?, true);
            }
            thread::sleep(Duration::from_millis(5));
        };
        Ok(Ran {
            code: status.code(),
            stdout: out.finish()?,
            stderr: err.finish()?,
            timed_out,
        })
    }
}

/// The first descriptor a program does not inherit: 0, 1 and 2 are its own.
const FIRST_INHERITED: u32 = 3;

/// How long the readers of a program's output get, after the program and its group are
/// dead, to reach the end of the pipes. A descendant that left the group (`setsid`) can
/// hold them open for as long as it lives; the output read so far is then all there is.
const READ_GRACE: Duration = Duration::from_secs(2);

/// Whether a switch to `user` is needed: not when the caller already is that user, which
/// is the only case an unprivileged caller can ask for.
fn needs_switch(user: &RunAs) -> bool {
    rustix::process::geteuid().as_raw() != user.uid
        || rustix::process::getegid().as_raw() != user.gid
}

/// The three system calls that change who a process is, so a test plays them (and reads
/// the order they are made in).
pub trait Creds {
    /// `setgroups(0, NULL)`: no supplementary groups.
    fn set_groups_none(&self) -> io::Result<()>;
    fn set_gid(&self, gid: u32) -> io::Result<()>;
    fn set_uid(&self, uid: u32) -> io::Result<()>;
}

/// The real calls.
pub struct Libc;

impl Creds for Libc {
    fn set_groups_none(&self) -> io::Result<()> {
        // SAFETY: setgroups with no groups reads no memory.
        check(unsafe { libc::setgroups(0, std::ptr::null()) })
    }

    fn set_gid(&self, gid: u32) -> io::Result<()> {
        // SAFETY: setgid takes any number; one the caller may not take fails.
        check(unsafe { libc::setgid(gid) })
    }

    fn set_uid(&self, uid: u32) -> io::Result<()> {
        // SAFETY: as setgid.
        check(unsafe { libc::setuid(uid) })
    }
}

/// `Ok` for the 0 a system call returns, else its `errno`.
fn check(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Switch the calling process to `user`: no supplementary groups, then the group, then
/// the user (the group first, because a process that is no longer root cannot change it),
/// as a login would not keep the root helper's. The first call that fails ends it.
pub fn switch_user(creds: &dyn Creds, user: RunAs) -> io::Result<()> {
    creds.set_groups_none()?;
    creds.set_gid(user.gid)?;
    creds.set_uid(user.uid)
}

/// What the child runs between fork and exec: mark every descriptor from `first_fd` up
/// (so it holds only its own standard streams, whatever the helper's parent left open),
/// then become `user` ([`switch_user`]) when it is given one. A named function, so that
/// the closure is not part of [`SystemRunner::run`] (which the coverage run cannot see
/// finish in the child) and a test can run it in this process with a high `first_fd`.
fn before_exec(
    user: Option<RunAs>,
    first_fd: u32,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || {
        cloexec_from(first_fd);
        match user {
            Some(user) => switch_user(&Libc, user),
            None => Ok(()),
        }
    }
}

/// `CLOSE_RANGE_CLOEXEC`: mark the range close-on-exec instead of closing it. Closing would
/// take the standard library's own pipe, over which a failed exec reports its error.
const CLOSE_RANGE_CLOEXEC: u32 = 4;

/// Make every descriptor numbered `first` or more close at exec. `close_range` with the
/// flag needs Linux 5.11; where it is missing the descriptors stay as they are (close-on-exec
/// already when this process made them, which is all Rust's standard library does).
fn cloexec_from(first: u32) {
    // SAFETY: close_range reads no memory and closes only descriptors of this process.
    unsafe {
        libc::syscall(libc::SYS_close_range, first, u32::MAX, CLOSE_RANGE_CLOEXEC);
    }
}

/// Whether the child `pid` has exited, without reaping it.
fn exited(pid: u32) -> io::Result<bool> {
    let pid = libc::id_t::from(pid);
    // SAFETY: waitid writes a siginfo_t, which a zeroed one is valid for; WNOWAIT leaves
    // the child to be waited for.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the union field is read after a successful waitid, which sets it (0 when
    // nothing has exited yet).
    Ok(unsafe { info.si_pid() } != 0)
}

/// SIGKILL to the process group `pgid` (the child made itself the leader of a group of
/// its own); a group already gone is not an error.
fn kill_group(pgid: u32) {
    let pgid = i32::try_from(pgid).expect("a pid fits an i32");
    // SAFETY: kill reads no memory; a negative pid names a process group.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// A thread reading `pipe` to its end, keeping the first [`CAPTURE`] bytes.
struct Capture {
    kept: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<io::Result<()>>,
}

/// A sink that keeps the first [`CAPTURE`] bytes written to it and says it took them all,
/// holding the lock only while it appends (the reader blocks in `read`, not here).
struct Keep(Arc<Mutex<Vec<u8>>>);

impl io::Write for Keep {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut kept = self.0.lock().expect("nothing panics holding the lock");
        let room = usize::try_from(CAPTURE).map_or(0, |cap| cap.saturating_sub(kept.len()));
        kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture(pipe: impl Read + Send + 'static) -> Capture {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let (sender, done) = mpsc::channel();
    let mut sink = Keep(Arc::clone(&kept));
    thread::spawn(move || {
        let mut pipe = pipe;
        let read = io::copy(&mut pipe, &mut sink).map(drop);
        // The receiver is gone when the grace ran out; nobody is waiting.
        let _ = sender.send(read);
    });
    Capture { kept, done }
}

impl Capture {
    /// What was read, once the pipe ended or [`READ_GRACE`] ran out (the reader is left
    /// to end on its own).
    fn finish(self) -> io::Result<Vec<u8>> {
        match self.done.recv_timeout(READ_GRACE) {
            Ok(read) => read?,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        Ok(self
            .kept
            .lock()
            .expect("the reader holds no lock across a panic")
            .clone())
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
