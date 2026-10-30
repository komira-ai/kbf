//! The fault proxy of another process, as a cell sees it ([`crate::cells::engine::FaultSwitch`]).
//!
//! The proxy (`fault-proxy --control FILE --result-dir DIR`) reads the control file every tick and
//! writes its counters file when they change. [`FileSwitch`] sets a mode by writing the control
//! file, then waits until the counters file says the proxy is in that mode: a fault arm is never
//! measured before the fault is in force. A proxy that does not answer in time is an error, not a
//! healthy arm.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cells::engine::FaultSwitch;
use crate::error::BenchError;
use crate::proxy::{COUNTERS_FILE, Counters, FaultMode};

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
}

impl FileSwitch {
    /// The switch of a proxy started with `--control control` and `--result-dir result_dir`.
    #[must_use]
    pub fn new(control: PathBuf, result_dir: &Path) -> Self {
        Self {
            control,
            counters: result_dir.join(COUNTERS_FILE),
            settle: SETTLE,
        }
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
        std::fs::write(&self.control, format!("{mode}\n")).map_err(|source| BenchError::Io {
            path: self.control.clone(),
            source,
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
