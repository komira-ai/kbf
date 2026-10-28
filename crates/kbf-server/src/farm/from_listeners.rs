//! The farm a server runs, built from its listeners' settings: the one place the
//! all-in-one server and the frontend both take them from, so a setting added here reaches
//! both.

use kbf_front::Backend;

use super::Farm;
use crate::serve::Listeners;

impl Farm {
    /// A farm over `cache` with the settings `listeners` carries: how long unservable work
    /// waits, how long a finished operation is kept, the memory ladder, the learned sizes
    /// and the placement rule (`--placement-load`). The caller adds what is its own (the
    /// recorder, the state store).
    #[must_use]
    pub(crate) fn from_listeners(cache: impl Into<Backend>, listeners: &Listeners) -> Self {
        Self::new(
            cache,
            listeners.unservable_wait,
            listeners.finished_retention,
        )
        .with_memory_ladder(listeners.memory_ladder.clone())
        .with_learned_sizes(listeners.learned_sizes, listeners.memory_ladder.first())
        .with_placement_load(listeners.placement_load)
    }
}
