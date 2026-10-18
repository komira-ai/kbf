//! The controller, run by the frontend: [`spawn`] starts a task that looks once per `every`
//! (the farm's tick) when the server has a state store to keep the rows in.
//!
//! **Nothing runs unasked.** The controller does not read `update/policy.auto_nodes`; this
//! does: with it false (the default) the controller still moves hosts already in a step
//! (so turning the switch off does not strand a cordoned host) and starts none
//! ([`Controller::tick_gated`]). There is no operator trigger yet that starts a run with
//! `auto_nodes` off.
//!
//! The controller blocks on the store, so each look is a blocking task. Its errors are logged
//! once while they last.
//!
//! **Two ways the controller stops ordering.** While the farm is handed over (fenced, as a
//! quiesce or a stand-down does: [`Handed::handed`]) no look is made: such a farm is the
//! successor's, and a store handle that is not yet superseded would still let the
//! controller move host rows and cordon hosts. [`SessionTransport`](super::SessionTransport)
//! refuses the orders of a look that was already under way when the farm was fenced. And the
//! task ends when a look finds its store handle superseded
//! ([`StoreError::Fenced`](kbf_state::StoreError::Fenced)): no later look could write. Neither
//! replaces the store's generation fence, which stays what refuses a superseded controller's
//! writes; these close the time between a farm being fenced and its handle being superseded.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::Handed;
use kbf_state::{Store, StoreError};
use tokio::task::AbortHandle;

use kbf_types::FarmTime;

use super::session::ports;
use super::{Controller, ControllerError, RowError, Rows, Tick};
use crate::farm::Farm;

/// Starts the controller over `store` on the current runtime; the returned handle stops it.
/// `None` without a state store. The controller's epoch is the farm's: the one `Welcome`
/// names.
#[must_use]
pub fn spawn(
    farm: Arc<Farm>,
    store: Option<Arc<dyn Store>>,
    every: Duration,
) -> Option<AbortHandle> {
    store.map(|store| tokio::spawn(run(farm, store, every)).abort_handle())
}

async fn run(farm: Arc<Farm>, store: Arc<dyn Store>, every: Duration) {
    let rows = Rows::new(Arc::clone(&store));
    let (transport, source) = ports(Arc::clone(&farm), rows.clone());
    let mut controller = Controller::new(
        store,
        farm.lease_epoch(),
        Box::new(transport),
        Box::new(source),
    );
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing: Option<String> = None;
    let mut waiting = false;
    loop {
        ticker.tick().await;
        if farm.handed() {
            // Said once for as long as the farm stays handed over, not at every tick.
            if !std::mem::replace(&mut waiting, true) {
                tracing::info!("the update controller makes no look: this farm is handed over");
            }
            continue;
        }
        waiting = false;
        let Some((next, tick)) = step(controller, rows.clone(), farm.wall_now()).await else {
            tracing::error!("the update controller stopped: a look panicked");
            return;
        };
        controller = next;
        if superseded(&tick) {
            tracing::warn!("the update controller stopped: a newer writer holds the state store");
            return;
        }
        log(&tick, &mut failing);
    }
}

/// Whether a look found the controller's store handle superseded: a write (or the writer
/// check before an order) was refused because a newer writer holds the state store. Any
/// other store error is the store's trouble, and the look is made again.
fn superseded(tick: &Result<Tick, ControllerError>) -> bool {
    matches!(
        tick,
        Err(ControllerError::Rows(RowError::Store(StoreError::Fenced)))
    )
}

/// One look on a blocking task; `None` if it panicked (the controller is lost with it).
async fn step(
    mut controller: Controller,
    rows: Rows,
    now: FarmTime,
) -> Option<(Controller, Result<Tick, ControllerError>)> {
    tokio::task::spawn_blocking(move || {
        // An unreadable policy starts nothing: the controller reads it again and says why.
        let auto = rows
            .policy_if_set()
            .ok()
            .flatten()
            .is_some_and(|p| p.auto_nodes);
        let tick = controller.tick_gated(now, auto);
        (controller, tick)
    })
    .await
    .ok()
}

/// Logs what a look did; an error only when it differs from the one before.
fn log(tick: &Result<Tick, ControllerError>, failing: &mut Option<String>) {
    match tick {
        Ok(tick) => {
            *failing = None;
            for moved in &tick.moved {
                tracing::info!(host = %moved.host, from = ?moved.from, to = ?moved.to, "update: host moved");
            }
            for error in &tick.errors {
                tracing::warn!(host = %error.host, "update: an order did not go: {}", error.error);
            }
        }
        Err(e) => {
            if failing.as_deref() != Some(&e.to_string()) {
                tracing::warn!("update controller: {e}");
                *failing = Some(e.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kbf_types::WorkerId;

    use super::super::command::Command;
    use super::super::node_step::HostPhase;
    use super::super::ports::{FactsSource, FarmView, SourceError, TransportError};
    use super::super::report::{CommandError, Moved};
    use super::*;

    struct Panics;

    impl FactsSource for Panics {
        fn look(&mut self, _now: FarmTime) -> Result<FarmView, SourceError> {
            panic!("a source that panics");
        }
    }

    /// Catches a look that panics taking the frontend with it, or going unnoticed: the step
    /// answers `None`, which stops the controller (and `run` says so).
    #[tokio::test]
    async fn a_look_that_panics_ends_the_controller() {
        let store: Arc<dyn Store> = Arc::new(kbf_state::fake::Fake::with_flaw(
            Arc::new(kbf_state::ManualClock::default()),
            kbf_state::fake::Flaw::None,
        ));
        let rows = Rows::new(Arc::clone(&store));
        rows.offer(&super::super::Release::new("a"), FarmTime::from_millis(1))
            .expect("a run");
        let farm = Arc::new(Farm::new(
            Arc::new(kbf_front::Cache::<
                kbf_front::MemoryMetaLog,
                kbf_objstore::MemoryStore,
            >::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ));
        let (transport, _) = ports(farm, rows.clone());
        let controller = Controller::new(store, 1, Box::new(transport), Box::new(Panics));
        assert!(
            step(controller, rows, FarmTime::from_millis(2))
                .await
                .is_none()
        );
    }

    /// Catches a controller that ends for a store error that passes: only the refusal of a
    /// superseded handle ends it, while any other store failure (the database down for a
    /// moment) is logged and the next look tries again. Mutant seen red: any store error
    /// taken for a superseded handle.
    #[test]
    fn only_a_superseded_store_handle_ends_the_controller() {
        let store = |e: StoreError| Err(ControllerError::Rows(RowError::Store(e)));
        assert!(superseded(&store(StoreError::Fenced)));
        assert!(!superseded(&store(StoreError::Failed("down".to_owned()))));
        assert!(!superseded(&Err(ControllerError::Invariant(
            "x".to_owned()
        ))));
        assert!(!superseded(&Ok(Tick::default())));
    }

    /// Catches an error logged at every look while it lasts (the same text is kept once),
    /// one that is never logged again after it cleared, and moves or refused orders that
    /// are not read.
    #[test]
    fn an_error_is_remembered_until_a_look_succeeds() {
        let mut failing = None;
        let error: Result<Tick, ControllerError> = Err(ControllerError::Invariant("x".into()));
        log(&error, &mut failing);
        assert_eq!(
            failing.as_deref(),
            Some("the controller broke its own rule: x")
        );
        log(&error, &mut failing);
        assert!(failing.is_some());
        let host = WorkerId::new("mac-1");
        let tick = Tick {
            moved: vec![Moved {
                host: host.clone(),
                from: HostPhase::Pending,
                to: HostPhase::Selected,
            }],
            errors: vec![CommandError {
                host,
                command: Command::Cordon,
                error: TransportError::new("down"),
            }],
            ..Tick::default()
        };
        log(&Ok(tick), &mut failing);
        assert_eq!(failing, None);
    }
}
