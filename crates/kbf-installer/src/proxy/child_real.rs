//! The [`Child`] port on real processes (Linux): the frontend children the proxy starts,
//! watches through a pidfd, adopts again after a restart of its own, and stops with SIGTERM.
//!
//! - **Spawn** runs `releases/<commit>/kbf` (the file of that release, never `current`,
//!   which moves at a confirm) in its own process group, with the activation variables
//!   removed. The child outlives the proxy: nothing here kills on drop, and the unit keeps
//!   `KillMode=process` so a restart of the proxy leaves it running. The identity (pid,
//!   start time, executable) is written to `<dir>/<slot>.json` before `spawn` returns, so a
//!   candidate that was started and not yet recorded in `update.json` can still be found.
//! - **Identity.** Every signal goes through a pidfd opened while the pid was verified to be
//!   the recorded process: a pid that was recycled is never signalled, and an adoption
//!   compares pid, the start time the kernel reports and the executable
//!   ([`verify_ident`]), then compares again after it opened the pidfd.
//! - **Liveness.** A child this process started is polled with `try_wait`, which also reaps
//!   it. An adopted one is not our child, so its exit is the pidfd turning readable, and its
//!   exit code is unknown ([`UNKNOWN_EXIT`]).
//! - **Signals.** `terminate` is SIGTERM, with no timeout behind it. `kill_unserved` is
//!   SIGKILL and exists only for a candidate that never served; it reaps on a thread.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::PathBuf;
use std::process::{Child as Process, Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use serde::{Deserialize, Serialize};

use crate::CommitId;
use crate::layout::{Layout, RELEASE_BINARY};

use super::model::{AdoptError, PortError, ProcIdent, Slot, verify_ident};
use super::ports::Child;

/// The exit code reported for a process that is gone but whose status this process cannot
/// read (it was adopted, or it was not ours).
pub const UNKNOWN_EXIT: i32 = -1;

/// Variables of the service manager's activation protocol: the child is not the proxy's
/// socket-activated service and must not read them.
const ACTIVATION: [&str; 3] = ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"];

/// What `<dir>/<slot>.json` holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recorded {
    pub ident: ProcIdent,
    pub commit: CommitId,
}

/// The argument vector of a child in `slot`, after the program.
pub type Argv = Box<dyn Fn(Slot) -> Vec<OsString> + Send + Sync>;

struct Held {
    slot: Slot,
    ident: ProcIdent,
    pidfd: OwnedFd,
    /// Our own child; `None` for an adopted process.
    process: Option<Process>,
}

/// Real children of one host.
pub struct ProcChild {
    pub(super) layout: Layout,
    dir: PathBuf,
    argv: Argv,
    binary: &'static str,
    held: Mutex<BTreeMap<u32, Held>>,
}

impl ProcChild {
    /// Children of the releases under `layout`, recorded in `dir`, started with `argv`.
    pub fn new(layout: Layout, dir: PathBuf, argv: Argv) -> ProcChild {
        ProcChild {
            layout,
            dir,
            argv,
            binary: RELEASE_BINARY,
            held: Mutex::new(BTreeMap::new()),
        }
    }

    /// The same, running another file of a release (tests run a stand-in program).
    #[cfg(test)]
    pub(super) fn with_binary(mut self, binary: &'static str) -> ProcChild {
        self.binary = binary;
        self
    }

    /// The program of `release`.
    fn program(&self, release: &CommitId) -> PathBuf {
        let dir = self.layout.release_dir(release);
        self.layout.join(&dir).join(self.binary)
    }

    fn record_path(&self, slot: Slot) -> PathBuf {
        self.dir.join(format!("{}.json", slot_name(slot)))
    }

    /// What `spawn` recorded for `slot` and no `exited` or `kill_unserved` has cleared: a
    /// process that may be live. The caller checks it with [`Child::adopt`].
    pub fn recorded(&self, slot: Slot) -> Option<Recorded> {
        let bytes = fs::read(self.record_path(slot)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_record(&self, slot: Slot, record: &Recorded) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.record_path(slot);
        let temp = path.with_extension("json.new");
        let mut file = fs::File::create(&temp)?;
        file.write_all(&serde_json::to_vec(record).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        fs::File::open(&self.dir)?.sync_all()
    }

    fn clear_record(&self, slot: Slot, pid: u32) {
        if self.recorded(slot).is_some_and(|r| r.ident.pid == pid) {
            let _ = fs::remove_file(self.record_path(slot));
        }
    }

    /// The entry for exactly this process, dropped from the table when it is gone.
    fn lookup<R>(&self, ident: &ProcIdent, f: impl FnOnce(&mut Held) -> R) -> Option<R> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = held.get_mut(&ident.pid)?;
        (entry.ident == *ident).then(|| f(entry))
    }
}

/// How a child is started: nothing on its standard input, its own process group (a signal to
/// the proxy's group is not a signal to a child that must outlive it) and none of the
/// service manager's activation variables.
fn detach(command: &mut Command) {
    command.stdin(Stdio::null()).process_group(0);
    for name in ACTIVATION.into_iter().chain(["NOTIFY_SOCKET"]) {
        command.env_remove(name);
    }
}

fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::A => "a",
        Slot::B => "b",
    }
}

/// The identity of the live process `pid`, from `/proc`.
pub fn ident_of(pid: u32) -> std::io::Result<ProcIdent> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let start_time = start_time_of(&stat)
        .ok_or_else(|| std::io::Error::other(format!("unreadable /proc/{pid}/stat")))?;
    let exe = fs::read_link(format!("/proc/{pid}/exe"))?;
    Ok(ProcIdent {
        pid,
        start_time,
        exe,
    })
}

/// Field 22 of `/proc/<pid>/stat`, the start time in clock ticks since boot. The command
/// name (field 2) may hold spaces and parentheses, so the fields are counted from the last
/// closing parenthesis.
fn start_time_of(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

fn pidfd_of(pid: u32) -> Result<OwnedFd, Errno> {
    let raw = i32::try_from(pid).map_err(|_| Errno::INVAL)?;
    let pid = Pid::from_raw(raw).ok_or(Errno::INVAL)?;
    pidfd_open(pid, PidfdFlags::empty())
}

/// Whether the process behind `pidfd` is gone (the descriptor is readable once it exits).
fn gone(pidfd: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    matches!(poll(&mut fds, Some(&now)), Ok(n) if n > 0)
}

/// A pidfd for the process `recorded` names, after checking that it is that process. The pid
/// could be recycled between the check and the open, and the pidfd names whatever held the
/// pid when it opened; so the identity is read again afterwards, and only a pid that has the
/// recorded identity both before and after is certain not to have been recycled in between.
fn open_verified(recorded: &ProcIdent) -> Result<OwnedFd, AdoptError> {
    verified_open(recorded, ident_of, pidfd_of)
}

/// [`open_verified`] over the two reads it makes, so a test can recycle the pid between them.
fn verified_open(
    recorded: &ProcIdent,
    identity: impl Fn(u32) -> std::io::Result<ProcIdent>,
    open: impl FnOnce(u32) -> Result<OwnedFd, Errno>,
) -> Result<OwnedFd, AdoptError> {
    let pid = recorded.pid;
    let identity = || identity(pid).map_err(|_| AdoptError::Gone(pid));
    verify_ident(recorded, &identity()?)?;
    let pidfd = open(pid).map_err(|_| AdoptError::Gone(pid))?;
    verify_ident(recorded, &identity()?)?;
    Ok(pidfd)
}

fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(UNKNOWN_EXIT)
}

impl Child for ProcChild {
    fn spawn(&self, slot: Slot, release: &CommitId) -> Result<ProcIdent, PortError> {
        {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            held.retain(|_, h| {
                h.slot != slot
                    || match &mut h.process {
                        Some(p) => matches!(p.try_wait(), Ok(None)),
                        None => !gone(&h.pidfd),
                    }
            });
            if held.values().any(|h| h.slot == slot) {
                return Err(PortError(format!("slot {slot:?} has a live child already")));
            }
        }
        let program = self.program(release);
        let mut command = Command::new(&program);
        command.args((self.argv)(slot));
        detach(&mut command);
        let process = command
            .spawn()
            .map_err(|e| PortError(format!("start {}: {e}", program.display())))?;
        let pid = process.id();
        // The process is not reaped yet, so the pid is ours until `try_wait` says otherwise.
        let started = pidfd_of(pid).map_err(|e| e.to_string()).and_then(|pidfd| {
            ident_of(pid)
                .map(|ident| (pidfd, ident))
                .map_err(|e| e.to_string())
        });
        let (pidfd, ident) = match started {
            Ok(v) => v,
            Err(e) => {
                let mut process = process;
                let _ = process.kill();
                reap(process);
                return Err(PortError(format!("identify the new child {pid}: {e}")));
            }
        };
        let record = Recorded {
            ident: ident.clone(),
            commit: release.clone(),
        };
        if let Err(e) = self.write_record(slot, &record) {
            // A child nobody can find again must not be left running.
            let _ = pidfd_send_signal(&pidfd, Signal::KILL);
            reap(process);
            return Err(PortError(format!("record the new child: {e}")));
        }
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                pid,
                Held {
                    slot,
                    ident: ident.clone(),
                    pidfd,
                    process: Some(process),
                },
            );
        Ok(ident)
    }

    fn adopt(&self, slot: Slot, recorded: &ProcIdent) -> Result<(), AdoptError> {
        let pidfd = open_verified(recorded)?;
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                recorded.pid,
                Held {
                    slot,
                    ident: recorded.clone(),
                    pidfd,
                    process: None,
                },
            );
        Ok(())
    }

    fn exited(&self, ident: &ProcIdent) -> Option<i32> {
        let verdict = self.lookup(ident, |h| match &mut h.process {
            Some(p) => match p.try_wait() {
                Ok(Some(status)) => Some(code_of(status)),
                Ok(None) | Err(_) => None,
            },
            None => gone(&h.pidfd).then_some(UNKNOWN_EXIT),
        });
        match verdict {
            // Not a process this port holds (never spawned or adopted, or the pid now
            // belongs to another): it is not ours, so for the swap it is gone and untouched.
            None => Some(UNKNOWN_EXIT),
            Some(None) => None,
            Some(Some(code)) => {
                let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(h) = held.remove(&ident.pid) {
                    drop(held);
                    self.clear_record(h.slot, ident.pid);
                }
                Some(code)
            }
        }
    }

    fn terminate(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.signal(ident, Signal::TERM)
    }

    fn kill_unserved(&self, ident: &ProcIdent) -> Result<(), PortError> {
        self.signal(ident, Signal::KILL)?;
        let entry = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&ident.pid);
        if let Some(h) = entry {
            self.clear_record(h.slot, ident.pid);
            if let Some(process) = h.process {
                reap(process);
            }
        }
        Ok(())
    }
}

impl ProcChild {
    /// `signal` to exactly the process `ident` names, through its pidfd. A process this port
    /// does not hold is refused; one that is already gone is not an error.
    fn signal(&self, ident: &ProcIdent, signal: Signal) -> Result<(), PortError> {
        let sent = self.lookup(ident, |h| pidfd_send_signal(&h.pidfd, signal));
        match sent {
            None => Err(PortError(format!(
                "process {} is not held by this proxy",
                ident.pid
            ))),
            Some(Ok(())) | Some(Err(Errno::SRCH)) => Ok(()),
            Some(Err(e)) => Err(PortError(format!("signal {}: {e}", ident.pid))),
        }
    }
}

/// Wait for a process that was sent SIGKILL on a thread of its own, so no zombie is left
/// and the caller does not block.
fn reap(mut process: Process) {
    std::thread::spawn(move || {
        let _ = process.wait();
    });
}

#[cfg(test)]
#[path = "child_real_tests.rs"]
mod tests;
