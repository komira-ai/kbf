//! The engine a [`Farm`](super::Farm) runs on, and the settings its `with_*` builders take.
//!
//! A farm's methods reach its engine through [`Driver`], so a second engine is a new
//! variant and one new arm in the methods that differ, not a change to every method.
//! The farm's own scheduler, engine and side under one lock is [`Driver::Legacy`]. The
//! second is [`Driver::Js`]: the core of `crate::farm_js`.
//!
//! A method of the farm that differs by engine starts with [`js_arm!`], which returns the
//! result of the matching function in `farm::js` when the farm runs on the second engine.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_front::Recorder;
use kbf_sched::{MEMORY_FLOORS, MemoryLadder, Scheduler, WholeNodeConfig};

use super::State;
use super::durable::ORPHAN_WAIT;
use super::engine::HELD_CAP;
use super::js::JsFarm;
use crate::learned::{Learned, LearnedSizes};
use crate::placement_load::PlacementLoad;
use crate::placement_predictions::PlacementPredictions;

/// Returns from the enclosing method with `farm::js::<group>::<method>(js, args..)` when
/// the farm runs on the js core: `js_arm!(self, hot::resize(worker, stream, reported))`.
macro_rules! js_arm {
    ($farm:expr, $group:ident :: $method:ident ( $($arg:expr),* $(,)? )) => {
        if let $crate::farm::driver::Driver::Js(js) = &$farm.driver {
            return $crate::farm::js::$group::$method(js $(, $arg)*);
        }
    };
}

/// [`js_arm!`] for an `async fn`: awaits the function of the js core.
macro_rules! js_arm_async {
    ($farm:expr, $group:ident :: $method:ident ( $($arg:expr),* $(,)? )) => {
        if let $crate::farm::driver::Driver::Js(js) = &$farm.driver {
            return $crate::farm::js::$group::$method(js $(, $arg)*).await;
        }
    };
}

/// Returns from the enclosing [`Driver`] method once the js core is built again from
/// `settings`, if the driver is the js core: its builders change the core's configuration,
/// which the core takes at construction.
macro_rules! js_rebuilt {
    ($driver:expr, $settings:expr) => {
        if $driver.rebuilt($settings) {
            return;
        }
    };
}

/// What a farm's `with_*` builders set: the values an engine is configured from, kept
/// whichever engine the farm runs on.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// How long a lease whose node has no session row waits before the intake poll requeues
    /// it ([`Farm::with_orphan_wait`](super::Farm::with_orphan_wait)).
    pub orphan_wait: Duration,
    /// The memory ladder an action that passes its limit climbs.
    pub ladder: MemoryLadder,
    /// Whether an own-limit kill climbs the ladder (`--no-memory-climb` is `false`). Only the
    /// legacy scheduler is told: the js core builds its own, so a startup check refuses the
    /// flag with it.
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
    /// What the farm learns from the runs it hears of (`--usage-profiles`). Only the js core
    /// learns; the flag is refused with the legacy core.
    pub usage: kbf_usage::UsageMode,
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
            usage: kbf_usage::UsageMode::Off,
        }
    }
}

/// The engine state of a farm.
// A farm holds one driver for its whole life, so the legacy state, which is the large
// variant, is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(super) enum Driver {
    /// The farm's scheduler, engine and side, under the one lock every operation takes.
    Legacy(Mutex<State>),
    /// The core of `crate::farm_js` and the pieces around it (`farm::js`).
    Js(Box<JsFarm>),
}

impl Driver {
    /// A driver over `state`.
    pub(super) const fn legacy(state: State) -> Self {
        Self::Legacy(Mutex::new(state))
    }

    /// The core this driver is.
    pub(super) const fn core(&self) -> crate::SchedulerCore {
        match self {
            Self::Legacy(_) => crate::SchedulerCore::Legacy,
            Self::Js(_) => crate::SchedulerCore::Js,
        }
    }

    /// The engine state, locked. A farm on the js core has none: each of its methods
    /// reaches [`Driver::Js`] first ([`js_arm!`]).
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        match self {
            // Nothing under the lock panics midway through a change.
            Self::Legacy(state) => state.lock().unwrap_or_else(PoisonError::into_inner),
            Self::Js(_) => unreachable!("a farm on the js core has no engine lock"),
        }
    }

    /// The engine state, for a farm nothing else holds yet.
    pub(super) fn state_mut(&mut self) -> &mut State {
        match self {
            Self::Legacy(state) => state.get_mut().unwrap_or_else(PoisonError::into_inner),
            Self::Js(_) => unreachable!("a farm on the js core has no engine state"),
        }
    }

    /// Opens the core again over `settings` if this is the js core, and says whether it is.
    fn rebuilt(&mut self, settings: &Settings) -> bool {
        match self {
            Self::Js(js) => {
                js.rebuild(settings);
                true
            }
            Self::Legacy(_) => false,
        }
    }

    /// Applies `settings.ladder`.
    pub(super) fn set_ladder(&mut self, settings: &Settings) {
        js_rebuilt!(self, settings);
        self.state_mut()
            .engine
            .sched
            .set_memory_ladder(settings.ladder.clone());
    }

    /// Applies `settings.climb` to the legacy scheduler.
    pub(super) fn set_climb(&mut self, settings: &Settings) {
        if matches!(self, Self::Legacy(_)) {
            self.state_mut()
                .engine
                .sched
                .set_memory_climb(settings.climb);
        }
    }

    /// Applies `settings.floors` to the scheduler of `term`.
    pub(super) fn set_floors(&mut self, term: u64, settings: &Settings) {
        js_rebuilt!(self, settings);
        let sched = &mut self.state_mut().engine.sched;
        *sched = std::mem::replace(sched, Scheduler::new(term)).with_memory_floors(settings.floors);
    }

    /// Applies `settings.placement_load` to the scheduler of `term`.
    pub(super) fn set_placement_load(&mut self, term: u64, settings: &Settings) {
        js_rebuilt!(self, settings);
        let sched = &mut self.state_mut().engine.sched;
        // The scheduler is moved out and back; the stand-in is dropped at once.
        *sched = std::mem::replace(sched, Scheduler::new(term))
            .with_load_config(settings.placement_load.config());
    }

    /// Applies `settings.learned` and `settings.default_memory`, for a farm built at
    /// `epoch_unix_ms`.
    pub(super) fn set_learned(&mut self, epoch_unix_ms: u64, settings: &Settings) {
        js_rebuilt!(self, settings);
        self.state_mut().side.learned =
            Learned::new(settings.learned, settings.default_memory, epoch_unix_ms)
                .with_predictions(settings.predictions);
    }

    /// Applies `settings.predictions`.
    pub(super) fn set_predictions(&mut self, settings: &Settings) {
        js_rebuilt!(self, settings);
        self.state_mut()
            .side
            .learned
            .set_predictions(settings.predictions);
    }

    /// Applies `settings.whole_node` to the scheduler of `term`. Called once it is set.
    pub(super) fn set_whole_node(&mut self, term: u64, settings: &Settings) {
        js_rebuilt!(self, settings);
        let sched = &mut self.state_mut().engine.sched;
        settings.whole_node.iter().for_each(|config| {
            *sched = std::mem::replace(sched, Scheduler::new(term)).with_whole_node(config.clone());
        });
    }

    /// Applies `settings.recorder`.
    pub(super) fn set_recorder(&mut self, settings: &Settings) {
        js_rebuilt!(self, settings);
        self.state_mut()
            .side
            .recording
            .tell(settings.recorder.clone());
    }

    /// Applies `settings.orphan_wait`.
    pub(super) fn set_orphan_wait(&mut self, settings: &Settings) {
        js_rebuilt!(self, settings);
        self.state_mut().engine.intake.orphan_wait = settings.orphan_wait;
    }

    /// Applies `settings.usage`: the js core opens its usage side again over the store it
    /// has, and the legacy core learns nothing.
    pub(super) fn set_usage(&mut self, settings: &Settings) {
        js_rebuilt!(self, settings);
    }

    /// Applies `settings.held_cap`, to the core as it is on either driver: the cap changes
    /// nothing the core was built from.
    pub(super) fn set_held_cap(&mut self, settings: &Settings) {
        if let Self::Js(js) = self {
            js.set_held_cap(settings);
            return;
        }
        self.state_mut().engine.held_cap = settings.held_cap;
    }
}
