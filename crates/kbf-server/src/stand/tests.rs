//! What a retire stops, in the order it stops it, and what the stand counts as open.

use futures::future::{AbortHandle, Abortable};
use kbf_front::{Backend, Cache};

use super::*;
use crate::health::Readiness;

/// A serving frontend's parts, as a retire stands them down.
struct Rig {
    backend: Backend,
    drain: Arc<Drain>,
    /// The drain's readiness, which a retire marks handed over and then not ready.
    readiness: Arc<Readiness>,
    worker: Stop,
    sweeper: AbortHandle,
    /// The task the sweeper handle ends.
    sweeping: Option<futures::future::AbortRegistration>,
}

impl Rig {
    fn new() -> Self {
        let backend = Backend::from(Arc::new(Cache::memory()));
        let (closer, _closing) = kbf_front::closing();
        let readiness = Arc::new(Readiness::default());
        let (drain, _goaway) = Drain::new(closer, Arc::clone(&readiness));
        let (sweeper, sweeping) = AbortHandle::new_pair();
        Self {
            backend,
            drain: Arc::new(drain),
            readiness,
            worker: Stop::new(),
            sweeper,
            sweeping: Some(sweeping),
        }
    }

    fn stand(&self) -> Stand {
        Stand {
            drain: Arc::clone(&self.drain),
            backend: self.backend.clone(),
            worker: self.worker.clone(),
            sweeper: self.sweeper.clone(),
        }
    }
}

/// Asserts that a retire's stand-down does all four steps: the drain hands over, the
/// sweeper's task is ended, the worker listener's stop is given, and the frontend goes not
/// ready. Before it none of them has happened.
#[tokio::test]
async fn release_hands_over_ends_the_sweeper_stops_the_worker_listener_and_goes_not_ready() {
    let mut rig = Rig::new();
    assert!(!rig.readiness.is_handed_over());
    assert!(!rig.drain.is_unready());
    let registration = rig.sweeping.take().expect("a registration");
    let sweeping = Abortable::new(std::future::pending::<()>(), registration);
    let worker = rig.worker.fired();

    rig.stand().release().await;

    assert!(rig.readiness.is_handed_over());
    assert!(rig.drain.is_unready());
    assert!(sweeping.await.is_err(), "the sweeper's task was ended");
    worker.await;
}

/// Asserts that a stand-down run twice (a retire after a drain's end) is harmless: the
/// steps are idempotent and the frontend stays not ready.
#[tokio::test]
async fn release_twice_leaves_the_frontend_not_ready() {
    let rig = Rig::new();
    rig.stand().release().await;
    rig.stand().release().await;
    assert!(rig.readiness.is_handed_over());
    assert!(rig.drain.is_unready());
}

/// Asserts that the stand reports the Execute streams open on the front's store: none when
/// nothing is open, one per stream held, and back to none when it is dropped.
#[test]
fn open_executions_counts_the_streams_the_front_holds() {
    let rig = Rig::new();
    let stand = rig.stand();
    assert_eq!(stand.open_executions(), 0);
    let open = rig.backend.metrics().execute.open_executions.raise();
    assert_eq!(stand.open_executions(), 1);
    drop(open);
    assert_eq!(stand.open_executions(), 0);
}
