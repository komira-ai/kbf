//! Time as an input: the runner, the dead-man and the registry read the clock through this
//! trait, so a test drives them with a clock that moves only when told to.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A monotonic clock with a boot id.
pub trait Clock: Send + Sync {
    /// Whole seconds on a monotonic clock. Comparable only under the same [`Clock::boot_id`].
    fn mono_secs(&self) -> u64;
    /// An id that changes when the host boots (the kernel's boot id).
    fn boot_id(&self) -> String;
    /// Waits for `secs` seconds.
    fn sleep_secs(&self, secs: u64);
}

/// A point on the monotonic clock, with the boot it was read in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub boot_id: String,
    pub mono_secs: u64,
}

impl Stamp {
    /// Now, on `clock`.
    pub fn now(clock: &dyn Clock) -> Stamp {
        Stamp {
            boot_id: clock.boot_id(),
            mono_secs: clock.mono_secs(),
        }
    }

    /// Seconds from this stamp to now, `0` when the host booted since: the time that passed
    /// before a reboot is unknown, and a window that restarts is bounded by one more window.
    pub fn elapsed_secs(&self, clock: &dyn Clock) -> u64 {
        if self.boot_id == clock.boot_id() {
            clock.mono_secs().saturating_sub(self.mono_secs)
        } else {
            0
        }
    }
}

/// The host's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn mono_secs(&self) -> u64 {
        let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        u64::try_from(now.tv_sec).unwrap_or(0)
    }

    fn boot_id(&self) -> String {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
        id.trim().to_owned()
    }

    fn sleep_secs(&self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }
}
