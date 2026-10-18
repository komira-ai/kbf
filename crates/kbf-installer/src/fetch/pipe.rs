//! The root parent's end of the fetch child: start it with nothing, read what it writes to
//! its standard output, count and hash every byte as it arrives, write them to a file the
//! parent created, and end the child the moment a cap is crossed.
//!
//! The child is a program the parent started as another user (`kbf-fetch`), with a cleared
//! environment, `/dev/null` for input, the working directory `/`, only descriptors 0, 1, 2 and
//! (for the frontend source) the one lent identity, and the limits of [`Confine::FETCH`]; it
//! dies with the parent (Linux) and it is the leader of a process group the parent kills at
//! the end, so nothing it started outlives it.
//! Its exit status is never an input: the bytes are judged by their count and their hash.
//!
//! Both of its pipes are read with `poll` and non-blocking reads: a child that stalls on one
//! cannot hold the other, nor the parent past the caps. What it writes to standard error is
//! kept to [`LOG_BYTES`] for the log and the rest is drained and dropped.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::run::{
    Confine, Inherited, READ_GRACE, RunAs, before_exec_confined, capture, kill_group, needs_switch,
};

/// How much of the child's standard error is kept.
pub const LOG_BYTES: usize = 4096;
/// The bytes read from the pipe at a time.
const CHUNK: usize = 64 * 1024;

/// The program that fetches, and what it is confined to.
#[derive(Clone, Debug)]
pub struct ChildSpec {
    /// The program: the installer's own binary on a host.
    pub program: PathBuf,
    /// The arguments before the fixed flags (`fetch-child` on a host).
    pub args: Vec<OsString>,
    /// Who it runs as. `None` is for a test; a host always names the fetch user.
    pub run_as: Option<RunAs>,
    pub confine: Confine,
}

/// How long the parent waits.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// The bytes the component is declared to have.
    pub size: u64,
    /// The longest the child may go without writing a byte.
    pub no_byte: Duration,
    /// The longest the whole read may take.
    pub total: Duration,
}

/// What came in.
#[derive(Debug, PartialEq, Eq)]
pub struct Received {
    pub count: u64,
    /// Lowercase hex sha256 of the bytes written to the file.
    pub sha256: String,
    /// What the child said on standard error (at most [`LOG_BYTES`], for a person to read).
    pub log: String,
}

/// Why the read ended without the declared bytes.
#[derive(Debug, thiserror::Error)]
pub enum PipeFault {
    #[error("cannot start the fetch child: {0}")]
    Spawn(io::Error),
    #[error("cannot read the fetch child's output: {0}")]
    Read(io::Error),
    #[error("cannot write the download: {0}")]
    Write(io::Error),
    #[error("the source sent more than the {size} bytes declared; the child was killed")]
    Oversize { size: u64 },
    #[error("the child ended after {got} of the {size} bytes declared{}", log_suffix(.log))]
    Short { got: u64, size: u64, log: String },
    #[error("the child sent no byte for {} s after {got} of {size} bytes; it was killed", .after.as_secs())]
    Stalled { got: u64, size: u64, after: Duration },
    #[error("the fetch took more than {} s; the child was killed", .after.as_secs())]
    TimedOut { after: Duration },
}

fn log_suffix(log: &str) -> String {
    if log.is_empty() {
        String::new()
    } else {
        format!(" ({log})")
    }
}

/// Kills the child's group and reaps it, whatever way the read ends.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        kill_group(self.0.id());
        let _ = self.0.wait();
    }
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor this process holds.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    crate::run::check(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })
}

/// Wait for `fd` to be readable for at most `wait`; whether it is (or hung up).
fn readable(fd: i32, wait: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // Rounded up, so a wait of a fraction of a millisecond is not a spin.
    let ms = wait.as_millis().saturating_add(1).min(i32::MAX as u128) as libc::c_int;
    // SAFETY: poll reads and writes the one struct it is given.
    let n = unsafe { libc::poll(&raw mut poll, 1, ms) };
    match n {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Start the child with `flags` after its fixed arguments, and write what it sends to `out`.
/// `lend`, if given, is the one descriptor besides the three standard ones the child keeps (the
/// identity); the caller names its number in `flags`.
/// Returns once the child has sent exactly `caps.size` bytes and closed its output (or, having
/// sent them all, stalled or run out the whole time: it is then judged on the bytes it gave).
pub fn receive(
    spec: &ChildSpec,
    flags: &[OsString],
    lend: Option<BorrowedFd<'_>>,
    caps: Caps,
    out: &mut File,
) -> Result<Received, PipeFault> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .args(flags)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let inherited = Inherited::of(3).map_err(PipeFault::Spawn)?;
    // SAFETY: the closure runs in the child between fork and exec and makes only system calls
    // on numbers copied in.
    unsafe {
        command.pre_exec(before_exec_confined(
            spec.run_as.filter(needs_switch),
            inherited,
            Some(spec.confine),
            lend.map(|fd| fd.as_raw_fd()),
        ));
    }
    let mut guard = Reaper(command.spawn().map_err(PipeFault::Spawn)?);
    let stderr = capture(guard.0.stderr.take().expect("stderr is piped"));
    let mut stdout = guard.0.stdout.take().expect("stdout is piped");
    let fd = stdout.as_raw_fd();
    set_nonblocking(fd).map_err(PipeFault::Read)?;

    let started = Instant::now();
    let mut last_byte = started;
    let mut hash = Sha256::new();
    let mut count = 0_u64;
    let mut buf = vec![0_u8; CHUNK];
    loop {
        let now = Instant::now();
        let whole = started + caps.total;
        let quiet = last_byte + caps.no_byte;
        if now >= whole.min(quiet) {
            // Having all the bytes, a child that will not close its output is judged on them.
            if count == caps.size {
                break;
            }
            return Err(if now >= whole {
                PipeFault::TimedOut { after: caps.total }
            } else {
                PipeFault::Stalled {
                    got: count,
                    size: caps.size,
                    after: caps.no_byte,
                }
            });
        }
        if !readable(fd, whole.min(quiet) - now).map_err(PipeFault::Read)? {
            continue;
        }
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                count += n as u64;
                if count > caps.size {
                    return Err(PipeFault::Oversize { size: caps.size });
                }
                last_byte = Instant::now();
                hash.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(PipeFault::Write)?;
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
            Err(e) => return Err(PipeFault::Read(e)),
        }
    }
    out.sync_all().map_err(PipeFault::Write)?;
    // The child is done, or judged on the bytes it gave: end it and what it started, then take
    // what it said.
    drop(guard);
    let log = stderr.finish(Instant::now() + READ_GRACE).unwrap_or_default();
    let log = String::from_utf8_lossy(&log[..log.len().min(LOG_BYTES)])
        .trim()
        .to_owned();
    if count < caps.size {
        return Err(PipeFault::Short {
            got: count,
            size: caps.size,
            log,
        });
    }
    Ok(Received {
        count,
        sha256: hex::encode(hash.finalize()),
        log,
    })
}

#[cfg(test)]
#[path = "pipe_tests.rs"]
mod tests;
