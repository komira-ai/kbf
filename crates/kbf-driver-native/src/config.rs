//! Where and how the native driver runs actions.

use std::path::PathBuf;
use std::time::Duration;

use kbf_outputs::OutputLimits;

/// How much memory one lease's processes may hold together before they are killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryPolicy {
    /// The limit is this percentage of what the lease booked...
    pub percent: u64,
    /// ...plus this many bytes.
    pub headroom_bytes: u64,
}

impl MemoryPolicy {
    /// 150 % of the booking plus 512 MiB, the container driver's per-lease cap too.
    pub const DEFAULT: Self = Self {
        percent: 150,
        headroom_bytes: 512 << 20,
    };

    /// The limit for a lease that booked `memory_bytes`; `None` (no limit) when nothing
    /// was booked, as the container driver does.
    #[must_use]
    pub fn limit(&self, memory_bytes: u64) -> Option<u64> {
        (memory_bytes > 0).then(|| {
            u64::try_from(u128::from(memory_bytes) * u128::from(self.percent) / 100)
                .unwrap_or(u64::MAX)
                .saturating_add(self.headroom_bytes)
        })
    }
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The default of [`NativeConfig::term_grace`]: two seconds.
pub const DEFAULT_TERM_GRACE: Duration = Duration::from_secs(2);

/// Where and how the native driver runs actions.
#[derive(Clone, Debug)]
pub struct NativeConfig {
    /// The directory each lease's directory is made in.
    pub scratch: PathBuf,
    /// The timeout of an action that names none.
    pub default_timeout: Duration,
    /// How much output one action may leave, stdout and stderr included.
    pub outputs: OutputLimits,
    /// How long one lease's whole output upload (outputs, stdout and stderr) may take
    /// before the lease fails saying so ([`kbf_daemon::upload_within`]).
    pub upload_deadline: Duration,
    /// The per-lease memory limit.
    pub memory: MemoryPolicy,
    /// How often the action's memory is measured (and its process tree refreshed).
    pub poll: Duration,
    /// How long ending an action's processes may take before the survivors fail the
    /// lease.
    pub kill_wait: Duration,
    /// How long the processes of an action are given to
    /// end after SIGTERM before they are killed; zero kills at once
    /// ([`crate::grace`]). Extra to `kill_wait`, which bounds the kill that follows, and
    /// outside the action's timeout and the memory watch, which have ended the action
    /// before it starts and do not run during it; at most [`crate::grace::MAX_GRACE`]
    /// (60 s). A lease past its memory limit gets none.
    pub term_grace: Duration,
}

impl NativeConfig {
    /// A configuration with a one hour default timeout, the default output limits,
    /// upload deadline ([`kbf_daemon::UPLOAD_DEADLINE`]) and memory policy, a 250 ms poll, a 5 s kill wait after a 2 s SIGTERM grace.
    #[must_use]
    pub fn new(scratch: PathBuf) -> Self {
        Self {
            scratch,
            default_timeout: Duration::from_secs(3600),
            outputs: OutputLimits::DEFAULT,
            upload_deadline: kbf_daemon::UPLOAD_DEADLINE,
            memory: MemoryPolicy::DEFAULT,
            poll: Duration::from_millis(250),
            kill_wait: Duration::from_secs(5),
            term_grace: DEFAULT_TERM_GRACE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a limit for an unbooked lease (which would kill everything that books
    /// nothing at the headroom), a percentage applied twice or not at all, and an
    /// overflow on a huge booking.
    #[test]
    fn the_limit_is_a_percentage_of_the_booking_plus_headroom() {
        let policy = MemoryPolicy::DEFAULT;
        assert_eq!(policy.limit(0), None);
        assert_eq!(policy.limit(1 << 30), Some((3 << 29) + (512 << 20)));
        assert_eq!(policy.limit(u64::MAX), Some(u64::MAX));
        let tight = MemoryPolicy {
            percent: 100,
            headroom_bytes: 0,
        };
        assert_eq!(tight.limit(64 << 20), Some(64 << 20));
        assert_eq!(MemoryPolicy::default(), policy);
    }
}
