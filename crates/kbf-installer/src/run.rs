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
        if let Some(user) = spawn.run_as.filter(needs_switch) {
            // SAFETY: the closure runs in the child between fork and exec and makes
            // three system calls on numbers copied in; it allocates nothing.
            unsafe {
                command.pre_exec(in_child(user));
            }
        }
        let mut child = command.spawn()?;
        let out = capture(child.stdout.take().expect("stdout is piped"));
        let err = capture(child.stderr.take().expect("stderr is piped"));
        let end = Instant::now() + spawn.timeout;
        let (status, timed_out) = loop {
            if let Some(status) = child.try_wait()? {
                break (status, false);
            }
            if Instant::now() >= end {
                kill_group(child.id());
                break (child.wait()?, true);
            }
            thread::sleep(Duration::from_millis(5));
        };
        // Whatever the program left running with the pipes open dies with its group.
        kill_group(child.id());
        Ok(Ran {
            code: status.code(),
            stdout: out.join().expect("the reader does not panic")?,
            stderr: err.join().expect("the reader does not panic")?,
            timed_out,
        })
    }
}

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

/// What the child runs between fork and exec to become `user`: [`switch_user`] on the
/// real calls. A named function, so that the closure is not part of [`SystemRunner::run`]
/// (which the coverage run cannot see finish in the child).
fn in_child(user: RunAs) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || switch_user(&Libc, user)
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

/// A thread that reads `pipe` to its end, keeping the first [`CAPTURE`] bytes.
fn capture(pipe: impl Read + Send + 'static) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut pipe = pipe;
        let mut kept = Vec::new();
        (&mut pipe).take(CAPTURE).read_to_end(&mut kept)?;
        io::copy(&mut pipe, &mut io::sink())?;
        Ok(kept)
    })
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
