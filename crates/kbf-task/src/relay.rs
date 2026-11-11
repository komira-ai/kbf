//! The processes that keep the Command off PID 1 of its PID namespace (Linux), run by this same
//! binary in two hidden modes.
//!
//! The process the tool starts makes the namespaces and the sandbox's mounts between `fork` and
//! `exec` ([`crate::child`]) and then executes this binary as
//! `kbf-task --sandbox-relay --group <pgid> [--userns] -- <program> <args...>` ([`argv`]). The
//! `unshare` made a PID namespace for its children, not for itself, so:
//!
//! - **The relay** ([`relay`]), outside the namespace, starts this binary again as
//!   `--sandbox-init`, which becomes the namespace's first process (PID 1). It then leaves the
//!   Command's process group for the tool's own (`--group`), so that neither a Command that
//!   signals its own group (`kill 0`) nor the tool's kill of that group at the deadline ends it
//!   before it has reaped the namespace, and the tool's resource figures cover every process of
//!   the run. It waits for the first process and leaves with its exit code. Where no PID namespace
//!   is pending (the `unshare` failed), it executes the Command itself, as the tool would have.
//! - **The first process** ([`init`]) is killed when the relay dies (`PR_SET_PDEATHSIG`), is
//!   non-dumpable, and, with `--userns` (the Command is in a user namespace of its own, where it
//!   would hold every capability), drops every capability and sets `no_new_privs`, so that
//!   neither the Command nor a file capability of a program it runs gives it one there. It starts
//!   the Command with standard input closed, reaps every process handed to it until the
//!   Command's ends, and leaves with the Command's exit code. When it ends, the kernel kills every
//!   process left in the namespace, and its end is seen by the relay only once they are all gone.
//!
//! An exit code is passed up as it is; a process a signal ended is passed up as 128 plus the
//! signal ([`exit_code`]).
//!
//! # The start report
//!
//! Both get, as their standard input, the write end of the pipe the tool reads the child's
//! report on ([`crate::child::Report`]). Whichever starts the Command writes two bytes there:
//! [`STARTED`] and 0 once it has started, or [`NOT_STARTED`] and the `errno` when it could not
//! (the tool then fails the run start, exit 11, as for any program that cannot be started). A
//! Command the relay executes itself gets standard input closed by the `exec`, so its successful
//! start is the end of the pipe.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use rustix::process::{Pid, Signal, WaitOptions, WaitStatus};

/// The first argument that selects the relay.
pub const RELAY: &str = "--sandbox-relay";
/// The first argument that selects the namespace's first process.
pub const INIT: &str = "--sandbox-init";

/// The start report: the Command was started.
pub const STARTED: u8 = 0;
/// The start report: the Command could not be started; the `errno` follows.
pub const NOT_STARTED: u8 = 1;

/// The exit code of a relay or first process that could not start the Command (the tool has the
/// reason from the start report).
const CANNOT_START: i32 = 127;

/// Whether `first`, the first argument, selects one of the two modes.
#[must_use]
pub fn selected(first: Option<&OsStr>) -> bool {
    first.is_some_and(|a| a == RELAY || a == INIT)
}

/// The arguments the relay is started with for `program` and `args`, from a tool whose process
/// group is `group`; `userns` when the Command is in a user namespace of its own.
#[must_use]
pub fn argv(group: Pid, userns: bool, program: &Path, args: &[OsString]) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![RELAY.into(), "--group".into()];
    argv.push(group.as_raw_nonzero().to_string().into());
    if userns {
        argv.push("--userns".into());
    }
    argv.push("--".into());
    argv.push(program.into());
    argv.extend(args.iter().cloned());
    argv
}

/// What `main` calls when [`selected`]: runs the mode `args` (the process's arguments without
/// the program name) name, its start report on standard input, and returns its exit code.
#[must_use]
pub fn main_with(args: &[OsString]) -> ExitCode {
    // Read while the process is still dumpable: a non-dumpable one may not follow its own
    // `/proc/self/exe`.
    let this = std::env::current_exe().unwrap_or_default();
    let stdin = io::stdin();
    let code = run_mode(&stdin.as_fd(), &this, args);
    ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX))
}

/// The parsed arguments of either mode.
#[derive(Debug, PartialEq, Eq)]
pub struct Mode<'a> {
    pub relay: bool,
    pub group: Option<Pid>,
    pub userns: bool,
    pub command: &'a [OsString],
}

/// Parses `args`: the mode, `--group <pgid>` (the relay only), `--userns`, `--`, the Command.
///
/// # Errors
/// A text naming what is wrong.
pub fn parse(args: &[OsString]) -> Result<Mode<'_>, String> {
    let (first, mut rest) = args.split_first().ok_or("no mode")?;
    let relay = first == RELAY;
    let mut mode = Mode {
        relay,
        group: None,
        userns: false,
        command: &[],
    };
    loop {
        match rest {
            [flag, value, more @ ..] if flag == "--group" => {
                let raw: i32 = value
                    .to_str()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--group takes a process group id")?;
                mode.group = Some(Pid::from_raw(raw).ok_or("--group takes a process group id")?);
                rest = more;
            }
            [flag, more @ ..] if flag == "--userns" => {
                mode.userns = true;
                rest = more;
            }
            [dash, command @ ..] if dash == "--" && !command.is_empty() => {
                mode.command = command;
                return Ok(mode);
            }
            _ => return Err("expected [--group <pgid>] [--userns] -- <program> [args]".into()),
        }
    }
}

/// Runs the mode `args` name: [`relay`] or [`init`], starting `init_program` as the first
/// process. A wrong argument is written to standard error and is exit code 2.
pub fn run_mode(report: &impl AsFd, init_program: &Path, args: &[OsString]) -> i32 {
    match parse(args) {
        Ok(mode) if mode.relay => relay(report, init_program, &mode),
        Ok(mode) => init(report, &mode),
        Err(error) => {
            eprintln!("kbf-task: {error}");
            2
        }
    }
}

/// The relay: see the module documentation.
pub fn relay(report: &impl AsFd, init_program: &Path, mode: &Mode<'_>) -> i32 {
    let _ = crate::child::set_dumpable(false);
    let [program, args @ ..] = mode.command else {
        return CANNOT_START;
    };
    if !pid_namespace_pending() {
        // `exec` returns only when it failed.
        let error = Command::new(program).args(args).stdin(Stdio::null()).exec();
        return not_started(report, &error);
    }
    let mut first = Command::new(init_program);
    first.arg(INIT);
    if mode.userns {
        first.arg("--userns");
    }
    let started = first.arg("--").args(mode.command).spawn();
    let mut first = match started {
        Ok(first) => first,
        Err(error) => return not_started(report, &error),
    };
    // A failure leaves it in the Command's group, where the deadline's kill ends it too, before
    // it reaps the rest; nothing else changes.
    let _ = rustix::process::setpgid(None, mode.group);
    first.wait().map_or(128 + libc::SIGKILL, status_code)
}

/// The namespace's first process: see the module documentation.
pub fn init(report: &impl AsFd, mode: &Mode<'_>) -> i32 {
    match start(mode) {
        Ok(command) => {
            tell(report, STARTED, 0);
            wait_until(Pid::from_child(&command), true)
        }
        Err(error) => not_started(report, &error),
    }
}

/// The first process's setup, then the Command started with its input closed.
fn start(mode: &Mode<'_>) -> io::Result<std::process::Child> {
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
    crate::child::set_dumpable(false)?;
    if mode.userns {
        drop_capabilities()?;
    }
    let [program, args @ ..] = mode.command else {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    };
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .spawn()
}

/// Writes the start report `[what, errno]`. A failed write leaves the tool with no report, which
/// it reads as a start.
fn tell(report: &impl AsFd, what: u8, errno: u8) {
    let _ = rustix::io::write(report, &[what, errno]);
}

/// Reports that the Command could not be started, for `error`, and returns the exit code that
/// says so.
fn not_started(report: &impl AsFd, error: &io::Error) -> i32 {
    tell(report, NOT_STARTED, crate::child::code(error));
    CANNOT_START
}

/// Whether the children of this process are made in a PID namespace other than its own: the
/// `unshare` of [`crate::child`] made one. Until that namespace has its first process the kernel
/// does not show it (`pid_for_children` cannot be read), which is the case here.
#[must_use]
pub fn pid_namespace_pending() -> bool {
    pending(
        std::fs::read_link("/proc/self/ns/pid").ok(),
        std::fs::read_link("/proc/self/ns/pid_for_children").ok(),
    )
}

/// [`pid_namespace_pending`] from what the two links read: a namespace for the children that is
/// not the process's own, or one not shown yet.
fn pending(own: Option<std::path::PathBuf>, children: Option<std::path::PathBuf>) -> bool {
    match (own, children) {
        (Some(own), Some(children)) => own != children,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Sets `no_new_privs` and empties the effective, permitted and inheritable capabilities of this
/// process. A program it starts then gains none, a file capability included: under
/// `no_new_privs` the new permitted set is at most the old one, which is empty.
///
/// # Errors
/// The `prctl` or the `capset` failed.
pub fn drop_capabilities() -> io::Result<()> {
    rustix::thread::set_no_new_privs(true)?;
    let none = rustix::thread::CapabilitySet::empty();
    rustix::thread::set_capabilities(
        None,
        rustix::thread::CapabilitySets {
            effective: none,
            permitted: none,
            inheritable: none,
        },
    )?;
    Ok(())
}

/// Waits for the child `target` and returns the exit code it is passed up as ([`exit_code`]).
/// With `any`, every child is waited for until `target` is, so a process the namespace handed to
/// its first process is reaped. A wait that fails for a reason other than a signal (it cannot
/// while `target` is this process's child) is reported as a kill by `SIGKILL`.
#[must_use]
pub fn wait_until(target: Pid, any: bool) -> i32 {
    let which = if any { None } else { Some(target) };
    loop {
        match rustix::process::waitpid(which, WaitOptions::empty()) {
            Ok(Some((pid, status))) if pid == target => return exit_code(status),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return 128 + libc::SIGKILL,
        }
    }
}

/// The exit code a wait status is passed up as: the code itself, or 128 plus the signal that
/// ended the process.
#[must_use]
pub fn exit_code(status: WaitStatus) -> i32 {
    status
        .exit_status()
        .unwrap_or_else(|| 128 + status.terminating_signal().unwrap_or(0))
}

/// [`exit_code`] for a status `std` reaped.
fn status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
