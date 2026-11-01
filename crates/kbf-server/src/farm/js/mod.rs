//! The farm on the js core (`--scheduler-core js`): [`JsFarm`] is what [`Driver::Js`](super::driver::Driver) holds.
//!
//! Every method of [`Farm`](super::Farm) that differs by core starts with an arm that calls
//! the function of the same name in one of three files, with the same arguments after the
//! [`JsFarm`]:
//!
//! - [`hot`]: a daemon's `Hello`, heartbeat and `Result`, the tick, a caller's submit and
//!   wait, `quiet` and the end of the worker streams;
//! - [`lifecycle`]: the handover (fence, resume, close intake, flush, follow), the intake
//!   poll, the epoch, operator placement and the reads of the state store;
//! - [`views`]: the node lists and details, the update facts, the queue and lease
//!   snapshots, and the gauges and counters operators read.
//!
//! What the farm does before it serves is here: the core is built from the farm's
//! [`Settings`] ([`crate::farm_js::open::farm_config`]), built again when a builder changes
//! them or a state store is taken over, and the farm's term and lease epoch are the epoch
//! of the store the core opened ([`Core::epoch`]) once it has one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

use js_server_core::Core;
use kbf_front::Backend;
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};
use kbf_types::FarmTime;
use tokio::sync::watch;

use super::ambient::Ambient;
use super::driver::{Driver, Settings};
use super::{EpochPolicy, Farm};
use crate::action_usage::Shapes;
use crate::farm_js::open::{self, KbfObserver};
use crate::farm_js::usage::Usage;
use crate::farm_js::{KbfAdapter, Wall};
use crate::learned::{Learned, LearnedSizes};
use crate::state_store::StateStore;

pub(crate) mod hot;
pub(crate) mod lifecycle;
pub(crate) mod load;
pub(crate) mod views;

#[cfg(test)]
mod tests;

/// The state store of a farm that has none: it refuses every call. The core is built
/// without a writer then, so it is never asked.
#[derive(Debug)]
struct Detached;

impl Detached {
    fn refuse<T>() -> Result<T, StoreError> {
        Err(StoreError::Failed("the farm has no state store".to_owned()))
    }
}

impl Store for Detached {
    fn apply_batch(&self, _batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Self::refuse()
    }

    fn get(&self, _key: &str) -> Result<Option<Versioned>, StoreError> {
        Self::refuse()
    }

    fn scan_prefix(&self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        Self::refuse()
    }

    fn scan_range(&self, _scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        Self::refuse()
    }

    fn changes(&self, _after: Revision, _limit: usize) -> Result<Changes, StoreError> {
        Self::refuse()
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Self::refuse()
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats::default()
    }

    fn now(&self) -> StoreTime {
        StoreTime::ZERO
    }
}

/// What the js core of a farm is made of, and what the groups of methods read.
pub(crate) struct JsFarm {
    /// The loop around the scheduler. It takes its state store at construction, so a farm
    /// that takes one over later ([`Farm::with_state`]) builds it again.
    pub(crate) core: Core<KbfAdapter>,
    /// The farm's settings as the core was last built from them: the memory ladder and
    /// floors, the placement mode, the whole-node settings, the recorder, the held cap and
    /// the orphan wait, and the learned sizes mode, which the adapter's estimator is built
    /// from (`on` learns as `shadow`).
    pub(crate) settings: Settings,
    /// The state store the core opened, if the farm has taken one.
    pub(crate) store: Option<Arc<dyn Store>>,
    /// The cache the farm answers through, whose metrics the views read (for one,
    /// `cache.metrics().execute.open_executions`).
    pub(crate) cache: Backend,
    /// The count of intake polls that ran to the end, as [`Farm::intake_polls`] shows it.
    /// The intake poll sends the new count.
    pub(crate) polls: watch::Sender<u64>,
    /// How long queued work no live worker can run waits before it is refused.
    pub(crate) unservable_wait: Duration,
    /// How long a finished operation is kept.
    pub(crate) finished_retention: Duration,
    /// The term the core was opened under: the store's epoch once the farm has a store.
    pub(crate) term: u64,
    /// Where the wall clock and the term of a new incarnation come from.
    pub(crate) ambient: Ambient,
    /// When the farm was built, on tokio's clock: farm time zero.
    pub(crate) epoch: tokio::time::Instant,
    /// The wall-clock time of `epoch`, in milliseconds since the Unix epoch.
    pub(crate) epoch_unix_ms: u64,
    /// The action shapes this process has described.
    pub(crate) shapes: Shapes,
    /// `Result`s being taken now: a handover waits for none to be.
    pub(crate) reports: AtomicUsize,
    /// Lets one writer of size summaries at a time take its rows and write them.
    pub(crate) learning_gate: crate::farm_js::Gate,
    /// Whether the farm is fenced ([`Farm::fence`](crate::farm::Farm::fence)), which the
    /// core does not say. A fenced core feeds nothing, so it refuses every `Result` as
    /// stale, the current lease's too.
    pub(crate) fenced: AtomicBool,
    /// What the farm learns from the runs it hears of, when `--usage-profiles` is not `off`
    /// and the farm has a state store.
    pub(crate) usage: Option<Arc<Usage>>,
}

/// The wall minute `ambient` says it is.
pub(crate) fn wall_minute(ambient: Ambient) -> u64 {
    let since = ambient
        .wall()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    since.as_secs() / 60
}

/// The usage side `settings` ask for over `store`: none for `off` or no store. A store whose
/// profiles cannot be read at the start leaves the farm without it, logged: learning is not
/// worth a farm that does not start.
fn open_usage(
    store: Option<&Arc<dyn Store>>,
    settings: &Settings,
    ambient: Ambient,
) -> Option<Arc<Usage>> {
    let store = store.filter(|_| settings.usage.learns())?;
    match Usage::open(Arc::clone(store), wall_minute(ambient)) {
        Ok(usage) => Some(Arc::new(usage)),
        Err(error) => {
            tracing::error!(%error, "the usage profiles cannot be read: the farm runs without recording them");
            None
        }
    }
}

impl std::fmt::Debug for JsFarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsFarm")
            .field("term", &self.term)
            .field("store", &self.store.is_some())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// What a core is built from.
struct Opening<'a> {
    cache: &'a Backend,
    settings: &'a Settings,
    unservable_wait: Duration,
    finished_retention: Duration,
    ambient: Ambient,
    /// The scheduler term of a core with no store, and the name of this frontend in a store
    /// that has none.
    term: u64,
    /// The farm time of the start.
    now: FarmTime,
    /// The wall-clock time the farm was built at, in milliseconds since the Unix epoch.
    epoch_unix_ms: u64,
    /// What the adapter tells the profile name of each action.
    usage: Option<Arc<Usage>>,
}

impl Opening<'_> {
    /// The core's configuration. Under `--usage-profiles on` it carries the curves of this
    /// opening's usage side, the one the farm keeps ([`JsFarm::rebuild`] and
    /// [`JsFarm::keep_in`] open the usage side once and give the same one to the core and to the
    /// farm), so the core reads what the farm feeds.
    fn config(&self) -> js_server_core::Config {
        open::farm_config(
            self.term,
            self.unservable_wait,
            self.finished_retention,
            self.settings,
            self.usage.as_deref(),
        )
    }

    /// The estimator the settings ask for. `on` is not supported on this core: it learns as
    /// `shadow` does and books what `off` books. `--placement-predictions` is read as it is.
    fn learned(&self) -> Learned {
        let mode = match self.settings.learned {
            LearnedSizes::On => LearnedSizes::Shadow,
            mode => mode,
        };
        Learned::new(mode, self.settings.default_memory, self.epoch_unix_ms)
            .with_predictions(self.settings.predictions)
    }

    fn adapter(&self, store: Arc<dyn Store>, stored: bool) -> KbfAdapter {
        let ambient = self.ambient;
        let wall: Wall = Arc::new(move || ambient.wall());
        KbfAdapter::new(self.term, self.cache.clone(), store, wall)
            .recording(self.settings.recorder.clone())
            .noting_usage(self.usage.clone())
            .learning_with(self.learned(), stored)
    }

    /// A core with no state store: it writes nothing.
    fn bare(&self) -> Core<KbfAdapter> {
        let observer = KbfObserver::new(self.cache.clone());
        Core::new(&self.config(), self.adapter(Arc::new(Detached), false))
            .with_observer(observer)
            .with_held_cap(self.settings.held_cap)
    }

    /// A core over `store`, which takes up what it holds.
    fn over(
        &self,
        store: Arc<dyn Store>,
        epoch: EpochPolicy,
    ) -> Result<Core<KbfAdapter>, js_store::StateError> {
        let adapter = self.adapter(Arc::clone(&store), true);
        let core = open::open(&self.config(), adapter, store, epoch, self.now)?;
        Ok(core.with_held_cap(self.settings.held_cap))
    }
}

impl JsFarm {
    /// Farm time: milliseconds since the farm was built.
    pub(crate) fn now(&self) -> FarmTime {
        let millis = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        FarmTime::from_millis(millis)
    }

    fn opening<'a>(
        &'a self,
        settings: &'a Settings,
        term: u64,
        usage: Option<Arc<Usage>>,
    ) -> Opening<'a> {
        Opening {
            cache: &self.cache,
            settings,
            unservable_wait: self.unservable_wait,
            finished_retention: self.finished_retention,
            ambient: self.ambient,
            term,
            now: self.now(),
            epoch_unix_ms: self.epoch_unix_ms,
            usage,
        }
    }

    /// Builds the core again from `settings`, over the store it opened if it has one (whose
    /// epoch it keeps). What the core holds is lost, so a builder runs before the farm serves.
    pub(crate) fn rebuild(&mut self, settings: &Settings) {
        let usage = open_usage(self.store.as_ref(), settings, self.ambient);
        let opening = self.opening(settings, self.term, usage.clone());
        let built = match &self.store {
            Some(store) => opening.over(Arc::clone(store), EpochPolicy::Keep),
            None => Ok(opening.bare()),
        };
        match built {
            Ok(core) => {
                self.core = core;
                self.usage = usage;
            }
            // The store opened a moment ago cannot be read: the core as it was keeps serving.
            Err(error) => {
                tracing::error!(%error, "the js core was not built again with the new settings");
            }
        }
        self.settings = settings.clone();
    }

    /// Sets the cap on the records the core holds for a store that refuses them, on the core
    /// as it is. [`JsFarm::rebuild`] would take a core over a store up again from the store,
    /// and lose what it holds only in memory: the records held back, which the cap counts.
    /// The core takes the cap by value, so a bare core stands in for it meanwhile.
    pub(crate) fn set_held_cap(&mut self, settings: &Settings) {
        let stand_in = self.opening(settings, self.term, None).bare();
        let core = std::mem::replace(&mut self.core, stand_in);
        self.core = core.with_held_cap(settings.held_cap);
        self.settings = settings.clone();
    }

    /// Takes `state` over: builds the core over its store, which decides the epoch. Returns
    /// the epoch the core opened under, which is the farm's term and lease epoch from now on;
    /// `None` for no store.
    ///
    /// # Errors
    /// The store cannot be read or written, or holds a row that is not one the scheduler
    /// wrote.
    pub(crate) fn keep_in(
        &mut self,
        state: Option<StateStore>,
        settings: &Settings,
    ) -> Result<Option<u64>, js_store::StateError> {
        let Some(StateStore { store, epoch, .. }) = state else {
            return Ok(None);
        };
        let usage = open_usage(Some(&store), settings, self.ambient);
        let core = self
            .opening(settings, self.ambient.term(), usage.clone())
            .over(Arc::clone(&store), epoch)?;
        let opened = core.epoch();
        self.core = core;
        self.usage = usage;
        self.store = Some(store);
        self.term = opened.unwrap_or(self.term);
        Ok(opened)
    }
}

impl Farm {
    /// A farm over `cache` on the js core, with an empty scheduler of a new term and no
    /// state store: what [`Farm::new`] builds where [`crate::SchedulerCore`] names the js
    /// core.
    pub(super) fn new_js(
        cache: Backend,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        let (term, lease_epoch) = Self::terms_of(&cache);
        let epoch = tokio::time::Instant::now();
        let epoch_unix_ms = super::unix_ms();
        let settings = Settings::default();
        let ambient = Ambient::Process;
        let core = Opening {
            cache: &cache,
            settings: &settings,
            unservable_wait,
            finished_retention,
            ambient,
            term,
            now: FarmTime::from_millis(0),
            epoch_unix_ms,
            usage: None,
        }
        .bare();
        let js = JsFarm {
            core,
            settings: settings.clone(),
            store: None,
            cache: cache.clone(),
            polls: watch::channel(0).0,
            unservable_wait,
            finished_retention,
            term,
            ambient,
            epoch,
            epoch_unix_ms,
            shapes: Shapes::default(),
            reports: AtomicUsize::new(0),
            learning_gate: crate::farm_js::Gate::default(),
            fenced: AtomicBool::new(false),
            usage: None,
        };
        Self {
            cache,
            term,
            lease_epoch,
            epoch,
            epoch_unix_ms,
            shapes: Shapes::default(),
            reports: AtomicUsize::new(0),
            refusals: crate::worker_refusals::Refusals::default(),
            ambient,
            settings,
            usage_profiles: crate::UsageProfiles::default(),
            driver: Driver::Js(Box::new(js)),
        }
    }
}
