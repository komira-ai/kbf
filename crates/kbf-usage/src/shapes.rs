//! Action key to profile name: what a `KeyMap` reads.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_usage::{KeyMap, ProfileKey};
use kbf_types::{ActionKey, WorkKey};

/// The most actions an index holds. At about 250 bytes an entry that is about 16 MiB; the
/// same number as the farm's set of action shapes already seen.
pub const MAX_SHAPES: usize = 65_536;

struct Entry {
    profile: ProfileKey,
    /// Orders entries by when they were last put in: the smallest is the oldest.
    tick: u64,
}

#[derive(Default)]
struct Inner {
    by_action: BTreeMap<ActionKey, Entry>,
    by_tick: BTreeMap<u64, ActionKey>,
    next_tick: u64,
    evicted: u64,
}

/// Which profile an action's runs are kept under.
///
/// The scheduler asks for a profile by the action's [`WorkKey`], which holds the action's
/// digest and nothing of its shape. The farm knows the shape when an action is submitted
/// or loaded, and puts the profile name here then; a lookup of an action not here gives
/// `None`, and the scheduler charges it flat.
///
/// Bounded to a number of actions chosen at construction ([`MAX_SHAPES`] by default):
/// putting a new action into a full index forgets the one put in longest ago (and counts
/// it, [`ShapeIndex::evicted`]). The farm removes an action when its operation ends.
pub struct ShapeIndex {
    cap: usize,
    inner: Mutex<Inner>,
}

impl Default for ShapeIndex {
    fn default() -> Self {
        Self::with_capacity(MAX_SHAPES)
    }
}

impl ShapeIndex {
    /// An empty index of [`MAX_SHAPES`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty index of `cap` actions (at least one).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records that `action`'s runs are kept under `profile`. An action already here is
    /// updated and counts as the newest.
    pub fn insert(&self, action: ActionKey, profile: ProfileKey) {}

    /// Forgets `action`; whether it was here.
    pub fn remove(&self, action: &ActionKey) -> bool {
        false
    }

    /// The profile `action` is kept under.
    #[must_use]
    pub fn get(&self, action: &ActionKey) -> Option<ProfileKey> {
        None
    }

    /// The profile of the action `key` names; `None` when it is not here, or when `key` is
    /// not an action's (its dedup text is not a digest).
    #[must_use]
    pub fn lookup(&self, key: &WorkKey) -> Option<ProfileKey> {
        self.get(&ActionKey::try_from(key).ok()?)
    }

    /// How many actions are held.
    #[must_use]
    pub fn len(&self) -> usize {
        0
    }

    /// Whether no action is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many actions were forgotten to make room, since the index was made.
    #[must_use]
    pub fn evicted(&self) -> u64 {
        0
    }

    /// The map the scheduler's curve source reads.
    #[must_use]
    pub fn key_map(self: &Arc<Self>) -> KeyMap {
        let index = Arc::clone(self);
        Box::new(move |key: &WorkKey| index.lookup(key))
    }
}
