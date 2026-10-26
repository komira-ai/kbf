//! Everything the probes need from the host, behind one trait, so the logic that decides
//! a verdict runs in unit tests on any OS against a fake. The real implementation is
//! `real::RealSystem` (macOS only).

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::bundle::Sizing;
use crate::spec::{Event, VmSpec};

/// What waiting for a VM helper's next event found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Event(Event),
    /// Nothing arrived within the wait.
    Quiet,
    /// The helper is gone: its exit code, or the signal that ended it.
    Gone {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// The time the probes read and pass. The real one is the monotonic clock and a thread
/// sleep; the fake one is a counter that only moves when something passes time, so a test
/// of a wait gets the same answer on any host at any load.
pub trait Clock: Send + Sync {
    /// Time since an origin fixed when the clock was made; never goes back.
    fn now(&self) -> Duration;
    /// Lets `d` pass: the real clock sleeps, the fake one advances.
    fn pause(&self, d: Duration);
}

/// The monotonic clock.
#[derive(Debug)]
pub struct RealClock(Instant);

impl RealClock {
    #[must_use]
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A running VM helper process.
pub trait VmProcess {
    fn pid(&self) -> u32;
    /// The next event, waiting up to `wait`.
    fn poll(&mut self, wait: Duration) -> Poll;
    /// SIGKILL: the helper gets no chance to stop its VM.
    fn kill9(&mut self);
    /// Asks the guest to shut down (the helper keeps running until the guest has stopped,
    /// then exits). Without a guest that obeys, [`request_stop`](Self::request_stop) is
    /// still needed.
    fn request_shutdown(&mut self);
    /// Closes the helper's stdin, which makes it force-stop its VM (the guest is not asked
    /// to shut down) and exit.
    fn request_stop(&mut self);
    /// Waits for the helper to exit (after [`request_stop`](Self::request_stop) or
    /// [`kill9`](Self::kill9)), up to `wait`; true once it has.
    fn wait_gone(&mut self, wait: Duration) -> bool;
    /// The tail of what the helper wrote to its log, for a failure report; `None` when
    /// there is no log.
    fn diagnostics(&self) -> Option<String>;
}

/// One file written into a guest volume while the guest is off (probe P0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineFile {
    /// Path inside the guest's Data volume, without a leading slash.
    pub path: String,
    pub contents: Vec<u8>,
    pub mode: u32,
}

/// The facts of a binary's code signature that matter to the virtualization entitlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub adhoc: bool,
    pub hardened_runtime: bool,
    pub virtualization_entitlement: bool,
    /// The raw `codesign` text, kept in the report.
    pub raw: String,
}

pub trait System {
    /// The clock the probes wait by. Every call may return a new clock with its own origin:
    /// only differences between readings of one clock mean anything.
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(RealClock::new())
    }
    /// `clonefile(2)`: a copy-on-write copy of the file `src` at `dst`.
    ///
    /// # Errors
    /// The call failed (`EXDEV` across volumes, `ENOTSUP` off APFS, `EEXIST`).
    fn clonefile(&self, src: &Path, dst: &Path) -> io::Result<()>;
    /// Clears the user-immutable flag (`uchg`) on `path`, which a clone of a sealed base
    /// keeps; a path without the flag is left alone. The caller owns `path`.
    ///
    /// # Errors
    /// The file could not be examined or the flag could not be cleared.
    fn clear_immutable(&self, path: &Path) -> io::Result<()>;
    /// The data representation of a new `VZMacMachineIdentifier`.
    ///
    /// # Errors
    /// The framework refused.
    fn new_machine_identifier(&self) -> Result<Vec<u8>, String>;
    /// Starts a VM helper for `spec`; as the user `run_as` when given.
    ///
    /// # Errors
    /// The helper could not be spawned.
    fn start_vm(&self, spec: &VmSpec, run_as: Option<u32>) -> Result<Box<dyn VmProcess>, String>;
    /// Empties the filesystem page cache (needs root).
    ///
    /// # Errors
    /// Not root, or `purge` failed.
    fn purge_cache(&self) -> Result<(), String>;
    fn uid(&self) -> u32;
    fn ppid(&self) -> u32;
    /// The launchd domain this process runs in (`launchctl managername`: `System` for a
    /// launch daemon, `Aqua` or `Background` for a login session); `None` when unknown.
    fn launchd_context(&self) -> Option<String>;
    /// The owner of `/dev/console`: 0 when nobody is logged in at the console.
    fn console_uid(&self) -> Option<u32>;
    /// macOS version and build, architecture and this binary's version, as JSON.
    fn host_facts(&self) -> Value;
    /// The signature of the running binary.
    ///
    /// # Errors
    /// `codesign` could not be run or its output not read.
    fn signature(&self) -> Result<Signature, String>;
    /// Attaches the raw disk image `disk`, writes `files` into its Data volume as root,
    /// detaches, and returns the observations (steps run, owner and mode read back).
    ///
    /// # Errors
    /// A step failed; the message names it.
    fn offline_write(&self, disk: &Path, files: &[OfflineFile]) -> Result<Value, String>;
    /// Attaches the raw disk image `disk`, stages Xcode and the simulator runtimes of
    /// `plan` into its Data volume as root (`stage::execute`), detaches, and returns what
    /// was staged.
    ///
    /// # Errors
    /// A step failed or the volume is too small; the message names it.
    fn offline_stage(&self, disk: &Path, plan: &crate::stage::Plan) -> Result<Value, String>;
    /// Attaches the raw disk image `disk` read-only, copies the harvest allowlist out of
    /// its Data volume into the new directory `out`, detaches, and returns the manifest.
    ///
    /// # Errors
    /// A step failed; the message names it.
    fn offline_harvest(&self, disk: &Path, out: &Path) -> Result<Value, String>;
    /// The process table as text, one `pid cputime command` row per line (`ps`).
    ///
    /// # Errors
    /// The table could not be read.
    fn process_table(&self) -> Result<String, String>;
    /// Downloads `url` to `dest`.
    ///
    /// # Errors
    /// The download failed.
    fn fetch(&self, url: &str, dest: &Path) -> Result<(), String>;
    /// Installs macOS from the restore image `ipsw` into a new bundle at `out`, sized as
    /// `sizing` says.
    ///
    /// # Errors
    /// The restore image is unusable for this host or the install failed.
    fn install_image(&self, ipsw: &Path, out: &Path, sizing: &Sizing) -> Result<Value, String>;
    /// The helper mode (`vm-run`): runs the VM `spec` describes until stdin closes, printing
    /// [`Event`]s as JSON lines on stdout.
    ///
    /// # Errors
    /// The VM could not be configured; start failures are events, not errors.
    fn run_vm_helper(&self, spec: &VmSpec) -> Result<(), String>;
}
