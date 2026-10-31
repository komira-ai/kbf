//! The keys an ingest is verifying and the keys the janitor has claimed: what lets the `cas/b/`
//! janitor delete without ever deleting a key an ingest is about to commit.
//!
//! **The invariant.** A key is, at any instant, held by ingests, or claimed by the janitor, or
//! neither, never both, and both ways of entering the state are decided under one lock:
//!
//! - [`VerifyingKeys::hold`] (an ingest, from the start of [`ingest_set`](super::ingest_set) to
//!   its end or its being dropped) fails if any of its keys is claimed.
//! - [`VerifyingKeys::claim`] (the janitor, taken after the pack index's last answer for the key
//!   and held through its delete) fails if the key is held, or already claimed.
//!
//! So an ingest that has begun makes the janitor keep the key, and an ingest that begins after the
//! janitor has claimed it is refused (`RejectReason::Claimed`) and reads nothing. The index is
//! never told of a pack whose object the janitor is deleting: a pack recorded before the claim is
//! seen by the index's last answer, and none can be recorded between the claim and the delete.
//!
//! The other half is the age: an ingest refuses a key as old as [`kbf_pack::MIN_GRACE`], and the
//! janitor deletes only keys at least that old, so a key an ingest accepts is not one the janitor
//! may delete for as long as the ingest's own age check stands, and a key held is kept whatever its
//! age. The set is shared by every clone of the [`Packs`](super::Packs) it belongs to.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

/// The keys held and the keys claimed, together under one lock.
#[derive(Debug, Default)]
struct State {
    /// Each key held, with the number of holds on it (two runs of one lease hold the same keys).
    held: HashMap<String, usize>,
    /// The keys the janitor has claimed for a delete.
    claimed: HashSet<String>,
}

/// The keys held and claimed now.
#[derive(Debug, Default)]
pub struct VerifyingKeys {
    state: Mutex<State>,
}

impl VerifyingKeys {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `keys` until the returned [`Held`] is dropped.
    ///
    /// # Errors
    /// [`Claimed`], naming the key, when the janitor has claimed one of the keys for its delete;
    /// nothing is held then.
    pub fn hold(self: &Arc<Self>, keys: Vec<String>) -> Result<Held, Claimed> {
        let mut state = self.lock();
        if let Some(claimed) = keys.iter().find(|key| state.claimed.contains(*key)) {
            return Err(Claimed(claimed.clone()));
        }
        for key in &keys {
            *state.held.entry(key.clone()).or_default() += 1;
        }
        Ok(Held {
            registry: Arc::clone(self),
            keys,
        })
    }

    /// Claims `key` for its delete until the returned [`Claim`] is dropped, or `None` when
    /// something holds it or it is already claimed.
    #[must_use]
    pub fn claim(self: &Arc<Self>, key: &str) -> Option<Claim> {
        let mut state = self.lock();
        if state.held.contains_key(key) || !state.claimed.insert(key.to_owned()) {
            return None;
        }
        Some(Claim {
            registry: Arc::clone(self),
            key: key.to_owned(),
        })
    }

    /// Whether some [`Held`] holds `key` now.
    #[must_use]
    pub fn is_held(&self, key: &str) -> bool {
        self.lock().held.contains_key(key)
    }
}

/// The keys of one hold, released when it is dropped.
#[derive(Debug)]
pub struct Held {
    registry: Arc<VerifyingKeys>,
    keys: Vec<String>,
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut state = self.registry.lock();
        for key in self.keys.drain(..) {
            state.held.entry(key).and_modify(|holds| *holds -= 1);
        }
        state.held.retain(|_, holds| *holds > 0);
    }
}

/// The key the janitor has claimed for its delete, which a hold was refused for.
#[derive(Debug, PartialEq, Eq)]
pub struct Claimed(pub String);

/// A claim on a key, released when it is dropped.
#[derive(Debug)]
pub struct Claim {
    registry: Arc<VerifyingKeys>,
    key: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.registry.lock().claimed.remove(&self.key);
    }
}
