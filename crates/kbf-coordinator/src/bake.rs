//! Baking: how many healthy beats a host has sent at the version it runs.
//!
//! A host that has just updated holds the slot it updated in until it has beaten healthy
//! [`crate::services::ServiceDef::bake_beats`] times (`docs/design/host-updates.md`, section
//! 5.3). The count is kept in memory, per service and host, beside the heartbeats
//! ([`crate::fleet::Fleet`]): a restart of the coordinator forgets it, and a host at the
//! desired version bakes again. So does a host that was silent for [`DOWN_AFTER`] or more
//! between two beats: the count only grows on beats, and a host that vanished is not
//! proven by the beats from before it went.

use std::time::Duration;

use kbf_updater::version::Version;
use kbf_updater::wire::{Health, Heartbeat};

use crate::fleet::DOWN_AFTER;

/// The most beats a definition may ask a host to bake for.
pub const MAX_BAKE_BEATS: u32 = 100;

/// The beats a host has sent healthy in a row at one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bake {
    /// The version the host ran in those beats.
    pub target: Version,
    /// How many.
    pub beats: u32,
    /// When the last of them came.
    pub at: Duration,
}

impl Bake {
    /// The count after `beat`, which came at `now`, given the count before it: one more when
    /// the beat is healthy, not updating and not waiting for a promotion at the same version
    /// as before, one at a new version or after a silence of [`DOWN_AFTER`] or more, and none
    /// (`None`) for any other beat.
    #[must_use]
    pub fn after(before: Option<&Self>, beat: &Heartbeat, now: Duration) -> Option<Self> {
        let counts =
            beat.health == Health::Healthy && beat.updating.is_none() && !beat.promotion_pending;
        counts.then(|| Self {
            target: beat.version.clone(),
            beats: before
                .filter(|b| b.target == beat.version)
                .filter(|b| now.saturating_sub(b.at) < DOWN_AFTER)
                .map_or(1, |b| b.beats.saturating_add(1)),
            at: now,
        })
    }
}
