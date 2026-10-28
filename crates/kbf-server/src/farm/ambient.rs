//! What a farm reads from the process around it: the wall clock, and the number that
//! names a new incarnation of the process. A farm built by [`Farm::new`](super::Farm::new)
//! reads the real ones; a farm built by `Farm::for_test` reads fixed ones, so that a run
//! of it can be written down and compared byte for byte.

use std::time::SystemTime;
#[cfg(any(test, feature = "testkit"))]
use std::time::{Duration, UNIX_EPOCH};

use super::{process_term, unix_ms};

/// Where the wall clock and the process term come from.
#[derive(Clone, Copy, Debug)]
pub(super) enum Ambient {
    /// The operating system's clock and [`process_term`].
    Process,
    /// A term chosen by the caller, and a wall clock that starts at `unix_ms` when the
    /// farm is built and then moves with tokio's clock, so a test that pauses it
    /// controls the wall clock too.
    #[cfg(any(test, feature = "testkit"))]
    Fixed {
        term: u64,
        started: tokio::time::Instant,
        unix_ms: u64,
    },
}

impl Ambient {
    /// The term a new incarnation (a farm opening a state store) names itself by.
    pub(super) fn term(self) -> u64 {
        match self {
            Self::Process => process_term(),
            #[cfg(any(test, feature = "testkit"))]
            Self::Fixed { term, .. } => term,
        }
    }

    /// The wall clock now, in milliseconds since the Unix epoch.
    pub(super) fn unix_ms(self) -> u64 {
        match self {
            Self::Process => unix_ms(),
            #[cfg(any(test, feature = "testkit"))]
            Self::Fixed {
                started, unix_ms, ..
            } => {
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                unix_ms.saturating_add(elapsed)
            }
        }
    }

    /// The wall clock now.
    pub(super) fn wall(self) -> SystemTime {
        match self {
            Self::Process => SystemTime::now(),
            #[cfg(any(test, feature = "testkit"))]
            Self::Fixed { .. } => UNIX_EPOCH + Duration::from_millis(self.unix_ms()),
        }
    }
}
