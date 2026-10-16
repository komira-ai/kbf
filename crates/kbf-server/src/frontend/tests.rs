//! What a frontend does with the state store `bind_frontend` cannot yet open: this build
//! has no backend, so no start reaches the code that hands an open store to `/readyz`.

use std::sync::Arc;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};

use super::store_of;
use crate::StateStore;
use crate::farm::EpochPolicy;

/// Catches a frontend that probes a different store than the one it opened (or none when
/// one is open): `/readyz` must ask the very store the scheduler writes to, and ask
/// nothing when there is no store.
#[test]
fn readyz_probes_the_store_that_was_opened() {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = StateStore {
        store: Arc::clone(&store),
        epoch: EpochPolicy::Keep,
    };
    let probed = store_of(Some(&state)).expect("an open store is probed");
    assert!(Arc::ptr_eq(&probed, &store));
    assert!(store_of(None).is_none());
}
