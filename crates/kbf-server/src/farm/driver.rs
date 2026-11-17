//! The engine state a [`Farm`](super::Farm) runs on, and the settings its `with_*` builders take.
//!
//! A farm's methods reach its engine state through [`Driver`]: the farm's own scheduler,
//! engine and side under one lock.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_front::Recorder;
use kbf_sched::{MEMORY_FLOORS, MemoryLadder, Scheduler, WholeNodeConfig};

use super::State;
use super::durable::ORPHAN_WAIT;
use super::engine::HELD_CAP;
use crate::learned::{Learned, LearnedSizes};
use crate::placement_load::PlacementLoad;
use crate::placement_predictions::PlacementPredictions;

/// What a farm's `with_*` builders set: the values the engine is configured from.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// How long a lease whose node has no session row waits before the intake poll requeues
    /// it ([`Farm::with_orphan_wait`](super::Farm::with_orphan_wait)).
    pub orphan_wait: Duration,
    /// The memory ladder an action that passes its limit climbs.
    pub ladder: MemoryLadder,
    /// Whether an own-limit kill climbs the ladder (`--no-memory-climb` is `false`).
    pub climb: bool,
    /// How many memory floors are kept.
    pub floors: usize,
    /// How placement uses the load (`--placement-load`).
    pub placement_load: PlacementLoad,
    /// What learned sizes do (`--learned-sizes`).
    pub learned: LearnedSizes,
    /// Whether the scheduler is given predictions (`--placement-predictions`).
    pub predictions: PlacementPredictions,
    /// The first rung of the ladder, in bytes, learned sizes start from.
    pub default_memory: u64,
    /// The whole-node drain and caps (`--whole-node-max`), if set.
    pub whole_node: Option<WholeNodeConfig>,
    /// Where the invocation record goes.
    pub recorder: Recorder,
    /// Held-back committed records kept for a refusing state store at most.
    pub held_cap: usize,
}

impl Default for Settings {
    /// The settings of a farm no builder has touched.
    fn default() -> Self {
        Self {
            orphan_wait: ORPHAN_WAIT,
            ladder: MemoryLadder::default(),
            climb: true,
            floors: MEMORY_FLOORS,
            placement_load: PlacementLoad::Off,
            learned: LearnedSizes::Off,
            predictions: PlacementPredictions::Off,
            default_memory: 0,
            whole_node: None,
            recorder: Recorder::default(),
            held_cap: HELD_CAP,
        }
    }
}

/// The engine state of a farm: its scheduler, engine and side, under the one lock every
/// operation takes.
#[derive(Debug)]
pub(super) struct Driver(Mutex<State>);

impl Driver {
    /// A driver over `state`.
    pub(super) const fn legacy(state: State) -> Self {
        Self(Mutex::new(state))
    }

    /// The engine state, locked.
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing under the lock panics midway through a change.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The engine state, for a farm nothing else holds yet.
    pub(super) fn state_mut(&mut self) -> &mut State {
        self.0.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies `settings.ladder`.
    pub(super) fn set_ladder(&mut self, settings: &Settings) {
        self.state_mut()
            .engine
            .sched
            .set_memory_ladder(settings.ladder.clone());
    }

    /// Applies `settings.climb`.
    pub(super) fn set_climb(&mut self, settings: &Settings) {
        self.state_mut()
            .engine
            .sched
            .set_memory_climb(settings.climb);
    }

    /// Applies `settings.floors` to the scheduler of `term`.
    pub(super) fn set_floors(&mut self, term: u64, settings: &Settings) {
        let sched = &mut self.state_mut().engine.sched;
        *sched = std::mem::replace(sched, Scheduler::new(term)).with_memory_floors(settings.floors);
    }

    /// Applies `settings.placement_load` to the scheduler of `term`.
    pub(super) fn set_placement_load(&mut self, term: u64, settings: &Settings) {
        let sched = &mut self.state_mut().engine.sched;
        // The scheduler is moved out and back; the stand-in is dropped at once.
        *sched = std::mem::replace(sched, Scheduler::new(term))
            .with_load_config(settings.placement_load.config());
    }

    /// Applies `settings.learned` and `settings.default_memory`, for a farm built at
    /// `epoch_unix_ms`.
    pub(super) fn set_learned(&mut self, epoch_unix_ms: u64, settings: &Settings) {
        self.state_mut().side.learned =
            Learned::new(settings.learned, settings.default_memory, epoch_unix_ms)
                .with_predictions(settings.predictions);
    }

    /// Applies `settings.predictions`.
    pub(super) fn set_predictions(&mut self, settings: &Settings) {
        self.state_mut()
            .side
            .learned
            .set_predictions(settings.predictions);
    }

    /// Applies `settings.whole_node` to the scheduler of `term`. Called once it is set.
    pub(super) fn set_whole_node(&mut self, term: u64, settings: &Settings) {
        let sched = &mut self.state_mut().engine.sched;
        settings.whole_node.iter().for_each(|config| {
            *sched = std::mem::replace(sched, Scheduler::new(term)).with_whole_node(config.clone());
        });
    }

    /// Applies `settings.recorder`.
    pub(super) fn set_recorder(&mut self, settings: &Settings) {
        self.state_mut()
            .side
            .recording
            .tell(settings.recorder.clone());
    }

    /// Applies `settings.orphan_wait`.
    pub(super) fn set_orphan_wait(&mut self, settings: &Settings) {
        self.state_mut().engine.intake.orphan_wait = settings.orphan_wait;
    }

    /// Applies `settings.held_cap`.
    pub(super) fn set_held_cap(&mut self, settings: &Settings) {
        self.state_mut().engine.held_cap = settings.held_cap;
    }
}
