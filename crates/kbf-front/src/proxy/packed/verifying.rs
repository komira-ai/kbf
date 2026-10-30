//! The keys an ingest is verifying: what the `cas/b/` janitor must not delete.
//!
//! [`ingest_set`](super::ingest_set) holds every key of the run's key set from the moment it
//! starts until it returns or is dropped ([`VerifyingKeys::hold`]), so the janitor
//! ([`kbf_pack::Sweeper`], through [`VerifyingKeys::is_held`]) never deletes a pack or a large
//! object that is being read or copied, however old the key says it is. The set is shared by
//! every clone of the [`Packs`](super::Packs) it belongs to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The keys held now, each with the number of holds on it (two runs of one lease hold the same
/// keys).
#[derive(Debug, Default)]
pub struct VerifyingKeys {
    held: Mutex<HashMap<String, usize>>,
}

impl VerifyingKeys {
    /// Holds `keys` until the returned [`Held`] is dropped.
    #[must_use]
    pub fn hold(self: &Arc<Self>, keys: Vec<String>) -> Held {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        for key in &keys {
            *held.entry(key.clone()).or_default() += 1;
        }
        Held {
            registry: Arc::clone(self),
            keys,
        }
    }

    /// Whether some [`Held`] holds `key` now.
    #[must_use]
    pub fn is_held(&self, key: &str) -> bool {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.contains_key(key)
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
        let mut held = self
            .registry
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in self.keys.drain(..) {
            held.entry(key).and_modify(|holds| *holds -= 1);
        }
        held.retain(|_, holds| *holds > 0);
    }
}
