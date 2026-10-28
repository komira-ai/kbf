//! [`MacVmHost`]: the macOS [`VmHost`], which runs each VM as `kbf-vmm vm-run` started by
//! the root helper as the standing VM user (`docs/design/macos-vms.md` section 6,
//! `docs/design/macos-vm-guests.md` section 6).
//!
//! Everything but the real calls is plain Rust over a [`VmSession`], so these rules are
//! tested on every platform against a fake session; the real session
//! (`client`, macOS only) is the helper's socket.
//!
//! One `start`, in order:
//! 1. **A slot.** One of two lock files ([`slots`]) is taken before anything else. Both
//!    held means the Mac already runs its two VMs: [`HostError::LimitExceeded`], with no
//!    call to the helper and none to the framework.
//! 2. **The spec.** The driver's [`VmSpec`] becomes `kbf-vmm`'s spec JSON in the clone's
//!    directory ([`vmspec`]).
//! 3. **The session.** The helper must say the VM user's GUI session is up.
//! 4. **A record.** A pending run record is written ([`records`]) before the helper starts
//!    anything.
//! 5. **The run.** `role-run` of `<vmm> vm-run --spec <file>`, with stdin the read end of
//!    the control pipe, stdout the write end of the event pipe, stderr a log file in the
//!    clone and the clone as the working directory ([`pipes`]). The daemon closes its
//!    copies of the helper's ends at once. As soon as the pid is known (it leads the
//!    run's process group) the complete record, `{pgid, start, boot}`, replaces the
//!    pending one.
//! 6. **Two threads.** One reads the event pipe into the run's progress; the other waits
//!    for the helper to report the run's exit, then removes the record, gives the slot
//!    back and only then reports the run as ended. So a slot is free exactly when its
//!    run has exited, and every error path before step 5 gives the slot back as it
//!    returns.
//!
//! `stop` closes the control pipe (`vm-run` force-stops its VM at end of file), waits
//! [`MacConfig::stop_grace`] for the run to end, then has the helper end the group
//! (`role-reap`) and waits [`MacConfig::reap_grace`]. A daemon that dies closes the
//! control pipe with its descriptors: the same stop, with nobody left to wait.
//!
//! At [`MacVmHost::new`] the daemon takes the state directory ([`owner`]: a second live
//! daemon on it is refused) and sweeps the records of earlier daemons ([`records::sweep`]).
//!
//! Not yet: the relay to the guest agent. `kbf-vmm vm-run` serves only its events on
//! stdout and a stop on stdin, so the real session's `connect` answers "unsupported"; the
//! host's own part (a stream only for a VM that has started and not failed) is done.
//! And the machine identifier: `vm-run` takes none, so two clones of one image share the
//! identifier the image has until `vm-run` makes a fresh one.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_driver_native::Record;
use kbf_vmm::spec::{Event, LIMIT_EXCEEDED};

use crate::fake::MACOS_VM_LIMIT;
use crate::host::{HostError, VmHost, VmSpec};

#[cfg(target_os = "macos")]
pub mod client;
pub mod owner;
pub mod pipes;
pub mod records;
pub mod session;
pub mod slots;
pub mod vmspec;

#[cfg(test)]
mod tests;

pub use records::Swept;
pub use session::{RunExit, RunFds, VmSession};

/// The longest event line the host reads whole; a longer one is skipped.
const MAX_LINE: u64 = 64 * 1024;

/// The file in the clone's directory that takes `vm-run`'s stderr.
pub const LOG_FILE: &str = "vm-run.log";

/// What the macOS host is configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacConfig {
    /// The `kbf-vmm` the helper runs: the path the helper was given (`--vmm`), which is
    /// the only program `role-run` starts.
    pub vmm: PathBuf,
    /// Where the host keeps its run records (`runs/`) and slot locks (`slots/`).
    pub state: PathBuf,
    /// How long `stop` waits for a run to end after closing its control pipe, before it
    /// has the helper end the group.
    pub stop_grace: Duration,
    /// How long `stop` waits after the helper ended the group.
    pub reap_grace: Duration,
}

impl MacConfig {
    /// The defaults: ten seconds for each wait.
    #[must_use]
    pub fn new(vmm: PathBuf, state: PathBuf) -> Self {
        Self {
            vmm,
            state,
            stop_grace: Duration::from_secs(10),
            reap_grace: Duration::from_secs(10),
        }
    }

    fn runs(&self) -> PathBuf {
        self.state.join("runs")
    }

    fn slots(&self) -> PathBuf {
        self.state.join("slots")
    }
}

/// An error the framework or `vm-run` reported.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failure {
    code: Option<String>,
    message: String,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ended {
    Exit(RunExit),
    /// The helper's connection failed before it reported the end.
    Lost(String),
}

/// What the events and the exit said about one run.
#[derive(Debug, Default)]
struct Progress {
    started: bool,
    failure: Option<Failure>,
    ended: Option<Ended>,
}

/// One run's [`Progress`], shared by its reader, its monitor and the host.
#[derive(Debug, Default)]
struct RunState {
    progress: Mutex<Progress>,
    changed: Condvar,
}

impl RunState {
    fn lock(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn apply(&self, event: Event) {
        let mut progress = self.lock();
        match event {
            Event::Started { .. } => progress.started = true,
            Event::Error { message, code, .. } => {
                progress.failure = Some(Failure { code, message })
            }
            _ => return,
        }
        drop(progress);
        self.changed.notify_all();
    }

    fn end(&self, how: Ended) {
        self.lock().ended = Some(how);
        self.changed.notify_all();
    }

    /// Waits at most `grace` for the run to end; whether it has.
    fn wait_ended(&self, grace: Duration) -> bool {
        let guard = self.lock();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, grace, |progress| progress.ended.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        guard.ended.is_some()
    }
}

/// A VM the host has started and not stopped.
#[derive(Debug)]
struct Run {
    /// The control pipe's write end; `None` once closed.
    control: Option<OwnedFd>,
    state: Arc<RunState>,
    record: Record,
}

/// The macOS [`VmHost`]; see the module documentation.
#[derive(Debug)]
pub struct MacVmHost<S: VmSession> {
    session: Arc<S>,
    config: MacConfig,
    runs: Mutex<BTreeMap<String, Run>>,
    swept: Swept,
    /// Held for the host's life: no other daemon sweeps or runs on this state directory.
    _owner: owner::Owner,
    /// The monitor and reader threads, for a test to wait for.
    #[cfg(test)]
    monitors: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl<S: VmSession> MacVmHost<S> {
    /// A host over `session`, after ending the runs earlier daemons left.
    ///
    /// A run an earlier daemon left that the helper refuses to end does not stop the
    /// daemon: it stays in [`Swept::failed`] with its record, is tried again at the next
    /// start, and the framework's own limit covers the slot it may still hold.
    ///
    /// # Errors
    /// `WouldBlock` when another live daemon owns the state directory (its records are
    /// not ours to sweep); the records cannot be read, or this boot or the process table
    /// cannot be read.
    pub fn new(config: MacConfig, session: S) -> io::Result<Self> {
        let owner = owner::Owner::take(&config.state)?;
        let swept = records::sweep(&config.runs(), &session)?;
        Ok(Self {
            _owner: owner,
            session: Arc::new(session),
            config,
            runs: Mutex::new(BTreeMap::new()),
            swept,
            #[cfg(test)]
            monitors: Mutex::new(Vec::new()),
        })
    }

    /// What the start-up sweep did.
    #[must_use]
    pub fn swept(&self) -> &Swept {
        &self.swept
    }

    fn runs(&self) -> MutexGuard<'_, BTreeMap<String, Run>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A name that is safe as a file name here: ASCII letters, digits, `-` and `_`.
fn check_name(name: &str) -> io::Result<()> {
    let good = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if good {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a VM name (ASCII letters, digits, - and _)"),
        ))
    }
}

fn text(path: &Path) -> io::Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not UTF-8", path.display()),
        )
    })
}

/// Reads `vm-run`'s event lines into `state` until the pipe ends.
fn read_events(events: File, state: &RunState) {
    let mut reader = BufReader::new(events);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader).take(MAX_LINE).read_until(b'\n', &mut line);
        match read {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            // Cut at the bound or at the end of the pipe: drop the rest of the line.
            if !skip_line(&mut reader) {
                return;
            }
            continue;
        }
        match std::str::from_utf8(&line)
            .map_err(|why| why.to_string())
            .and_then(|text| Event::parse(text.trim_end()))
        {
            Ok(event) => state.apply(event),
            Err(why) => tracing::debug!("vm-run printed {why}"),
        }
    }
}

/// Discards input up to and including the next newline; whether one was found.
fn skip_line<R: io::BufRead>(reader: &mut R) -> bool {
    loop {
        let (used, found) = match reader.fill_buf() {
            Ok([]) | Err(_) => return false,
            Ok(buffer) => match buffer.iter().position(|&b| b == b'\n') {
                Some(at) => (at + 1, true),
                None => (buffer.len(), false),
            },
        };
        reader.consume(used);
        if found {
            return true;
        }
    }
}

impl<S: VmSession> VmHost for MacVmHost<S> {
    type Stream = S::Stream;

    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()> {
        self.session.clone_image(image, clone)
    }

    fn start(&self, spec: &VmSpec) -> Result<(), HostError> {
        check_name(&spec.name)?;
        if self.runs().contains_key(&spec.name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("the VM {} is running", spec.name),
            )
            .into());
        }
        // Before the spec, the session or the framework hear of this VM.
        let slot = slots::SlotLock::acquire(&self.config.slots(), MACOS_VM_LIMIT)?
            .ok_or(HostError::LimitExceeded(MACOS_VM_LIMIT))?;
        let spec_file = vmspec::write(spec)?;
        if !self.session.session_ready()? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the VM user's GUI session is not up",
            )
            .into());
        }
        let runs = self.config.runs();
        let boot = self.session.boot_id()?;
        records::begin(&runs, &spec.name, &boot)?;
        let trace = Trace {
            dir: runs.clone(),
            name: spec.name.clone(),
            keep: false,
        };
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(spec.clone.join(LOG_FILE))?;
        let cwd = File::open(&spec.clone)?;
        let pipes = pipes::Pipes::new()?;
        let argv = [
            text(&self.config.vmm)?,
            "vm-run".to_owned(),
            "--spec".to_owned(),
            text(&spec_file)?,
        ];
        let running = self.session.run(
            &argv,
            RunFds {
                stdin: std::os::fd::AsFd::as_fd(&pipes.helper.stdin),
                stdout: std::os::fd::AsFd::as_fd(&pipes.helper.stdout),
                stderr: std::os::fd::AsFd::as_fd(&log),
                cwd: std::os::fd::AsFd::as_fd(&cwd),
            },
        )?;
        let pipes::Pipes {
            control,
            events,
            helper,
        } = pipes;
        // The helper has its own copies. The daemon's must go now: the control pipe's
        // write end is then the only one, and the event pipe ends when the run does.
        drop(helper);
        drop((log, cwd));
        let pid = self.session.pid(&running);
        let state = Arc::new(RunState::default());
        let reader = Arc::clone(&state);
        let reading = std::thread::spawn(move || read_events(events, &reader));
        #[cfg(test)]
        self.monitors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(reading);
        #[cfg(not(test))]
        drop(reading);
        let leader = i32::try_from(pid).map_err(io::Error::other)?;
        let (record, written) = match self.session.process_start(leader) {
            Ok(Some(start)) => {
                let record = Record {
                    pgid: leader,
                    start,
                    boot,
                };
                let written = records::complete(&runs, &spec.name, &record);
                (Some(record), written)
            }
            // Gone already: the monitor reports how it ended.
            Ok(None) => (None, Ok(())),
            Err(why) => (None, Err(why)),
        };
        let monitor = Monitor {
            session: Arc::clone(&self.session),
            running,
            slot,
            trace,
            state: Arc::clone(&state),
        };
        let watching = std::thread::spawn(move || monitor.watch());
        #[cfg(test)]
        self.monitors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(watching);
        #[cfg(not(test))]
        drop(watching);
        // Failing from here closes the control pipe on the way out: the VM stops, and
        // the monitor gives the slot back when it has.
        written?;
        let record =
            record.ok_or_else(|| io::Error::other("the VM's process had ended as it started"))?;
        self.runs().insert(
            spec.name.clone(),
            Run {
                control: Some(control),
                state,
                record,
            },
        );
        Ok(())
    }

    fn connect(&self, name: &str) -> Result<Self::Stream, HostError> {
        let state = {
            let runs = self.runs();
            let run = runs
                .get(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no VM {name}")))?;
            Arc::clone(&run.state)
        };
        {
            let progress = state.lock();
            if let Some(failure) = &progress.failure {
                return Err(if failure.code.as_deref() == Some(LIMIT_EXCEEDED) {
                    HostError::LimitExceeded(MACOS_VM_LIMIT)
                } else {
                    io::Error::other(format!("the VM failed: {}", failure.message)).into()
                });
            }
            match &progress.ended {
                Some(Ended::Exit(exit)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("the VM has stopped ({exit:?})"),
                    )
                    .into());
                }
                Some(Ended::Lost(why)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        format!("the VM's run was lost: {why}"),
                    )
                    .into());
                }
                None if !progress.started => return Err(HostError::NotReady),
                None => {}
            }
        }
        Ok(self.session.connect(name)?)
    }

    fn stop(&self, name: &str) -> io::Result<()> {
        let (state, record) = {
            let mut runs = self.runs();
            let Some(run) = runs.get_mut(name) else {
                return Ok(());
            };
            // End of file on `vm-run`'s stdin: it force-stops the VM.
            run.control = None;
            (Arc::clone(&run.state), run.record.clone())
        };
        if !state.wait_ended(self.config.stop_grace) {
            tracing::warn!(
                vm = name,
                "the run did not end with its control pipe; having the helper end it"
            );
            self.session.reap(&record)?;
            if !state.wait_ended(self.config.reap_grace) {
                return Err(io::Error::other(format!(
                    "the run of {name} has not ended after the helper ended its group"
                )));
            }
        }
        self.runs().remove(name);
        Ok(())
    }
}

/// The pending or complete record of a run, removed when this is dropped unless `keep`.
struct Trace {
    dir: PathBuf,
    name: String,
    /// Leave the record for the next start's sweep.
    keep: bool,
}

impl Drop for Trace {
    fn drop(&mut self) {
        if !self.keep {
            records::finish(&self.dir, &self.name);
        }
    }
}

/// What the thread that waits for a run owns.
struct Monitor<S: VmSession> {
    session: Arc<S>,
    running: S::Running,
    slot: slots::SlotLock,
    trace: Trace,
    state: Arc<RunState>,
}

impl<S: VmSession> Monitor<S> {
    /// Waits for the run to end, then removes its record, frees its slot and reports the
    /// end, in that order: whoever sees the end can start another VM.
    ///
    /// A connection to the helper that fails before the end is reported leaves the VM
    /// unaccounted for. The slot is freed all the same (holding it for ever would take a
    /// slot from the Mac for good, and the framework refuses a third VM anyway), and the
    /// record stays for the next start's sweep to end the run if it is still there.
    fn watch(self) {
        let Self {
            session,
            running,
            slot,
            trace,
            state,
        } = self;
        // The record and the slot are this block's: they are gone, in this order, before
        // the end is reported.
        let how = {
            let (_slot, mut trace) = (slot, trace);
            match session.wait(running) {
                Ok(exit) => Ended::Exit(exit),
                Err(why) => {
                    tracing::error!(vm = %trace.name, "lost the connection to the helper before the run's end: {why}");
                    trace.keep = true;
                    Ended::Lost(why.to_string())
                }
            }
        };
        state.end(how);
    }
}
