//! The lifetime policy and the clock floor.

use crate::error::CaError;

/// The earliest time a build of this crate signs at: 2026-10-01T00:00:00Z. A clock
/// that reads earlier is wrong (an unset real-time clock reads 1970), and a certificate
/// stamped from it would be valid long before and after the real time. Raised when the
/// build date moves on.
pub const CLOCK_FLOOR_UNIX: i64 = 1_790_812_800;

/// How long a certificate lives, and the earliest clock it is issued at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    lifetime_s: i64,
    clock_floor: i64,
}

impl Policy {
    /// The shortest lifetime: one hour.
    pub const MIN_LIFETIME_S: i64 = 3_600;
    /// The longest lifetime: thirty days.
    pub const MAX_LIFETIME_S: i64 = 30 * 86_400;
    /// The default lifetime: seven days.
    pub const DEFAULT_LIFETIME_S: i64 = 7 * 86_400;
    /// How far before the issue time a certificate becomes valid: five minutes of slack.
    pub const BACKDATE_S: i64 = 300;

    /// A policy of `lifetime_s` seconds and the build's clock floor.
    ///
    /// # Errors
    /// [`CaError::Lifetime`] outside [`Self::MIN_LIFETIME_S`] to [`Self::MAX_LIFETIME_S`].
    pub fn new(lifetime_s: i64) -> Result<Self, CaError> {
        if (Self::MIN_LIFETIME_S..=Self::MAX_LIFETIME_S).contains(&lifetime_s) {
            Ok(Self {
                lifetime_s,
                clock_floor: CLOCK_FLOOR_UNIX,
            })
        } else {
            Err(CaError::Lifetime)
        }
    }

    /// The same policy with another clock floor (for a build that signs at a later time,
    /// or a test).
    #[must_use]
    pub const fn with_clock_floor(self, clock_floor: i64) -> Self {
        Self {
            clock_floor,
            ..self
        }
    }

    /// The lifetime in seconds.
    #[must_use]
    pub const fn lifetime_s(self) -> i64 {
        self.lifetime_s
    }

    /// The earliest unix time the CA signs at.
    #[must_use]
    pub const fn clock_floor(self) -> i64 {
        self.clock_floor
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            lifetime_s: Self::DEFAULT_LIFETIME_S,
            clock_floor: CLOCK_FLOOR_UNIX,
        }
    }
}
