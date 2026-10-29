//! The farm a server runs, built from its listeners' settings: the one place the
//! all-in-one server and the frontend both take them from, so a setting added here reaches
//! both.

use kbf_front::Backend;
use kbf_sched::WholeNodeConfig;

use super::Farm;
use crate::serve::Listeners;

impl Farm {
    /// A farm over `cache` with the settings `listeners` carries: how long unservable work
    /// waits, how long a finished operation is kept, the memory ladder, the learned sizes
    /// the placement rule (`--placement-load`) and the whole-node settings
    /// (`--whole-node-max`). The caller adds what is its own (the
    /// recorder, the state store).
    #[must_use]
    pub(crate) fn from_listeners(cache: impl Into<Backend>, listeners: &Listeners) -> Self {
        Self::on_core(
            listeners.scheduler_core,
            cache.into(),
            listeners.unservable_wait,
            listeners.finished_retention,
        )
        .with_memory_ladder(listeners.memory_ladder.clone())
        .with_learned_sizes(listeners.learned_sizes, listeners.memory_ladder.first())
        .with_placement_load(listeners.placement_load)
        .with_whole_node(listeners.whole_node.clone())
    }

    /// The core this farm runs on, as its driver is (not as a flag says).
    #[must_use]
    pub const fn scheduler_core(&self) -> crate::SchedulerCore {
        self.driver.core()
    }

    /// This farm, running whole-machine requests with `config`'s whole-node drain and caps
    /// (`--whole-node-max`) if it names any, as the scheduler built by [`Farm::new`] does
    /// not. Set before the farm takes a state store over.
    #[must_use]
    pub fn with_whole_node(mut self, config: Option<WholeNodeConfig>) -> Self {
        if let Some(config) = config {
            self.settings.whole_node = Some(config);
            self.driver.set_whole_node(self.term, &self.settings);
        }
        self
    }
}
