//! A whole-machine action (`kbf-lease=whole_machine`) through the real farm: the executor refuses
//! it, as a put directory cannot be mounted into a whole-host job. And a job whose host the
//! service could not ready is run again.

use js_proto::v1::StartFailureCode;
use kbf_front::Finished;
use kbf_types::LeaseKind;
use tonic::Code;

use crate::rig::{Rig, submits};

const HOST: &str = "host-1";

/// Catches a whole-machine action that reaches the job service or that blocks its worker: a put
/// directory is mounted into the job, and the job service gives a whole-host job no mount, so
/// the action is refused as the caller's (INVALID_ARGUMENT, naming `whole_machine`), nothing is
/// submitted and the farm does not run it again; an ordinary action submitted after it is placed
/// on the same worker, is one shared job (it does not need the whole host), and runs.
#[tokio::test(start_paused = true)]
async fn a_whole_machine_action_is_refused_to_its_caller_and_leaves_its_worker_free() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let whole = rig.submit_kind(1, LeaseKind::WholeMachine);
    let finished = rig.until_done(&whole, 3).await;
    let Finished::Failed(status) = &finished else {
        panic!("not refused: {finished:?}");
    };
    assert_eq!(Code::from_i32(status.code), Code::InvalidArgument);
    assert!(status.message.contains("whole_machine"), "{finished:?}");
    assert!(rig.jobs().is_empty(), "{:?}", rig.jobs());
    assert!(submits(&rig.fake.calls()).is_empty());

    let shared = rig.submit(2);
    rig.pump().await;
    let after = rig.jobs_of(2);
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(after[0].spec.needs.as_ref().is_some_and(|n| !n.whole_host));
    rig.host_finishes(after[0].id(), HOST, 0);
    assert!(matches!(rig.until_done(&shared, 8).await, Finished::Ran(_)));
}

/// Catches a job whose host the service could not ready being answered to the caller as the
/// action's failure: `exclusive-not-ready` is the farm's fault, so the action runs again as a
/// new job, and the caller is answered by that one.
#[tokio::test(start_paused = true)]
async fn a_job_whose_host_was_not_readied_is_rerun_as_another() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
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
    assert!(caller.finished().is_none(), "answered at the failed start");
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "no rerun: {jobs:?}");
    assert_ne!(jobs[1].id(), id);
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 8).await, Finished::Ran(_)));
    assert_eq!(submits(&rig.fake.calls()).len(), 2);
}
