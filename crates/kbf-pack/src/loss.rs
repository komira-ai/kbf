//! When a pack the index names is taken for lost ([`LossWitness::judge`]).
//!
//! A store is general-purpose, and its objects can vanish while the index names them: a row
//! that names a vanished pack tells clients its blobs are held, so they are never uploaded
//! again. But a store's answer that an object is not there can also be wrong for a while: a
//! read that raced the object's write, a node whose view lags. So a pack is forgotten only when
//! every one of these holds:
//!
//! - the store answered that the bytes are not there (`NotFound`, or a range past the object's
//!   end), never a failure to answer (a 5xx), which keeps the rows and is the read's error;
//! - the store's length of the object then shows it gone or shorter than the bytes asked;
//! - an earlier look found it so too, at least [`LossRule::confirm_after`] before this one
//!   (the first look is only remembered, [`Loss::Unconfirmed`]);
//! - the pack was recorded at least [`LossRule::min_age`] ago by the index's clock, checked in
//!   the same transaction as the delete ([`PackIndex::forget_pack`]); a younger pack is kept
//!   ([`Loss::Young`]).
//!
//! A pack is forgotten at most once per such pair of looks: the sighting goes with the forget,
//! and the same key recorded again is a new pack, young again. The sightings are bounded by
//! [`WITNESS_ENTRIES`], the oldest dropped first.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use kbf_pack_index::{PackIndex, PackIndexError};

use crate::blocking::offload;

/// How long after a first look that found a pack lost a later look may confirm it: 30 seconds.
pub const CONFIRM_AFTER: Duration = Duration::from_secs(30);

/// The least age of a pack, by the index's clock, for it to be forgotten: 15 minutes, well past
/// the time any view of a store takes to show an object it acknowledged.
pub const MIN_AGE: Duration = Duration::from_secs(15 * 60);

/// The most packs a witness remembers a first look of.
pub const WITNESS_ENTRIES: usize = 10_000;

/// The clock a witness dates its looks by.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// How sure a witness must be before a pack is forgotten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossRule {
    /// The least time between the first look that found a pack lost and the one that confirms
    /// it.
    pub confirm_after: Duration,
    /// The least age of a pack, by the index's clock, for it to be forgotten.
    pub min_age: Duration,
}

impl Default for LossRule {
    fn default() -> Self {
        Self {
            confirm_after: CONFIRM_AFTER,
            min_age: MIN_AGE,
        }
    }
}

/// What a look at a pack a read found lost came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loss {
    /// The store holds the pack whole after all: the rows stay, and an earlier look is
    /// forgotten.
    Held,
    /// The first look that found it lost, or one too soon after it: remembered, the rows stay.
    Unconfirmed,
    /// Confirmed lost, but the pack is younger than [`LossRule::min_age`] (or no longer
    /// recorded): the rows stay.
    Young,
    /// Confirmed lost: the index forgot the pack and its `rows` blob rows.
    Forgotten {
        /// The blob rows the index removed.
        rows: usize,
    },
}

impl Loss {
    /// Whether the index forgot the pack.
    #[must_use]
    pub const fn forgotten(&self) -> bool {
        matches!(self, Self::Forgotten { .. })
    }
}

/// The first looks that found packs lost, shared by every reader of one index.
pub struct LossWitness {
    state: Mutex<State>,
}

struct State {
    rule: LossRule,
    clock: Clock,
    /// Each pack's first look, and its place in `order`.
    seen: BTreeMap<String, (SystemTime, u64)>,
    order: BTreeMap<u64, String>,
    next: u64,
}

impl Default for LossWitness {
    fn default() -> Self {
        Self::new(LossRule::default(), Arc::new(SystemTime::now))
    }
}

impl std::fmt::Debug for LossWitness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LossWitness")
            .field("rule", &self.lock().rule)
            .finish_non_exhaustive()
    }
}

impl LossWitness {
    /// A witness that confirms by `rule`, dating its looks by `clock`.
    #[must_use]
    pub fn new(rule: LossRule, clock: Clock) -> Self {
        Self {
            state: Mutex::new(State {
                rule,
                clock,
                seen: BTreeMap::new(),
                order: BTreeMap::new(),
                next: 0,
            }),
        }
    }

    /// From now on, confirms by `rule`.
    pub fn set_rule(&self, rule: LossRule) {
        self.lock().rule = rule;
    }

    /// From now on, dates its looks by `clock`.
    pub fn set_clock(&self, clock: Clock) {
        self.lock().clock = clock;
    }

    /// Judges the pack `key` a read found without the bytes up to `end`, the store having
    /// answered its length as `length` (`None`: no object), and has `index` forget it when the
    /// loss is confirmed (the module docs).
    ///
    /// # Errors
    /// [`PackIndexError`] when the index fails the forget: the rows and the first look stay.
    pub async fn judge(
        &self,
        index: &Arc<dyn PackIndex>,
        key: &str,
        length: Option<u64>,
        end: u64,
    ) -> Result<Loss, PackIndexError> {
        if length.is_some_and(|length| length >= end) {
            self.forget_look(key);
            return Ok(Loss::Held);
        }
        let Some(min_age) = self.look(key) else {
            return Ok(Loss::Unconfirmed);
        };
        let index = Arc::clone(index);
        let pack = key.to_owned();
        match offload(move || index.forget_pack(&pack, min_age)).await? {
            Some(rows) => {
                self.forget_look(key);
                Ok(Loss::Forgotten { rows })
            }
            None => Ok(Loss::Young),
        }
    }

    /// A look that found `key` lost: the age a pack must have to be forgotten, when an earlier
    /// look confirms it; else `None`, the first look remembered.
    fn look(&self, key: &str) -> Option<Duration> {
        let mut state = self.lock();
        let now = (state.clock)();
        if let Some((first, _)) = state.seen.get(key) {
            let since = now.duration_since(*first).unwrap_or_default();
            return (since >= state.rule.confirm_after).then_some(state.rule.min_age);
        }
        let at = state.next;
        state.next += 1;
        state.seen.insert(key.to_owned(), (now, at));
        state.order.insert(at, key.to_owned());
        if state.seen.len() > WITNESS_ENTRIES
            && let Some((_, oldest)) = state.order.pop_first()
        {
            state.seen.remove(&oldest);
        }
        None
    }

    fn forget_look(&self, key: &str) {
        let mut state = self.lock();
        if let Some((_, at)) = state.seen.remove(key) {
            state.order.remove(&at);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
