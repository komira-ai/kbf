//! The agent's half of `Deferred` and `halt` (`docs/design/host-updates.md`, sections 3.8 and
//! 8, step 6c): what it announces, what it keeps on disk, and how it trims a heartbeat that has
//! grown too large.
//!
//! - **Features.** The agent announces [`ANNOUNCED`] in its heartbeat. The coordinator answers
//!   with the ones it supports (`features` in the reply); the agent uses a behaviour only
//!   while the last reply advertised it. An agent that sees none behaves as it did before: a
//!   drain bound is a `Failed`, it never declines an order for a deferral, and a failure it
//!   cannot repair is a plain `Failed`.
//! - **The deferral.** A deferred update leaves a [`Deferral`] on disk per unit
//!   (`defer-<unit>` in the agent's state directory), so a restart keeps the `retry_after`. It
//!   is in Unix seconds, because a restart has no memory of the process clock.
//! - **The size.** A heartbeat over [`crate::wire::MAX_HEARTBEAT_BYTES`] is read by no
//!   coordinator. [`fit`] drops the optional facts first and refuses the beat if that is not
//!   enough.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::version::Version;
use crate::wire::{DeferredReport, Heartbeat, Token};

/// The reply may carry a deferred update's report and the agent may decline for it.
pub const DEFERRED_V1: &str = "deferred-v1";

/// The reply may carry a halt and the agent holds the service for it.
pub const HALT_V1: &str = "halt-v1";

/// The features this agent announces in every heartbeat.
pub const ANNOUNCED: [&str; 2] = [DEFERRED_V1, HALT_V1];

/// The reason of a deferral whose drain bound passed.
pub const DRAIN_BOUND: &str = "drain_bound";

/// The reason of a halt: the helper could not put the previous release back and it does not run
/// healthy.
pub const ROLLBACK_FAILED: &str = "rollback_failed";

/// What the coordinator's last reply let the agent use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// Report a [`Hold::Deferred`] and decline an order for its `retry_after`.
    pub deferred: bool,
    /// Report a [`Hold::Halt`] and hold the service.
    pub halt: bool,
}

impl Features {
    /// What `advertised` (the reply's `features`) lets an agent that announced [`ANNOUNCED`]
    /// use: a name it does not know is not a feature.
    #[must_use]
    pub fn of(advertised: &[String]) -> Self {
        let has = |name: &str| advertised.iter().any(|a| a == name);
        Self {
            deferred: has(DEFERRED_V1),
            halt: has(HALT_V1),
        }
    }
}

/// Why an update ended without a verdict on the release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The host's moment was wrong; nothing was switched. Try again after the retry time.
    Deferred { reason: Token },
    /// The host is in a state no version and no retry repairs. The service is held.
    Halt { reason: Token },
}

/// A deferred hold for `reason`, if it is a token.
#[must_use]
pub fn deferred(reason: &str) -> Option<Hold> {
    Token::parse(reason).map(|reason| Hold::Deferred { reason })
}

/// A halt for `reason`, if it is a token.
#[must_use]
pub fn halt(reason: &str) -> Option<Hold> {
    Token::parse(reason).map(|reason| Hold::Halt { reason })
}

/// An update the host declined, and until when (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferral {
    pub target: Version,
    pub reason: Token,
    pub retry_after_unix: u64,
}

impl Deferral {
    /// The report for the heartbeat at `now`: none once the retry time has come.
    #[must_use]
    pub fn report(&self, now: u64) -> Option<DeferredReport> {
        let _ = now;
        None::<DeferredReport>
    }

    /// Whether an order for `target` at `now` is one this deferral declines.
    #[must_use]
    pub fn declines(&self, target: &Version, now: u64) -> bool {
        let _ = (target, now);
        false
    }
}

/// The deferrals on disk, one file per unit. Not kept yet.
#[derive(Clone, Debug)]
pub struct Marks {
    dir: PathBuf,
}

impl Marks {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// # Errors
    /// Never yet.
    pub fn load(&self, unit: &str) -> Result<Option<Deferral>, String> {
        let _ = (&self.dir, unit);
        Ok(None)
    }

    /// # Errors
    /// Never yet.
    pub fn save(&self, unit: &str, deferral: &Deferral) -> std::io::Result<()> {
        let _ = (unit, deferral);
        Ok(())
    }

    /// # Errors
    /// Never yet.
    pub fn clear(&self, unit: &str) -> std::io::Result<()> {
        let _ = unit;
        Ok(())
    }
}

/// A heartbeat as it may go out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Within the limit as it is.
    Whole(Heartbeat),
    /// Within the limit once the optional facts were dropped.
    Trimmed(Heartbeat),
    /// Over the limit even without them: it is not sent.
    Oversized,
}

/// `beat` as it is, whatever its size. Not checked yet.
#[must_use]
pub fn fit(beat: Heartbeat, max: usize) -> Fit {
    let _ = max;
    Fit::Whole(beat)
}
