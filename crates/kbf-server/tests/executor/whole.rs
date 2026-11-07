//! A whole-machine action (`kbf-lease=whole_machine`) through the real farm: the farm books a
//! virtual worker whole for it, and the executor submits it as one job of the whole host.

use js_proto::v1::{Needs, StartFailureCode};
use kbf_front::Finished;
use kbf_types::LeaseKind;

use crate::rig::{Rig, submits};

const HOST: &str = "host-1";

fn whole_host() -> Option<Needs> {
    Some(Needs {
        whole_host: true,
        ..Needs::default()
    })
}

/// Catches a whole-machine action that the farm never gives the executor, that is submitted
/// as a share of a host, or that shares its worker: the farm places it on the one worker, which
/// it books whole, and the executor submits one job that needs the whole host and nothing
/// beside it; an ordinary action submitted after it is not placed until the whole-host job has
/// ended, and is then a shared job. Both run, and both callers are answered.
#[tokio::test(start_paused = true)]
async fn the_farm_places_a_whole_machine_action_on_a_worker_it_books_whole() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let whole = rig.submit_kind(1, LeaseKind::WholeMachine);
    rig.pump().await;
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].spec.needs, whole_host());

    let shared = rig.submit(2);
    rig.pump().await;
    assert!(
        rig.jobs_of(2).is_empty(),
        "an action was placed beside a whole-machine one: {:?}",
        rig.jobs()
    );
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&whole, 8).await, Finished::Ran(_)));
    rig.pump().await;
    let after = rig.jobs_of(2);
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(after[0].spec.needs.as_ref().is_some_and(|n| !n.whole_host));
    rig.host_finishes(after[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&shared, 8).await, Finished::Ran(_)));
}

/// Catches a whole-host job whose host the service could not ready being answered to the
/// caller as the action's failure: `exclusive-not-ready` is the farm's fault, so the action
/// runs again as a new whole-host job, and the caller is answered by that one.
#[tokio::test(start_paused = true)]
async fn a_whole_host_job_whose_host_was_not_readied_is_rerun_as_another() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let whole = rig.submit_kind(1, LeaseKind::WholeMachine);
    rig.pump().await;
    let first = rig.jobs_of(1);
    assert_eq!(first.len(), 1, "{first:?}");
    let id = first[0].id().to_owned();
    assert!(rig.fake.place(&id, HOST));
    assert!(
        rig.fake
            .fail_to_start(&id, StartFailureCode::ExclusiveNotReady)
    );
    rig.pump().await;
    assert!(whole.finished().is_none(), "answered at the failed start");
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "no rerun: {jobs:?}");
    assert_ne!(jobs[1].id(), id);
    assert_eq!(jobs[1].spec.needs, whole_host());
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert!(matches!(rig.until_done(&whole, 8).await, Finished::Ran(_)));
    assert_eq!(submits(&rig.fake.calls()).len(), 2);
}
