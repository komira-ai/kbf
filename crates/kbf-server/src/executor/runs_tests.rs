use kbf_lease_client::ErrorCode;

use super::{Settled, pinned, require};
use crate::executor::traits::{InputsError, PresignError};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Catches an image the lease server would refuse (or, worse, run from a mutable tag) being
/// posted: only `name@sha256:<64 hex>` is taken, and the platform property's `docker://` is
/// dropped.
#[test]
fn an_image_must_be_pinned_by_digest() {
    let image = format!("registry.test/app@sha256:{DIGEST}");
    assert_eq!(pinned(&image).as_deref(), Ok(image.as_str()));
    assert_eq!(
        pinned(&format!("docker://{image}")).as_deref(),
        Ok(image.as_str())
    );
    let tagged = format!("registry.test/app:1.0@sha256:{DIGEST}");
    assert_eq!(pinned(&tagged).as_deref(), Ok(tagged.as_str()));
    for bad in [
        "registry.test/app:latest".to_owned(),
        "registry.test/app@sha256:abc".to_owned(),
        format!("registry.test/app@sha256:{}z", &DIGEST[1..]),
        format!("registry.test/app@sha512:{DIGEST}"),
        String::new(),
    ] {
        let Err(Settled::Invalid(why)) = pinned(&bad) else {
            panic!("{bad:?} was taken");
        };
        assert!(why.contains("not pinned"), "{why}");
    }
}

/// Catches a user error (an invalid action) being classed as the farm's, and the farm's
/// (an unreachable store) as the user's: the first is never rerun.
#[test]
fn what_could_not_be_read_is_classed_by_whose_fault_it_is() {
    assert_eq!(
        Settled::from(InputsError::Invalid("no image".to_owned())),
        Settled::Invalid("no image".to_owned())
    );
    assert_eq!(
        Settled::from(InputsError::Unavailable("down".to_owned())),
        Settled::Infra("down".to_owned())
    );
    for error in [
        PresignError::Refused("ceiling".to_owned()),
        PresignError::Unavailable("ceiling".to_owned()),
    ] {
        let Settled::Infra(why) = Settled::from(error) else {
            panic!("not infra");
        };
        assert!(why.contains("ceiling"), "{why}");
    }
    assert_eq!(require(true, || unreachable!()), Ok(()));
    assert_eq!(
        require(false, || "no".to_owned()),
        Err(Settled::Invalid("no".to_owned()))
    );
    let _ = ErrorCode::NoCapacity;
}

mod worker {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use kbf_lease_client::{RunLimits, RunSpec};
    use kbf_types::LeaseId;
    use tokio::time::Instant;

    use super::super::{Queued, Running, Stage, Task, reporting};
    use crate::executor::outcome::{Settled, result_of};
    use crate::executor::task::PackKey;
    use crate::executor::test_support::{GIB, Harness, start};

    fn pinned_spec(label: &str) -> RunSpec {
        let mut spec = RunSpec::new(
            format!("registry.test/app@sha256:{}", "ab".repeat(32)),
            RunLimits {
                cpu_millis: 1_000,
                memory_bytes: 2 * GIB,
                pids: None,
                wall_ms: Some(60_000),
                disk_bytes: None,
            },
        );
        spec.labels = BTreeMap::from([(super::super::RUN_LABEL.to_owned(), label.to_owned())]);
        spec
    }

    /// Catches a post whose answer was lost, whose run is on the list, being given up on (the
    /// lease released, the action rerun, a second run possible): the run is found by the label
    /// it was posted with and followed as the run, and a run of another label is not taken for
    /// it.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_was_posted_is_found_by_its_label() {
        let harness = Harness::new().await;
        let (worker, _messages) = harness.worker().await;
        let ctx = harness.ctx();
        let other = harness
            .deps
            .client
            .queue_run(worker.lease, &pinned_spec("someone-else"))
            .await
            .expect("posted");
        let posted = harness
            .deps
            .client
            .queue_run(worker.lease, &pinned_spec("mine"))
            .await
            .expect("posted");
        let key = PackKey::new("cas/b/1-1/aa-0");
        let running = |run| Running {
            run,
            key: key.clone(),
            truncated: false,
            cancelled: false,
        };
        let found = worker.recover(&ctx, "mine", running).await;
        let Queued::Posted(found) = found else {
            panic!("not found");
        };
        assert_eq!(found.run, posted.run_id);
        assert_ne!(found.run, other.run_id);
        let missing = worker.recover(&ctx, "nobody", running).await;
        assert!(matches!(
            missing,
            Queued::Retire(crate::executor::host::Retire::Ambiguous)
        ));
    }

    /// Catches a result being forgotten that the farm did not acknowledge: `report` answers
    /// `None` for a result its record is not written for (and for one it dropped over its
    /// cap), and the send that panicked answers nothing at all. Neither is an acknowledgement:
    /// the result is sent again, and only a real acknowledgement ends the sending.
    #[tokio::test(start_paused = true)]
    async fn a_report_the_farm_did_not_acknowledge_is_sent_again() {
        use kbf_proto::worker::ResultAck;
        let harness = Harness::new().await;
        let worker = kbf_types::WorkerId::new("js:node1");
        let held = tokio::spawn(async { None::<ResultAck> });
        let broken = tokio::spawn(async { panic!("the report broke") });
        let acked = tokio::spawn(async { Some(ResultAck::default()) });
        for (what, in_flight, done) in [
            ("held", held, false),
            ("broken", broken, false),
            ("acknowledged", acked, true),
        ] {
            let mut report = super::super::Reporting {
                result: kbf_proto::worker::Result::default(),
                in_flight: Some(in_flight),
                retry_at: Instant::now(),
            };
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            let said = report.drive(
                &harness.farm,
                &worker,
                Instant::now(),
                Duration::from_secs(5),
            );
            assert_eq!(said, done, "{what}");
            assert_eq!(report.in_flight.is_some(), !done, "{what}: sent again");
        }
    }

    /// Catches a lease that is gone being reported lost in part: each task the worker holds is
    /// told to the farm, a result already formed as it was formed, a run that was never posted
    /// as lost. Every one reaches the farm (an unknown lease, here: the answer is a refusal).
    #[tokio::test(start_paused = true)]
    async fn the_tasks_of_a_gone_lease_are_all_reported() {
        let harness = Harness::new().await;
        let (mut worker, _messages) = harness.worker().await;
        let ctx = harness.ctx();
        let now = Instant::now();
        worker.accept(&start(1, 1));
        let formed = LeaseId::new(1, 2);
        let action = start(1, 2).action_digest.expect("an action");
        let result = result_of(formed, &action, &Settled::Infra("formed".to_owned()));
        worker.tasks.insert(
            formed,
            Task {
                action,
                stage: reporting(result, now),
            },
        );
        worker.report_lost(&ctx).await;
        assert!(matches!(
            worker.tasks[&LeaseId::new(1, 1)].stage,
            Stage::Waiting(_)
        ));
    }
}
