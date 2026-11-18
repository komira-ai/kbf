//! When a pack the index names is taken for lost: the rules a forget of a pack waits for.
//!
//! The object store is general-purpose, and its objects can vanish while the pack index still
//! names them; a row that names a vanished pack keeps telling clients its blobs are held. The
//! index forgets such a pack ([`PackIndex::forget_pack`]) so its blobs are missing and a client
//! uploads them again. A forget costs work (every blob of the pack is uploaded or built again)
//! and never correctness, but a forget of a pack the store still holds throws that work away
//! for nothing. So a pack is forgotten only when its absence is confirmed, by these rules:
//!
//! - **Absence only.** The store must have answered that it does not have the bytes: not found,
//!   a range that starts past the end, or fewer bytes than asked. An answer that is not an
//!   answer (a 5xx, a timeout, a reset connection) is never a look at all: the callers pass
//!   only absences here, and a failed second look forgets nothing ([`Loss::Unconfirmed`]).
//! - **A second look that is not immediate.** The first absence of a pack is noted
//!   ([`Loss::Noted`]); a store whose volume server is restarting answers "not found" for an
//!   object its other replica holds, for as long as the restart takes. Only an absence seen
//!   at least [`LossRules::confirm_after`] after the first, and within
//!   [`LossRules::confirm_within`] of it, asks the store once more for the object's length
//!   (the second look); an earlier one forgets nothing ([`Loss::TooSoon`]), and a later one is a
//!   new first look. A read that gets the pack's bytes, or a second look that finds the object
//!   whole, clears what was noted ([`LossGuard::held`], [`Loss::Held`]); a read that ends before
//!   the bytes found absent does not (a pack cut short is still read before the cut).
//! - **Never a young pack.** The index forgets the pack only if it was recorded at least
//!   [`LossRules::min_age`] before, read in the forget's own transaction ([`Loss::Young`]): a
//!   pack just written may not be in every replica or tier of the store yet.
//! - **At most once an interval.** A key forgotten is not forgotten again for
//!   [`LossRules::forget_interval`] ([`Loss::Recent`]): a large object's key is its digest, so
//!   it is recorded again under the same key when uploaded again.
//!
//! What a forget removes is the whole pack: every blob row that points into it and the pack's
//! row, in one transaction. A blob whose bytes are also in another pack, which has no row for it
//! (the first location wins), is then missing too, and is packed again when a client uploads it:
//! nothing reads a pack's footer to find it there.
//!
//! The state is the process's own (one frontend's packs): two frontends that each see one
//! absence do not add up to a confirmation.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use kbf_pack_index::{Forgot, PackIndex};

use crate::blocking::offload;

/// The least time from the first absence of a pack to the look that confirms it: 10 minutes,
/// longer than a volume server of the store takes to restart.
pub const CONFIRM_AFTER: Duration = Duration::from_secs(10 * 60);

/// The most time from the first absence of a pack to the look that confirms it: an hour. An
/// absence seen later is a new first look.
pub const CONFIRM_WITHIN: Duration = Duration::from_secs(60 * 60);

/// The least age of a pack the index forgets: an hour, well above the time the store takes to
/// copy a write to all of its places (tierstore copies a write to its cold tier within minutes,
/// and sweeps up one it missed after ten).
pub const MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// The least time between two forgets of one key: an hour.
pub const FORGET_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The most packs noted as absent at once ([`LossRules::max_noted`]). Past it the oldest note is
/// dropped.
pub const MAX_NOTED: usize = 10_000;

/// The clock a guard reads.
pub type LossClock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// How long a guard waits before it takes a pack for lost. See the module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossRules {
    /// The least time from the first absence to the second look ([`CONFIRM_AFTER`]).
    pub confirm_after: Duration,
    /// The most time from the first absence to the second look ([`CONFIRM_WITHIN`]).
    pub confirm_within: Duration,
    /// The least age of a pack the index forgets ([`MIN_AGE`]).
    pub min_age: Duration,
    /// The least time between two forgets of one key ([`FORGET_INTERVAL`]).
    pub forget_interval: Duration,
    /// The most packs noted as absent at once ([`MAX_NOTED`]; at least one is); past it the
    /// oldest note goes.
    pub max_noted: usize,
}

impl Default for LossRules {
    fn default() -> Self {
        Self {
            confirm_after: CONFIRM_AFTER,
            confirm_within: CONFIRM_WITHIN,
            min_age: MIN_AGE,
            forget_interval: FORGET_INTERVAL,
            max_noted: MAX_NOTED,
        }
    }
}

/// What an absence of a pack came to ([`LossGuard::absent`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loss {
    /// The first absence: noted, nothing forgotten.
    Noted,
    /// An absence sooner than [`LossRules::confirm_after`] after the first: nothing forgotten.
    TooSoon,
    /// The key was forgotten less than [`LossRules::forget_interval`] ago: nothing forgotten.
    Recent,
    /// The second look found the object whole: what was noted is cleared, nothing forgotten.
    Held,
    /// The second look did not get an answer (the text says why): nothing forgotten, the note
    /// stays.
    Unconfirmed(String),
    /// The second look confirmed the absence, and the pack is younger than
    /// [`LossRules::min_age`]: nothing forgotten, the note stays.
    Young,
    /// The second look confirmed the absence, and the index has no pack under the key.
    NotRecorded,
    /// The second look confirmed the absence, and the index forgot the pack: `rows` blob rows.
    Forgotten {
        /// The blob rows removed.
        rows: usize,
    },
    /// The second look confirmed the absence, and the index failed the forget (the text says
    /// why): nothing forgotten, the note stays.
    IndexFailed(String),
}

impl Loss {
    /// Whether the index forgot the pack.
    #[must_use]
    pub const fn forgotten(&self) -> bool {
        matches!(self, Self::Forgotten { .. })
    }
}

/// The absences of packs a process has seen, and the forgets they came to. Shared by every
/// reader of one frontend's packs.
pub struct LossGuard {
    rules: LossRules,
    clock: RwLock<LossClock>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The first absence of each pack noted, by key, and the furthest end found absent since.
    noted: BTreeMap<String, Note>,
    /// When each key was forgotten, while it is within the interval.
    forgotten: BTreeMap<String, SystemTime>,
}

/// An absence of a pack noted: when it was first seen, and the furthest end of the bytes found
/// absent (0: the object itself).
#[derive(Clone, Copy)]
struct Note {
    first: SystemTime,
    end: u64,
}

impl fmt::Debug for LossGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LossGuard")
            .field("rules", &self.rules)
            .finish_non_exhaustive()
    }
}

impl Default for LossGuard {
    /// A guard with the default rules, reading the wall clock.
    fn default() -> Self {
        Self::new(LossRules::default(), Arc::new(SystemTime::now))
    }
}

/// The time from `at` to `now`; none when the clock went back.
fn since(at: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(at).unwrap_or(Duration::ZERO)
}

impl LossGuard {
    /// A guard with `rules`, reading `clock`.
    #[must_use]
    pub fn new(rules: LossRules, clock: LossClock) -> Self {
        Self {
            rules,
            clock: RwLock::new(clock),
            state: Mutex::default(),
        }
    }

    /// The guard's rules.
    #[must_use]
    pub const fn rules(&self) -> LossRules {
        self.rules
    }

    /// Reads `clock` from now on (the clock of the frontend's packs, which a test sets).
    pub fn set_clock(&self, clock: LossClock) {
        *self.clock.write().unwrap_or_else(PoisonError::into_inner) = clock;
    }

    fn now(&self) -> SystemTime {
        (self.clock.read().unwrap_or_else(PoisonError::into_inner))()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A read of the pack `key` got its bytes up to `through`: an absence noted of it is cleared,
    /// unless it was of bytes past `through` (a pack cut short is still read whole before the
    /// cut).
    pub fn held(&self, key: &str, through: u64) {
        let mut state = self.state();
        if state.noted.get(key).is_some_and(|note| through >= note.end) {
            state.noted.remove(key);
            tracing::info!(pack = key, "an absent pack was read: no longer noted");
        }
    }

    /// The store answered that it does not have the pack `key`, or has it shorter than `end`
    /// bytes. Notes it, or, when the module's rules say the time has come, asks `look` for the
    /// object's length (the second look) and has `index` forget the pack if that confirms the
    /// absence and the pack is old enough. `look` is called only then; an `end` of 0 takes
    /// only a missing object for an absence.
    pub async fn absent<F, Fut, E>(
        &self,
        index: &Arc<dyn PackIndex>,
        key: &str,
        end: u64,
        look: F,
    ) -> Loss
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<u64>, E>>,
        E: fmt::Display,
    {
        let now = self.now();
        if let Some(waits) = self.wait(key, end, now) {
            match &waits {
                Loss::Noted => tracing::info!(pack = key, "absent: noted for a later second look"),
                _ => tracing::debug!(pack = key, ?waits, "a pack absent again: kept"),
            }
            return waits;
        }
        let length = match look().await {
            Ok(length) => length,
            Err(error) => {
                let error = error.to_string();
                tracing::warn!(pack = key, %error, "the second look at an absent pack got no answer: kept");
                return Loss::Unconfirmed(error);
            }
        };
        if length.is_some_and(|length| length >= end) {
            self.state().noted.remove(key);
            tracing::warn!(pack = key, "the second look found the pack whole: kept");
            return Loss::Held;
        }
        let forget = Arc::clone(index);
        let pack = key.to_owned();
        let min_age = self.rules.min_age;
        let loss = match offload(move || forget.forget_pack(&pack, min_age)).await {
            Ok(Forgot::Forgotten { rows }) => {
                let mut state = self.state();
                state.noted.remove(key);
                state.forgotten.insert(key.to_owned(), now);
                Loss::Forgotten { rows }
            }
            Ok(Forgot::Young) => Loss::Young,
            Ok(Forgot::NotRecorded) => {
                self.state().noted.remove(key);
                Loss::NotRecorded
            }
            Err(error) => Loss::IndexFailed(error.to_string()),
        };
        match &loss {
            Loss::Forgotten { rows } => {
                tracing::warn!(pack = key, rows, ?length, "a lost pack: forgotten");
            }
            other => {
                tracing::warn!(pack = key, ?length, loss = ?other, "a pack confirmed absent was not forgotten")
            }
        }
        loss
    }

    /// What an absence of `key` at `now` comes to without a second look, or `None` when it is
    /// time for one.
    fn wait(&self, key: &str, end: u64, now: SystemTime) -> Option<Loss> {
        let rules = &self.rules;
        let mut state = self.state();
        state
            .noted
            .retain(|_, note| since(note.first, now) <= rules.confirm_within);
        state
            .forgotten
            .retain(|_, at| since(*at, now) < rules.forget_interval);
        if state.forgotten.contains_key(key) {
            return Some(Loss::Recent);
        }
        match state.noted.get_mut(key) {
            Some(note) => {
                note.end = note.end.max(end);
                (since(note.first, now) < rules.confirm_after).then_some(Loss::TooSoon)
            }
            None => {
                while state.noted.len() >= rules.max_noted.max(1) {
                    let oldest = (state.noted.iter())
                        .min_by_key(|(_, note)| note.first)
                        .map(|(key, _)| key.clone())
                        .unwrap_or_default();
                    state.noted.remove(&oldest);
                }
                let note = Note { first: now, end };
                state.noted.insert(key.to_owned(), note);
                Some(Loss::Noted)
            }
        }
    }
}
