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
//! - **The halt.** A halt leaves a [`Halt`] (`halt-<unit>`): the reason and the clear generation
//!   the agent saw in the last reply before it halted. A reply whose `halts` has a newer
//!   generation for the unit lifts the hold.
//! - **The size.** A heartbeat over [`crate::wire::MAX_HEARTBEAT_BYTES`] is read by no
//!   coordinator. [`fit`] drops the optional facts first and refuses the beat if that is not
//!   enough.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::version::Version;
use crate::wire::{DeferredReport, Heartbeat, Token, UpdateResult};

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

/// The longest a deferral lasts, in seconds, however long the retry hint or an older agent's
/// file says. It is the step runner's `MAX_RETRY_SECS` (kbf-step-runner) and the coordinator's
/// backoff cap: a bad or huge hint must not freeze a host. The crates do not depend on each
/// other, so a test pins the value.
pub const MAX_RETRY_SECS: u64 = 3600;

/// The most bytes of a deferral file that are read.
const MARK_MAX: u64 = 4096;

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
    /// This deferral as read at `now`: its retry time no further out than [`MAX_RETRY_SECS`]
    /// from `now`, so a file an older agent wrote with a longer one cannot hold the host longer
    /// than that.
    #[must_use]
    pub fn read_at(mut self, now: u64) -> Self {
        self.retry_after_unix = self
            .retry_after_unix
            .min(now.saturating_add(MAX_RETRY_SECS));
        self
    }

    /// The report for the heartbeat at `now`: none once the retry time has come.
    #[must_use]
    pub fn report(&self, now: u64) -> Option<DeferredReport> {
        (now < self.retry_after_unix).then(|| DeferredReport {
            target: self.target.clone(),
            reason: self.reason.clone(),
            retry_after_secs: self.retry_after_unix - now,
        })
    }

    /// Whether an order for `target` at `now` is one this deferral declines.
    #[must_use]
    pub fn declines(&self, target: &Version, now: u64) -> bool {
        self.target == *target && now < self.retry_after_unix
    }
}

/// A service held after a halt, and the clear generation the agent had seen in the last reply
/// before it halted: a reply with a newer one is a clear that came after the halt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Halt {
    pub reason: Token,
    pub generation: u64,
}

/// The deferrals and halts on disk, one file each per unit.
#[derive(Clone, Debug)]
pub struct Marks {
    dir: PathBuf,
}

impl Marks {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, unit: &str) -> PathBuf {
        self.dir.join(format!("defer-{unit}"))
    }

    fn halt_path(&self, unit: &str) -> PathBuf {
        self.dir.join(format!("halt-{unit}"))
    }

    /// The deferral of `unit`, if there is one.
    ///
    /// # Errors
    /// The file is there and cannot be read, is too long, or is no deferral.
    pub fn load(&self, unit: &str) -> Result<Option<Deferral>, String> {
        self.read(&self.path(unit))
    }

    /// The halt of `unit`, if there is one.
    ///
    /// # Errors
    /// The file is there and cannot be read, is too long, or is no halt.
    pub fn load_halt(&self, unit: &str) -> Result<Option<Halt>, String> {
        self.read(&self.halt_path(unit))
    }

    fn read<T: serde::de::DeserializeOwned>(
        &self,
        path: &std::path::Path,
    ) -> Result<Option<T>, String> {
        let read = std::fs::File::open(path).and_then(|file| {
            let mut text = Vec::new();
            file.take(MARK_MAX + 1).read_to_end(&mut text).map(|_| text)
        });
        let bad = |why: &dyn std::fmt::Display| format!("{}: {why}", path.display());
        let text = match read {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(bad(&e)),
        };
        if text.len() as u64 > MARK_MAX {
            return Err(bad(&"it is too long"));
        }
        serde_json::from_slice(&text).map(Some).map_err(|e| bad(&e))
    }

    /// Record `deferral` for `unit`: written beside, synced and renamed over.
    ///
    /// # Errors
    /// The directory does not take it.
    pub fn save(&self, unit: &str, deferral: &Deferral) -> std::io::Result<()> {
        self.write(&self.path(unit), &format!(".defer-{unit}.tmp"), deferral)
    }

    /// Record `halt` for `unit`, the same way.
    ///
    /// # Errors
    /// The directory does not take it.
    pub fn save_halt(&self, unit: &str, halt: &Halt) -> std::io::Result<()> {
        self.write(&self.halt_path(unit), &format!(".halt-{unit}.tmp"), halt)
    }

    fn write<T: Serialize>(
        &self,
        path: &std::path::Path,
        temp: &str,
        value: &T,
    ) -> std::io::Result<()> {
        let temp = self.dir.join(temp);
        serde_json::to_vec(value)
            .map_err(std::io::Error::other)
            .and_then(|body| {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&temp)
                    .and_then(|mut file| file.write_all(&body).and_then(|()| file.sync_all()))
            })
            .and_then(|()| std::fs::rename(&temp, path))
            .and_then(|()| std::fs::File::open(&self.dir).and_then(|dir| dir.sync_all()))
    }

    /// Forget the deferral of `unit`; none is not an error.
    ///
    /// # Errors
    /// The file is there and cannot be removed.
    pub fn clear(&self, unit: &str) -> std::io::Result<()> {
        Self::remove(&self.path(unit))
    }

    /// Forget the halt of `unit`; none is not an error.
    ///
    /// # Errors
    /// The file is there and cannot be removed.
    pub fn clear_halt(&self, unit: &str) -> std::io::Result<()> {
        Self::remove(&self.halt_path(unit))
    }

    fn remove(path: &std::path::Path) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
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

fn size(beat: &Heartbeat) -> usize {
    serde_json::to_vec(beat).map_or(usize::MAX, |bytes| bytes.len())
}

/// `beat` if it is at most `max` bytes as JSON; else `beat` without the facts the coordinator
/// can do without (`agent_features`, the step, commit, durations, `promotion_pending`,
/// `deferred`, and the phase, class and reason of a result, of `kbf` and of every service);
/// else nothing.
#[must_use]
pub fn fit(beat: Heartbeat, max: usize) -> Fit {
    if size(&beat) <= max {
        return Fit::Whole(beat);
    }
    let bare = bare(beat);
    if size(&bare) <= max {
        Fit::Trimmed(bare)
    } else {
        Fit::Oversized
    }
}

fn bare_result(result: &mut Option<UpdateResult>) {
    if let Some(result) = result {
        result.phase = None;
        result.class = None;
        result.reason = None;
    }
}

fn bare(mut beat: Heartbeat) -> Heartbeat {
    beat.agent_features.clear();
    beat.step = None;
    beat.commit = None;
    beat.step_secs.clear();
    beat.promotion_pending = false;
    beat.deferred = None;
    bare_result(&mut beat.last_update);
    for service in beat.services.iter_mut().flatten() {
        service.step = None;
        service.commit = None;
        service.step_secs.clear();
        service.promotion_pending = false;
        service.deferred = None;
        bare_result(&mut service.last_update);
    }
    beat
}
