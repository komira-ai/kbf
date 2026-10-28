//! The macOS host over a fake session. Unit tests, not a `tests/` directory, so the
//! host's generic code is instantiated in this crate, where its coverage is measured.

mod fake;
mod host;
mod units;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_testkit::dir::TestDir;

use super::{MacConfig, MacVmHost, RunState};
use crate::host::{INPUTS_TAG, MachineId, OUTPUTS_TAG, Share, VmSpec};
pub use fake::FakeSession;

/// A wait the tests never reach: a stop that is meant to find the run ended waits on the
/// run, not on a clock.
pub const LONG: Duration = Duration::from_secs(3600);

/// The pid the `n`th run of a fake session gets.
pub fn pid(n: u32) -> i32 {
    i32::try_from(fake::FIRST_PID + n).unwrap()
}

pub fn upid(n: u32) -> u32 {
    fake::FIRST_PID + n
}

/// Registers a subscriber for this thread, so the log statements' arguments run (nextest
/// runs a process per test; a `tracing` macro evaluates its fields only with one).
pub fn trace() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .finish(),
    )
}

pub struct Rig {
    pub session: FakeSession,
    pub host: MacVmHost<FakeSession>,
    pub vmm: PathBuf,
    pub state: PathBuf,
    // Last: removed after the host and the threads that use it.
    pub dir: TestDir,
}

pub fn dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-mac-{name}"))
}

pub fn config(dir: &TestDir) -> MacConfig {
    let mut config = MacConfig::new(PathBuf::from("/opt/kbf/kbf-vmm"), dir.path().join("state"));
    config.stop_grace = LONG;
    config.reap_grace = LONG;
    config
}

pub fn rig(name: &str) -> Rig {
    rig_with(name, FakeSession::new(), |_| {})
}

pub fn rig_with(name: &str, session: FakeSession, tune: impl FnOnce(&mut MacConfig)) -> Rig {
    let dir = dir(name);
    let mut config = config(&dir);
    tune(&mut config);
    let (vmm, state) = (config.vmm.clone(), config.state.clone());
    session.watch_runs_dir(&config.runs());
    std::fs::create_dir_all(config.runs()).unwrap();
    let host = MacVmHost::new(config, session.clone()).unwrap();
    Rig {
        session,
        host,
        vmm,
        state,
        dir,
    }
}

impl Rig {
    /// A spec for the VM `name`: a clone directory with a file in it, and both shares.
    pub fn spec(&self, name: &str) -> VmSpec {
        let clone = self.dir.path().join("clones").join(name);
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("disk.img"), "disk").unwrap();
        let share = |tag: &'static str, read_only| {
            let host = self.dir.path().join("shares").join(name).join(tag);
            std::fs::create_dir_all(&host).unwrap();
            Share {
                tag,
                host,
                read_only,
            }
        };
        VmSpec {
            name: name.to_owned(),
            clone,
            machine_id: MachineId([7; 16]),
            cpus: 4,
            memory_bytes: 8 << 30,
            shares: vec![share(INPUTS_TAG, true), share(OUTPUTS_TAG, false)],
            network: Vec::new(),
        }
    }

    pub fn runs(&self) -> PathBuf {
        self.state.join("runs")
    }

    /// The names in the records directory.
    pub fn records(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.runs())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Waits for the monitor threads of every run that has ended or will end.
    pub fn settle(&self) {
        let handles = std::mem::take(&mut *self.host.monitors.lock().unwrap());
        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Blocks until the run `name` has ended.
    pub fn wait_ended(&self, name: &str) {
        assert!(self.state_of(name).wait_ended(LONG));
    }

    /// The progress state of the run `name`.
    pub fn state_of(&self, name: &str) -> Arc<RunState> {
        Arc::clone(&self.host.runs().get(name).expect("a running VM").state)
    }

    /// Whether both slots can be taken at once, which they can only if no run holds one.
    pub fn both_slots_free(&self) -> bool {
        let dir = self.state.join("slots");
        let first = super::slots::SlotLock::acquire(&dir, 2).unwrap();
        let second = super::slots::SlotLock::acquire(&dir, 2).unwrap();
        first.is_some() && second.is_some()
    }
}
