//! Opening the core for the farm: its configuration, the rows it keeps in the state store,
//! the metrics it reports to, and the hosts it hears.
//!
//! - **Configuration** ([`config`]) is the scheduler the farm has always built: the
//!   unservable wait, the finished retention, and the placement mode of `--placement-load`
//!   given explicitly, so that raising the scheduler library cannot change where work goes.
//! - **Storage** ([`storage`]) is the farm's own row codec ([`KbfRowCodec`]) and build-only
//!   rows ([`KbfExt`]) over the state store. The core installs the writer that writes them;
//!   there is no other [`Writer`](js_server_core::Writer) to supply.
//! - **Metrics** ([`KbfObserver`]): the time a submit waited for the core's lock goes to
//!   `kbf_sched_lock_wait_seconds`, which the farm's own submit reports.
//! - **Admission** is the core's own [`AllowAll`](js_server_core::AllowAll): the farm decides
//!   whether a daemon is heard before it calls the core, in the worker service
//!   (`crate::worker`), against a deny list it reads asynchronously, which the core's
//!   synchronous [`Admission`](js_server_core::Admission) cannot wait for.

use std::sync::Arc;
use std::time::Duration;

use js_server_core::{Config, Core, Observer, OpenError};
use js_store::{EpochPolicy as StoredEpoch, StateError};
use kbf_front::Backend;
use kbf_state::Store;
use kbf_types::FarmTime;

use super::adapter::KbfAdapter;
use super::codec::KbfRowCodec;
use super::ext::KbfExt;
use crate::farm::EpochPolicy;
use crate::placement_load::PlacementLoad;

/// The core's configuration for a farm of scheduler term `term`: queued work no live
/// worker can run is refused after `unservable_wait`, a finished operation is kept for
/// `finished_retention`, and actions are placed as `placement` says.
#[must_use]
pub fn config(
    term: u64,
    unservable_wait: Duration,
    finished_retention: Duration,
    placement: PlacementLoad,
) -> Config {
    Config::new(term)
        .with_unservable_wait(unservable_wait)
        .with_finished_retention(finished_retention)
        .with_load_config(placement.config())
}

/// The core's configuration for a farm of scheduler term `term` and `settings`: [`config`],
/// with the memory ladder and floors, the loss requeues, the whole-node drain and caps
/// (`--whole-node-max`) if they are set, and the wait of the orphan sweep, as
/// [`Farm::new`](crate::farm::Farm::new) and its builders give the farm's own scheduler.
///
/// Under `--usage-profiles on` the configuration also carries the curves of `usage`, the farm's
/// usage side, which the scheduler reads for an action that has a prediction; `record` and `off`
/// carry none, and a farm with no usage side (no state store) has none to give. Every
/// configuration built from one usage side carries the one source, so the core and what feeds it
/// agree.
#[must_use]
pub(crate) fn farm_config(
    term: u64,
    unservable_wait: Duration,
    finished_retention: Duration,
    settings: &crate::farm::Settings,
    usage: Option<&super::usage::Usage>,
) -> Config {
    let config = self::config(
        term,
        unservable_wait,
        finished_retention,
        settings.placement_load,
    )
    .with_memory_ladder(settings.ladder.clone())
    .with_memory_floors(settings.floors)
    .with_loss_requeues(kbf_sched::LOSS_REQUEUES)
    .with_orphan_wait(settings.orphan_wait);
    let config = match usage {
        Some(usage) if settings.usage.serves() => config.with_curves(usage.curves()),
        _ => config,
    };
    match &settings.whole_node {
        Some(whole) => config.with_whole_node(whole.clone()),
        None => config,
    }
}

/// The state store `store` as the core keeps its rows in it, and what a start does with the
/// store's epoch.
#[must_use]
pub fn storage(store: Arc<dyn Store>, epoch: EpochPolicy) -> js_server_core::Storage {
    let policy = match epoch {
        EpochPolicy::Keep => StoredEpoch::Keep,
        EpochPolicy::Bump => StoredEpoch::Bump,
    };
    js_server_core::Storage::new(store, Arc::new(KbfRowCodec), Arc::new(KbfExt)).with_epoch(policy)
}

/// Reports the core to the farm's metrics.
#[derive(Clone, Debug)]
pub struct KbfObserver {
    cache: Backend,
}

impl KbfObserver {
    /// An observer that reports to the metrics of `cache`.
    #[must_use]
    pub const fn new(cache: Backend) -> Self {
        Self { cache }
    }
}

impl Observer for KbfObserver {
    fn lock_waited(&self, waited: Duration) {
        self.cache
            .metrics()
            .sched_lock_wait
            .observe_duration(waited);
    }
}

/// A core over `store` that drives `adapter`: it takes up what the store holds
/// ([`KbfAdapter::adopted`] is told of each operation), writes every change of the scheduler
/// to the store before acting on it, hears every daemon the farm lets through, and reports
/// to the metrics of the adapter's cache. `now` is the farm time of the start.
///
/// # Errors
/// The store cannot be read or written, or holds a row that is not one the scheduler wrote.
pub fn open(
    config: &Config,
    mut adapter: KbfAdapter,
    store: Arc<dyn Store>,
    epoch: EpochPolicy,
    now: FarmTime,
) -> Result<Core<KbfAdapter>, StateError> {
    // Read before the core exists: no caller waits for the store behind its lock.
    adapter.restore_learned();
    let observer = KbfObserver::new(adapter.cache.clone());
    let core =
        Core::open(config, adapter, storage(store, epoch), now).map_err(|error| match error {
            OpenError::State(state) => state,
            // kbf never sets `memory_schedulable_percent`, whose default of 100 always
            // validates, so this arm is not reached; it stays an error, not a panic.
            OpenError::Config(percent) => StateError::Corrupt {
                key: "memory_schedulable_percent".to_owned(),
                why: percent.to_string(),
            },
        })?;
    Ok(core.with_observer(observer))
}
