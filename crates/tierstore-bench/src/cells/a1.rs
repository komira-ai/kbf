//! Cell A1, put-to-ack with a hot tier.
//!
//! Quantity: the time from sending a put to the instance until it acknowledges, p50, p95 and
//! p99, with the cold store healthy, slow (added latency per request, a bandwidth cap) and
//! stopped (refused, blackholed), the last two through the fault proxy ([`crate::proxy`]).
//! Needs a hot tier, so it is the second stage: with the v1 farm (local cache in front of the
//! cold store, no hot Silo) the instance acknowledges from the local cache, and the same arms
//! measure that.
//!
//! Arms, by the same builder as `cold-put` ([`crate::cells::putcell`]): `instance` (a plain put
//! through the instance, acknowledged from its hot tier, the copy to the cold tier following; its
//! bytes are counted against the cold ledger) and `hot` (a plain put straight to the hot test
//! bucket, the baseline, when the run configures the hot role). Sizes, requests in flight, the
//! open-loop arms and the fault modes are as in `cold-put`. Writes under `tierstore-bench/<run
//! id>/` of the instance's test bucket and deletes what it wrote after each pass.

use std::sync::Arc;

use crate::cells::engine::{Env, FaultSwitch, LoadSource, ProcLoad, run_cell};
use crate::cells::putcell::{self, PutPath};
use crate::cells::{CellContext, CellId, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;
use crate::role::Role;

/// The paths of the cell.
pub const PATHS: [PutPath; 2] = [
    PutPath {
        name: "hot",
        role: Role::Hot,
        multipart: false,
        cold: false,
    },
    PutPath {
        name: "instance",
        role: Role::Instance,
        multipart: false,
        cold: true,
    },
];

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded.
///
/// # Errors
/// As [`crate::cells::cold_put::run`].
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    run_with(ctx, plan, Arc::new(ProcLoad::new()), None, None).await
}

/// [`run`] with the machine, the fault proxy and a cap on the requests of a pass given.
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    ctx: &CellContext,
    plan: &CellPlan,
    load: Arc<dyn LoadSource>,
    faults: Option<Arc<dyn FaultSwitch>>,
    limit: Option<u64>,
) -> Result<Vec<CellResult>, BenchError> {
    let env = Env::new(ctx, load)?;
    let arms = putcell::arms(&env, CellId::A1, &PATHS, plan, faults, limit)?;
    run_cell(&env, CellId::A1, arms, plan.passes, None).await
}

#[cfg(test)]
mod tests {
    use objstore_s3::s3::fake::FakeS3;

    use super::*;
    use crate::cells::testkit::{self, Script};
    use crate::result::Window;

    /// Catches an arm that puts in parts where a plain, hot-acknowledged put is the quantity,
    /// the hot baseline being charged to the cold ledger, and objects left behind: two arms, two
    /// passes, three puts each.
    #[tokio::test]
    async fn the_instance_arm_puts_plainly_and_only_it_is_charged_to_the_cold_ledger() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-a1");
        let (hot, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Hot, hot.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let plan = CellPlan {
            passes: 2,
            in_flight: vec![1],
            object_bytes: vec![4096],
            open_loop: true,
            multipart_objects: None,
        };
        let results = run_with(&ctx, &plan, Script::calm(), None, Some(3))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(labels, ["hot-4kib-c1-healthy", "instance-4kib-c1-healthy"]);
        assert!(results.iter().all(|r| r.cell == CellId::A1));
        for fake in [&hot, &instance] {
            let stats = fake.stats();
            assert_eq!((stats.single_puts, stats.parts), (6, 0));
            assert!(fake.keys().is_empty());
        }
        let ledger = std::fs::read_to_string(dir.path().join("ledger.json")).unwrap();
        assert_eq!(
            ledger,
            format!("{{\"cold_written_bytes\":{}}}", 2 * 3 * 4096)
        );
    }

    /// Catches `run` going on without the stores of the cell.
    #[tokio::test]
    async fn the_run_verb_refuses_a_run_without_the_stores() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-a1-none");
        let ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        let plan = CellPlan {
            passes: 2,
            in_flight: vec![1],
            object_bytes: vec![],
            open_loop: true,
            multipart_objects: None,
        };
        assert!(matches!(run(&ctx, &plan).await, Err(BenchError::Setup(_))));
    }

    /// Catches a request of the cell whose record cannot be written being taken for a good one:
    /// the log refuses from its n-th write, for every n, until a run is long enough not to meet
    /// it.
    #[tokio::test]
    async fn a_log_that_fails_at_any_request_stops_the_run() {
        let mut finished = false;
        for taken in 0..400 {
            let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-a1-log");
            let (hot, instance) = (FakeS3::start().await, FakeS3::start().await);
            let stores = [
                (Role::Hot, hot.endpoint()),
                (Role::Instance, instance.endpoint()),
            ];
            let log = Arc::new(crate::requestlog::RequestLog::to_writer(
                testkit::FailAfter(taken),
            ));
            let ctx = testkit::ctx_with_log(dir.path(), &stores, Window::Busy, log);
            let plan = CellPlan {
                passes: 2,
                in_flight: vec![1],
                object_bytes: vec![4096],
                open_loop: true,
                multipart_objects: None,
            };
            match run_with(&ctx, &plan, Script::calm(), None, Some(1)).await {
                Ok(_) => {
                    finished = true;
                    break;
                }
                Err(e) => assert!(matches!(e, BenchError::Log(_)), "{taken}: {e}"),
            }
        }
        assert!(finished);
    }
}
