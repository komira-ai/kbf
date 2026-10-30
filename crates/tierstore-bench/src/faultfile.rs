//! The fault proxy of another process, as a cell sees it ([`crate::cells::engine::FaultSwitch`]).
//!
//! The proxy (`fault-proxy --control FILE --result-dir DIR`) reads the control file every tick and
//! writes its counters file when they change. [`FileSwitch`] sets a mode by writing the control
//! file, then waits until the counters file says the proxy is in that mode: a fault arm is never
//! measured before the fault is in force. A proxy that does not answer in time is an error, not a
//! healthy arm.
//!
//! Every line written carries a time to live (`MODE ttl=SECS`), so a run that dies with a fault in
//! force leaves a proxy that falls back to `pass` by itself, not one that refuses the store for
//! ever. The control file is replaced atomically ([`crate::atomic`]): the proxy reads it every
//! tick and must never see half a line.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::atomic;
use crate::cells::engine::FaultSwitch;
use crate::error::BenchError;
use crate::proxy::{COUNTERS_FILE, Counters, FaultMode};

/// The time to live of a fault when `--fault-ttl-secs` is not given: five minutes. It has to be
/// longer than the longest stretch a cell holds one mode, since a fault that ends early turns the
/// rest of a fault arm into a healthy one.
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// How long [`FileSwitch::new`] waits for the proxy to take a mode.
const SETTLE: Duration = Duration::from_secs(30);
/// How often the counters file is read while waiting.
const POLL: Duration = Duration::from_millis(50);

/// A proxy process driven through its control file and watched through its counters file.
#[derive(Clone, Debug)]
pub struct FileSwitch {
    control: PathBuf,
    counters: PathBuf,
    settle: Duration,
    ttl: Duration,
}

impl FileSwitch {
    /// The switch of a proxy started with `--control control` and `--result-dir result_dir`.
    #[must_use]
    pub fn new(control: PathBuf, result_dir: &Path) -> Self {
        Self {
            control,
            counters: result_dir.join(COUNTERS_FILE),
            settle: SETTLE,
            ttl: DEFAULT_TTL,
        }
    }

    /// As [`Self::new`], with faults that end `ttl` after they are set (whole seconds, at least
    /// one: the proxy refuses `ttl=0`).
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl.max(Duration::from_secs(1));
        self
    }

    /// How long a fault lasts after it is set.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The line of the control file for `mode`.
    fn line(&self, mode: FaultMode) -> String {
        format!("{mode} ttl={}\n", self.ttl.as_secs())
    }

    /// As [`Self::new`], waiting `settle` at most for the proxy to take a mode.
    #[must_use]
    pub fn settling_within(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// The counters as the proxy last wrote them; none if the file is absent or half written.
    fn counters(&self) -> Option<Counters> {
        let text = std::fs::read(&self.counters).ok()?;
        serde_json::from_slice(&text).ok()
    }
}

impl FaultSwitch for FileSwitch {
    fn set_mode(&self, mode: FaultMode) -> Result<(), BenchError> {
        atomic::write(&self.control, self.line(mode).as_bytes()).map_err(|source| {
            BenchError::Io {
                path: self.control.clone(),
                source,
            }
        })?;
        let wanted = mode.to_string();
        let deadline = Instant::now() + self.settle;
        while self.counters().is_none_or(|c| c.mode != wanted) {
            if Instant::now() >= deadline {
                return Err(BenchError::Proxy(format!(
                    "the fault proxy did not take the mode {wanted} in time"
                )));
            }
            std::thread::sleep(POLL);
        }
        Ok(())
    }

    /// The requests the proxy has sent on; 0 while its counters cannot be read.
    fn request_count(&self) -> u64 {
        self.counters().map_or(0, |c| c.requests)
    }
}
