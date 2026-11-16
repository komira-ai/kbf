//! A job's Command runs while this process is its subreaper ([`crate::run::Job::Subreaper`]): a
//! process whose parent ends is handed to this one, and stays a zombie until somebody waits for
//! it. Left to the end of the run, a process the Command killed (and a test that asks whether a
//! killed process group is still alive) sees a group that is never empty. So, as an init does,
//! this reaps every child that is not the Command as its processes end.
//!
//! The Command is not touched: each wait names a pid (never `waitpid(-1)`), so its exit status is
//! still the one the run collects, as it always was, and the deadline's 137 with it.

use rustix::process::{Pid, WaitOptions, waitpid};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::task::JoinHandle;

use crate::run::own_children;

/// Starts listening for `SIGCHLD`, before the Command is started so that none is missed.
///
/// # Errors
/// The handler could not be installed.
pub fn listen() -> std::io::Result<Signal> {
    signal(SignalKind::child())
}

/// Reaps, until the returned task is aborted, every child that ends other than `command`.
pub fn reap_others(mut chld: Signal, command: Option<u32>) -> JoinHandle<()> {
    tokio::spawn(async move {
        // A `SIGCHLD` that came while this ran is remembered: the next `recv` returns at once.
        // (`None` is a runtime that has shut down, where this task is dropped anyway.)
        loop {
            chld.recv().await;
            reap_ended(command);
        }
    })
}

/// Waits, without blocking, for each child of this process but `command`: one that has ended is
/// reaped, one that is running is left (the call returns at once).
fn reap_ended(command: Option<u32>) {
    for pid in own_children() {
        if Some(pid_number(pid)) != command {
            // Gone already, or running: nothing to do either way.
            let _ = waitpid(Some(pid), WaitOptions::NOHANG);
        }
    }
}

fn pid_number(pid: Pid) -> u32 {
    pid.as_raw_nonzero().get().cast_unsigned()
}
