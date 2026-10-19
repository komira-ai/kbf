//! `kbf-init`: the first process of an action's container, which runs the action's
//! program as its child and reaps every process the container leaves orphaned.
//!
//! In a PID namespace the first process adopts every orphan, and a process nobody
//! waits for stays a zombie: a test that kills its process group and then asks
//! `kill(-pgid, 0)` is told the group is alive. A host's init and Buildbarn's runner
//! reap; without this program the action itself was the container's first process and
//! did not. See docs/design/daemon.md (the container sandbox).
//!
//! Usage: `kbf-init -- <program> [argument...]`, with the working directory and the
//! environment the program is to have.
//!
//! - The program is looked up as `crun` looks it up: a name with a slash as it is (a
//!   relative one from the working directory), a bare name on the environment's `PATH`
//!   and nowhere else (no `PATH`, not found). It runs with `argv[0]` as given, with
//!   `/dev/null` as stdin and this program's stdout and stderr.
//! - Every signal this program can catch is passed on to the program; it exits when the
//!   program does, with its exit code, or `128 + n` if signal `n` ended it (what `crun`
//!   reported when the program was the container's first process). The container's other
//!   processes die with it, as the kernel ends a namespace whose first process has gone.
//! - This program's stdin is not the program's: it is a file the driver opened, which
//!   only this program holds. When the program cannot be started, one line saying why,
//!   in `crun`'s words, is written to it, and the exit code is 127 (not found) or 126
//!   (anything else), as a shell answers; the driver reads the line back
//!   (`crates/kbf-driver-container/src/program.rs`).
//! - It makes itself the subreaper of its descendants, so that run on a host, outside
//!   a PID namespace, it reaps what its program orphans (the tests do that).
//!
//! The driver builds this file with `rustc` in its build script, statically linked, so
//! it runs in any image whatever the image's libc. Standard library only: the few libc
//! functions it needs are declared here.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// A `sigset_t`: 1024 bits on Linux with glibc and with musl.
#[repr(C, align(8))]
struct SigSet([u64; 16]);

const SIG_SETMASK: i32 = 2;
const SIGCHLD: i32 = 17;
const WNOHANG: i32 = 1;
const PR_SET_CHILD_SUBREAPER: i32 = 36;
const ENOENT: i32 = 2;
const ETXTBSY: i32 = 26;
/// The signals a fault of this program raises in itself: left unblocked.
const SYNCHRONOUS: [i32; 7] = [4, 5, 6, 7, 8, 11, 31];

unsafe extern "C" {
    fn sigfillset(set: *mut SigSet) -> i32;
    fn sigemptyset(set: *mut SigSet) -> i32;
    fn sigdelset(set: *mut SigSet, signal: i32) -> i32;
    fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
    fn sigwait(set: *const SigSet, signal: *mut i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
    fn write(fd: i32, bytes: *const std::ffi::c_void, count: usize) -> isize;
}

/// Why the program could not be started, in `crun`'s words.
struct Refused {
    exit: i32,
    words: String,
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(dashes), Some(program)) = (args.next(), args.next()) else {
        usage();
    };
    if dashes != "--" {
        usage();
    }
    let arguments: Vec<OsString> = args.collect();

    // Blocked, then taken one at a time with sigwait: no handler, no race.
    let mut set = SigSet([0; 16]);
    // SAFETY: `set` is a valid, writable sigset of the size libc expects.
    unsafe {
        sigfillset(&mut set);
        for signal in SYNCHRONOUS {
            sigdelset(&mut set, signal);
        }
        pthread_sigmask(SIG_SETMASK, &set, std::ptr::null_mut());
        prctl(PR_SET_CHILD_SUBREAPER, 1u64);
    }

    let child = match start(&program, &arguments) {
        Ok(child) => child,
        Err(refused) => {
            report(&refused.words);
            std::process::exit(refused.exit);
        }
    };
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    loop {
        let mut signal = 0;
        // SAFETY: `set` is initialised, `signal` is a valid int.
        if unsafe { sigwait(&set, &mut signal) } != 0 {
            continue;
        }
        if signal != SIGCHLD {
            // SAFETY: a plain system call on a pid and a signal number.
            unsafe { kill(pid, signal) };
            continue;
        }
        if let Some(status) = reap(pid) {
            std::process::exit(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            );
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: kbf-init -- <program> [argument...]");
    std::process::exit(2);
}

/// Waits for every child that has ended, the program's status if it is among them.
fn reap(program: i32) -> Option<ExitStatus> {
    let mut found = None;
    loop {
        let mut raw = 0;
        // SAFETY: `raw` is a valid int.
        let ended = unsafe { waitpid(-1, &mut raw, WNOHANG) };
        if ended <= 0 {
            return found;
        }
        if ended == program {
            found = Some(ExitStatus::from_raw(raw));
        }
    }
}

/// Starts `program`, with the mask of signals a process starts with.
fn start(program: &OsStr, arguments: &[OsString]) -> Result<std::process::Child, Refused> {
    let path = lookup(program)?;
    let mut command = Command::new(path);
    command.arg0(program).args(arguments).stdin(Stdio::null());
    // SAFETY: the closure runs between fork and exec and makes two async-signal-safe
    // calls on a local; it allocates nothing.
    unsafe {
        command.pre_exec(|| {
            let mut none = SigSet([0; 16]);
            sigemptyset(&mut none);
            pthread_sigmask(SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    command.spawn().map_err(|e| refusal(program, &e))
}

/// What file to run for `program`, as `crun` decides it.
fn lookup(program: &OsStr) -> Result<PathBuf, Refused> {
    if program.as_bytes().contains(&b'/') {
        return Ok(PathBuf::from(program));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in path.as_bytes().split(|b| *b == b':') {
        let directory = if directory.is_empty() {
            b"."
        } else {
            directory
        };
        let candidate = Path::new(OsStr::from_bytes(directory)).join(program);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            use std::os::unix::fs::PermissionsExt as _;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Ok(candidate);
            }
        }
    }
    Err(not_found(program))
}

fn not_found(program: &OsStr) -> Refused {
    Refused {
        exit: 127,
        words: format!(
            "executable file `{}` not found in $PATH: No such file or directory",
            program.to_string_lossy()
        ),
    }
}

/// The report of an exec that failed with `error`: `crun`'s words for it.
fn refusal(program: &OsStr, error: &std::io::Error) -> Refused {
    match error.raw_os_error() {
        Some(ENOENT) => not_found(program),
        Some(ETXTBSY) => Refused {
            exit: 126,
            words: format!(
                "exec container process `{}`: Text file busy",
                program.to_string_lossy()
            ),
        },
        _ => Refused {
            exit: 126,
            words: format!("open executable: {}", strip(error)),
        },
    }
}

/// The system's words for an error, without Rust's ` (os error N)`.
fn strip(error: &std::io::Error) -> String {
    let text = error.to_string();
    text.split(" (os error").next().unwrap_or(&text).to_owned()
}

/// Writes `words` and a newline to stdin, the driver's file. Nothing to say it could
/// not be written: the exit code still tells.
fn report(words: &str) {
    let line = format!("{words}\n");
    // SAFETY: the buffer is valid for its length.
    unsafe { write(0, line.as_ptr().cast(), line.len()) };
}
