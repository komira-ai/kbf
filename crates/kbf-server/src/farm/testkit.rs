//! A farm built from the pieces a test chooses, for runs that are written down and
//! compared: [`Farm::for_test`]. The pieces are the scheduler core, the term, the lease
//! epoch and the wall clock; [`Farm::new`] takes none of them (a new term from the
//! clock and the operating system's randomness), and has the same body.

use std::time::Duration;

use kbf_front::Backend;
use kbf_sched::{LOSS_REQUEUES, Scheduler};

use super::{Ambient, Farm, Parts};

/// The pieces of a farm [`Farm::for_test`] takes in place of what [`Farm::new`] reads
/// from the process.
#[derive(Debug)]
pub struct TestCore {
    /// The term the farm grants leases under, and its lease epoch. A farm that takes a
    /// state store over ([`Farm::with_state`]) takes the store's epoch instead, which a
    /// new store seeds with this.
    pub term: u64,
    /// The scheduler the farm drives, for the term `term`. The farm turns on its record
    /// of the leases it gives up, which it reads.
    pub sched: Scheduler,
    /// How long the farm keeps a finished operation, which the scheduler was also given.
    pub finished_retention: Duration,
    /// The wall clock when the farm is built, in milliseconds since the Unix epoch. It
    /// moves with tokio's clock from then on.
    pub wall_unix_ms: u64,
}

impl TestCore {
    /// The scheduler [`Farm::new`] builds for `term`, refusing queued work no live worker
    /// can run after `unservable_wait`, with a wall clock that starts at `wall_unix_ms`.
    #[must_use]
    pub fn new(
        term: u64,
        wall_unix_ms: u64,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        let sched = Scheduler::new(term)
            .with_load_config(crate::PlacementLoad::Off.config())
            .with_unservable_wait(unservable_wait)
            .with_finished_retention(finished_retention)
            .with_loss_requeues(LOSS_REQUEUES);
        Self {
            term,
            sched,
            finished_retention,
            wall_unix_ms,
        }
    }
}

impl Farm {
    /// [`Farm::new`] on `core`, whatever core [`crate::SchedulerCore::under_test`] names: for
    /// a test that runs the two cores over one state store. In a build without the
    /// `farm-core-js` feature the js core does not exist, and `Js` builds the legacy farm.
    #[must_use]
    pub fn new_on_core(
        core: crate::SchedulerCore,
        cache: impl Into<Backend>,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        Self::on_core(core, cache.into(), unservable_wait, finished_retention)
    }

    /// A farm over `cache` that drives `core`, and reads the wall clock and the term of a
    /// new incarnation from it instead of from the process, so what it does depends on
    /// the calls made and on tokio's clock alone. The cache's metadata log is not asked
    /// for a term: `core` names it. Everything else is as [`Farm::new`] made it.
    #[must_use]
    pub fn for_test(cache: impl Into<Backend>, core: TestCore) -> Self {
        let epoch = tokio::time::Instant::now();
        Self::assemble(
            Parts {
                cache: cache.into(),
                term: core.term,
                lease_epoch: core.term,
                sched: core.sched.recording_requeues(),
                finished_retention: core.finished_retention,
                ambient: Ambient::Fixed {
                    term: core.term,
                    started: epoch,
                    unix_ms: core.wall_unix_ms,
                },
                epoch_unix_ms: core.wall_unix_ms,
            },
            epoch,
        )
    }
}
