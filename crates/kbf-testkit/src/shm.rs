//! The `shm-owner` and `shm-tracker` steps of `kbf-act` (see [`crate::act`]), which mimic
//! how Python's `multiprocessing` keeps POSIX shared memory and semaphores, and the
//! checks a test makes of the names they leave.
//!
//! - `shm-owner PREFIX COUNT [SECS]`: makes `COUNT` shared-memory names `/PREFIX-m<i>` and
//!   `COUNT` semaphore names `/PREFIX-s<i>` (`i` from 0), after starting a tracker
//!   child (`shm-tracker`) that holds a pipe from this process and waiting until it is
//!   ready (so the names exist only once it handles SIGTERM), prints `shm-owner ready TRACKER`
//!   (the tracker's process id), and blocks for `SECS` seconds (five
//!   minutes without it) unless it is ended first, then exits 0. Nothing in this process removes a name.
//! - `shm-tracker PREFIX COUNT`: the child `shm-owner` starts; not for a test to run. It
//!   reads its standard input, the pipe, prints `ready`, and when the pipe closes (the
//!   owner has gone) or it gets SIGTERM, unlinks every name `shm-owner` made and exits
//!   0. SIGKILL gives it no chance to: its names stay, as they do with the real tracker.
//!
//! A name is at most 31 bytes on macOS, the slash included, so the prefix is short.
//!
//! [`present`] and [`unlink`] are for the test: they look for each name by opening it
//! without creating it (`ENOENT` is gone), and remove the ones that are left.

use std::ffi::CString;
use std::io::{BufRead as _, BufReader};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crate::{Error, failed, number, write_line};

/// How long `shm-owner` blocks when nothing ends it.
const HOLD: Duration = Duration::from_secs(300);

/// One of the two steps, checked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Shm<'a> {
    Owner {
        prefix: &'a str,
        count: u32,
        hold: Duration,
    },
    Tracker {
        prefix: &'a str,
        count: u32,
    },
}

/// The step `words` spell, `None` when they spell neither name. A name with arguments
/// that do not fit is a usage error.
pub(crate) fn parse<'a>(words: &[&'a str]) -> Result<Option<Shm<'a>>, Error> {
    let (name, prefix, count, hold) = match words {
        [name @ ("shm-owner" | "shm-tracker"), prefix, count] => (*name, *prefix, *count, None),
        ["shm-owner", prefix, count, secs] => ("shm-owner", *prefix, *count, Some(*secs)),
        _ => return Ok(None),
    };
    let count = u32::try_from(number(count)?)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| Error::Usage(format!("{count:?} is not a count of names")))?;
    if prefix.is_empty() || prefix.contains('/') || prefix.len() + "/-m4294967295".len() > 31 {
        return Err(Error::Usage(format!("{prefix:?} is not a name prefix")));
    }
    Ok(Some(if name == "shm-owner" {
        let hold = hold.map_or(Ok(HOLD), crate::act::seconds)?;
        Shm::Owner {
            prefix,
            count,
            hold,
        }
    } else {
        Shm::Tracker { prefix, count }
    }))
}

/// The shared-memory names then the semaphore names of `prefix`.
fn names(prefix: &str, count: u32) -> Vec<(Kind, CString)> {
    let make = |kind: Kind, letter: char| {
        (0..count).map(move |i| {
            let name = CString::new(format!("/{prefix}-{letter}{i}")).expect("no NUL in a name");
            (kind, name)
        })
    };
    make(Kind::Memory, 'm')
        .chain(make(Kind::Semaphore, 's'))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Memory,
    Semaphore,
}

/// Opens `name` without creating it: whether it exists.
fn exists(kind: Kind, name: &CString) -> bool {
    match kind {
        Kind::Memory => {
            // SAFETY: `name` is a NUL-terminated string that outlives the call; the
            // descriptor it returns is closed at once.
            let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
            if fd >= 0 {
                // SAFETY: `fd` was just opened here and nothing else holds it.
                unsafe { libc::close(fd) };
            }
            fd >= 0
        }
        Kind::Semaphore => {
            // SAFETY: as above; without O_CREAT, sem_open takes no further argument.
            let sem = unsafe { libc::sem_open(name.as_ptr(), 0) };
            if sem_failed(sem) {
                return false;
            }
            // SAFETY: `sem` was just opened here and is closed once.
            unsafe { libc::sem_close(sem) };
            true
        }
    }
}

/// Whether `sem_open` returned its failure value (`SEM_FAILED`: -1 on macOS, null on
/// Linux).
fn sem_failed(sem: *mut libc::sem_t) -> bool {
    sem.is_null() || sem as isize == -1
}

/// Makes `name`, replacing one that is there.
fn make(kind: Kind, name: &CString) -> std::io::Result<()> {
    remove(kind, name);
    match kind {
        Kind::Memory => {
            // SAFETY: `name` is a NUL-terminated string that outlives the call.
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `fd` was just opened here and nothing else holds it.
            unsafe { libc::close(fd) };
        }
        Kind::Semaphore => {
            // SAFETY: with O_CREAT, sem_open takes a mode and an initial value, both
            // unsigned ints.
            let sem = unsafe {
                libc::sem_open(
                    name.as_ptr(),
                    libc::O_CREAT | libc::O_EXCL,
                    0o600 as libc::c_uint,
                    0 as libc::c_uint,
                )
            };
            if sem_failed(sem) {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `sem` was just opened here and is closed once.
            unsafe { libc::sem_close(sem) };
        }
    }
    Ok(())
}

/// Unlinks `name`; a name that is not there is not an error.
fn remove(kind: Kind, name: &CString) {
    // SAFETY: `name` is a NUL-terminated string that outlives the call.
    unsafe {
        match kind {
            Kind::Memory => libc::shm_unlink(name.as_ptr()),
            Kind::Semaphore => libc::sem_unlink(name.as_ptr()),
        };
    }
}

/// The names `shm-owner PREFIX COUNT` made that still exist.
#[must_use]
pub fn present(prefix: &str, count: u32) -> Vec<String> {
    names(prefix, count)
        .into_iter()
        .filter(|(kind, name)| exists(*kind, name))
        .map(|(_, name)| name.to_string_lossy().into_owned())
        .collect()
}

/// Unlinks every name `shm-owner PREFIX COUNT` made, those that are gone included.
pub fn unlink(prefix: &str, count: u32) {
    for (kind, name) in names(prefix, count) {
        remove(kind, &name);
    }
}

/// Performs the step.
pub(crate) fn perform(step: &Shm<'_>) -> Result<(), Error> {
    match *step {
        Shm::Owner {
            prefix,
            count,
            hold,
        } => owner(prefix, count, hold),
        Shm::Tracker { prefix, count } => tracker(prefix, count),
    }
}

fn owner(prefix: &str, count: u32, hold: Duration) -> Result<(), Error> {
    // The tracker first, so that its SIGTERM handler is in place before any name
    // exists: a test that ends this process once the names are there cannot reach a
    // tracker that would die on the signal and unlink nothing. (Python's tracker is told
    // of a name only once it runs, too.)
    let mut child = std::env::current_exe()
        .and_then(|exe| {
            Command::new(exe)
                .arg0(crate::act::NAME)
                .args(["shm-tracker", prefix, &count.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
        })
        .map_err(failed("start the tracker"))?;
    let out = child
        .stdout
        .take()
        .ok_or_else(|| Error::Failed("no pipe from the tracker".to_owned()))?;
    let mut said = String::new();
    BufReader::new(out)
        .read_line(&mut said)
        .map_err(failed("read the tracker"))?;
    ready(&said)?;
    for (kind, name) in names(prefix, count) {
        make(kind, &name).map_err(failed(format!("make {name:?}")))?;
    }
    write_line(
        std::io::stdout(),
        &format!("shm-owner ready {}", child.id()),
    )?;
    // The tracker's pipe stays open (`child.stdin`) for as long as this process lives.
    std::thread::sleep(hold);
    drop(child);
    Ok(())
}

/// Whether `said` is the line a tracker prints once it handles SIGTERM.
fn ready(said: &str) -> Result<(), Error> {
    if said.trim() == "ready" {
        Ok(())
    } else {
        Err(Error::Failed(format!("the tracker said {said:?}")))
    }
}

/// The write end of the tracker's wake-up pipe, for the signal handler.
static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_term(_: libc::c_int) {
    let fd = WAKE.load(Ordering::Relaxed);
    // SAFETY: write(2) is async-signal-safe; `fd` is the pipe made before the handler
    // was installed. A failed write only means the tracker is already waking.
    unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) };
}

fn tracker(prefix: &str, count: u32) -> Result<(), Error> {
    let (wake, writer) = std::io::pipe().map_err(failed("pipe"))?;
    WAKE.store(writer.as_raw_fd(), Ordering::Relaxed);
    // SAFETY: `on_term` only calls write(2); the handler is installed once, here.
    unsafe { libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t) };
    write_line(std::io::stdout(), "ready")?;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` holds two valid pollfds for the length passed.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            // A signal (SIGTERM, whose handler has woken the pipe) interrupted the wait.
            continue;
        }
        // Ready to read is the end of the pipe (the owner never writes to it) or the
        // wake-up from the SIGTERM handler: either way the names go.
        break;
    }
    drop(writer);
    unlink(prefix, count);
    Ok(())
}

#[cfg(test)]
mod tests;
