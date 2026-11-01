//! Cell cold-put, put-to-ack straight to the cold store.
//!
//! Quantity: the time from sending a put to the cold store until it acknowledges, p50, p95 and
//! (from quiet passes of 1,000 requests at least) p99, with the cold store healthy, slow (added
//! latency per request, a bandwidth cap) and stopped (refused, blackholed), the last two through
//! the fault proxy ([`crate::proxy`]).
//!
//! Which path each number is, in the arm label (`direct`, `direct-mp`, `instance`):
//! - `direct`: this tool against the test cold bucket with one PUT per object, the floor;
//! - `direct-mp`: this tool against the same bucket with a multipart upload, the floor of the
//!   path the instance uses;
//! - `instance`: a multipart upload through the instance in front of the same bucket. Tierstore
//!   at the pinned tag (v0.0.37) has no cold-only mode (`--hot` is a required flag), so the
//!   instance runs with a hot tier of its own on the same host (two-tier mode). A multipart
//!   upload sends its parts straight to the cold tier and skips the hot tier, so this arm
//!   measures the cold path of a two-tier instance; a plain put through it is acknowledged from
//!   the hot tier and is the cell `a1`.
//!
//! Sizes (4 KiB, 19 KiB, 64 KiB, 1 MiB, 16 MiB, 128 MiB unless `--object-bytes` says), requests
//! in flight (`--in-flight`; 64 only up to 1 MiB), a closed loop and, for 19 KiB and 1 MiB, an
//! open loop at a fixed rate so a stall is not coordinated omission, and the proxy in pass mode
//! as the control of every faulted arm. The arms are built by [`crate::cells::putcell`].
//!
//! Objects per pass: 1,000 up to 1 MiB (see [`putcell::requests_for`]). A multipart object is three
//! requests to the store (create, part, complete), so on a slow cold store the plan's count of the
//! two multipart arms may not fit a window; `--multipart-objects N` caps their count per pass at
//! N (`direct-mp` and `instance`, in the closed-loop arms of every size and width; an arm whose
//! plan is smaller keeps it) and leaves the
//! `direct` arm, the open-loop arms and the fault arms' own counts as they are. Without the flag
//! nothing changes. The ledger is asked for, and charged, the bytes of the objects the arm puts. A
//! p99 needs 1,000 requests, so an arm with fewer reports none (`p99_us` is null in each of its
//! passes) and says so in its notes, and each result records the objects per pass its arm used
//! (`objects_per_pass`).
//!
//! Writes under `tierstore-bench/<run id>/` of the test bucket, counts the bytes against the
//! ledger and deletes what it wrote after each pass. A stopped cold store is a result (the failed
//! requests and the time to them), not an error of the run. The fault arms need a
//! [`FaultSwitch`]; [`run`] has none, so it measures the healthy arms and says so.

use std::sync::Arc;

use crate::cells::engine::{Env, FaultSwitch, LoadSource, ProcLoad, run_cell};
use crate::cells::putcell::{self, PutPath};
use crate::cells::{CellContext, CellId, CellPlan};
use crate::error::BenchError;
use crate::result::CellResult;
use crate::role::Role;

/// The paths of the cell.
pub const PATHS: [PutPath; 3] = [
    PutPath {
        name: "direct",
        role: Role::Cold,
        multipart: false,
        cold: true,
    },
    PutPath {
        name: "direct-mp",
        role: Role::Cold,
        multipart: true,
        cold: true,
    },
    PutPath {
        name: "instance",
        role: Role::Instance,
        multipart: true,
        cold: true,
    },
];

/// Runs the cell against the stores the run configures: one [`CellResult`] per arm, passes
/// interleaved, pass 0 discarded. Needs the cold or the instance role.
///
/// # Errors
/// [`BenchError`] if no store of the cell is configured, the ledger refuses the cell, the load
/// gate trips or cannot be read, or a file or the request log fails.
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    run_with(ctx, plan, Arc::new(ProcLoad), None, None).await
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
    let arms = putcell::arms(&env, CellId::ColdPut, &PATHS, plan, faults, limit)?;
    run_cell(&env, CellId::ColdPut, arms, plan.passes, None).await
}

#[cfg(test)]
mod tests {
    use objstore_s3::s3::fake::FakeS3;

    use super::*;
    use crate::cells::testkit::{self, Script, SwitchLog};
    use crate::proxy::FaultMode;
    use crate::result::Window;

    fn plan(sizes: &[u64]) -> CellPlan {
        CellPlan {
            passes: 2,
            in_flight: vec![2],
            object_bytes: sizes.to_vec(),
            open_loop: true,
            multipart_objects: None,
        }
    }

    fn temp(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name)
    }

    /// Catches an arm that puts the wrong way (a plain put where a multipart is promised, or the
    /// reverse), to the wrong store, an object left behind, an upload left open, a byte not
    /// charged, or a sample lost: three arms, two passes, four puts each.
    #[tokio::test]
    async fn each_path_puts_its_own_way_to_its_own_store_and_leaves_nothing() {
        let dir = temp("tb-cold-put");
        let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let results = run_with(&ctx, &plan(&[4096]), Script::calm(), None, Some(4))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(
            labels,
            [
                "direct-4kib-c2-healthy",
                "direct-mp-4kib-c2-healthy",
                "instance-4kib-c2-healthy"
            ]
        );
        for result in &results {
            assert_eq!(result.cell, CellId::ColdPut);
            assert!(
                result
                    .passes
                    .iter()
                    .all(|p| (p.requests, p.failures) == (4, 0))
            );
            assert!(result.passes[1].percentiles.is_some());
        }
        let (c, i) = (cold.stats(), instance.stats());
        assert_eq!((c.single_puts, c.parts, c.completed), (8, 8, 8));
        assert_eq!((i.single_puts, i.parts, i.completed), (0, 8, 8));
        for fake in [&cold, &instance] {
            assert!(fake.keys().is_empty(), "{:?}", fake.keys());
            assert_eq!(fake.incomplete_uploads(), 0);
        }
        let ledger = std::fs::read_to_string(dir.path().join("ledger.json")).unwrap();
        assert_eq!(
            ledger,
            format!("{{\"cold_written_bytes\":{}}}", 3 * 2 * 4 * 4096)
        );
        let samples = std::fs::read_to_string(dir.path().join("samples.csv")).unwrap();
        let rows: Vec<Vec<&str>> = samples
            .lines()
            .skip(1)
            .map(|l| l.split(',').collect())
            .collect();
        assert_eq!(rows.len(), 24);
        assert!(rows.iter().all(|r| (r[8], r[9]) == ("put", "4096")));
        let by_role = |role: &str| rows.iter().filter(|r| r[7] == role).count();
        assert_eq!((by_role("cold"), by_role("instance")), (16, 8));
        let sent = testkit::requests(dir.path());
        assert!(
            sent.iter()
                .any(|r| (r.role, r.method.as_str()) == (Role::Instance, "POST"))
        );
        assert!(sent.iter().all(|r| r.bucket == "kbf-fake"));
    }

    fn with_multipart_objects(mut plan: CellPlan, n: u64) -> CellPlan {
        plan.multipart_objects = Some(n);
        plan
    }

    /// Catches the multipart count not reaching a multipart arm, leaking into the single-PUT arm,
    /// or the ledger charging the plan's bytes and not the bytes put: with 3 multipart objects
    /// and the requests capped at 5, `direct` puts 5 a pass and both multipart arms 3, the store
    /// sees exactly those puts, and the ledger holds their bytes.
    #[tokio::test]
    async fn the_multipart_arms_put_their_own_count_and_the_direct_arm_its_own() {
        let dir = temp("tb-cold-put-mp-count");
        let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let plan = with_multipart_objects(plan(&[4096]), 3);
        let results = run_with(&ctx, &plan, Script::calm(), None, Some(5))
            .await
            .unwrap();
        let counted: Vec<(&str, Vec<u64>)> = results
            .iter()
            .map(|r| {
                (
                    r.arm.as_str(),
                    r.passes.iter().map(|p| p.requests).collect(),
                )
            })
            .collect();
        assert_eq!(
            counted,
            [
                ("direct-4kib-c2-healthy", vec![5, 5]),
                ("direct-mp-4kib-c2-healthy", vec![3, 3]),
                ("instance-4kib-c2-healthy", vec![3, 3]),
            ]
        );
        let (c, i) = (cold.stats(), instance.stats());
        assert_eq!((c.single_puts, c.completed), (10, 6));
        assert_eq!((i.single_puts, i.completed), (0, 6));
        let ledger = std::fs::read_to_string(dir.path().join("ledger.json")).unwrap();
        assert_eq!(
            ledger,
            format!("{{\"cold_written_bytes\":{}}}", (5 + 3 + 3) * 2 * 4096)
        );
    }

    /// Catches a p99 claimed from fewer than a thousand requests, or an arm that drops the p99
    /// without saying why: in a quiet window with the multipart arms at 3 objects and the direct
    /// arm capped at 5, no arm has a p99 and every arm's notes give its own count as the reason.
    #[tokio::test]
    async fn an_arm_under_a_thousand_objects_records_no_p99_and_says_why() {
        let dir = temp("tb-cold-put-mp-p99");
        let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Quiet);
        let plan = with_multipart_objects(plan(&[4096]), 3);
        let results = run_with(&ctx, &plan, Script::calm(), None, Some(5))
            .await
            .unwrap();
        let why = |n: u64| {
            format!("p99 not reported: {n} objects per pass, fewer than the 1,000 a p99 needs")
        };
        for (result, n) in results.iter().zip([5, 3, 3]) {
            let p99: Vec<Option<u64>> = result
                .passes
                .iter()
                .map(|p| p.percentiles.unwrap().p99_us)
                .collect();
            assert_eq!(p99, [None, None], "{}", result.arm);
            let reasons = result.notes.iter().filter(|x| x.starts_with("p99 "));
            assert_eq!(reasons.collect::<Vec<_>>(), [&why(n)], "{}", result.arm);
        }
    }

    /// Catches a result that does not say how many objects its arm put per pass, in the value
    /// returned or in the file written: the JSON of each arm holds the count the arm used.
    #[tokio::test]
    async fn the_result_records_the_object_count_used() {
        let dir = temp("tb-cold-put-mp-record");
        let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let plan = with_multipart_objects(plan(&[4096]), 3);
        let results = run_with(&ctx, &plan, Script::calm(), None, Some(5))
            .await
            .unwrap();
        let used: Vec<Option<u64>> = results.iter().map(|r| r.objects_per_pass).collect();
        assert_eq!(used, [Some(5), Some(3), Some(3)]);
        let written: Vec<Option<u64>> = results
            .iter()
            .map(|r| {
                let file = dir.path().join(format!("cold-put-{}-a1.json", r.arm));
                let text = std::fs::read_to_string(file).unwrap();
                CellResult::from_json(&text).unwrap().objects_per_pass
            })
            .collect();
        assert_eq!(written, used);
    }

    /// Catches fault modes not set around each pass, or not put back before the cleanup (a
    /// cleanup through a blackhole would hang a minute per delete): every pass of a faulted arm
    /// sets its mode, and every pass ends with the proxy in pass mode.
    #[tokio::test]
    async fn every_faulted_pass_sets_its_mode_and_ends_in_pass_mode() {
        let dir = temp("tb-cold-put-faults");
        let cold = FakeS3::start().await;
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, cold.endpoint())], Window::Busy);
        let switch = Arc::new(SwitchLog::default());
        let results = run_with(
            &ctx,
            &plan(&[4096]),
            Script::calm(),
            Some(switch.clone()),
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 10);
        let modes = switch.modes();
        assert_eq!(modes.len(), 40);
        let count = |m: FaultMode| modes.iter().filter(|x| **x == m).count();
        assert_eq!(count(FaultMode::Latency { ms: 50 }), 4);
        assert_eq!(count(FaultMode::Latency { ms: 500 }), 4);
        assert_eq!(count(FaultMode::Refuse), 4);
        assert_eq!(count(FaultMode::Blackhole), 4);
        assert_eq!(count(FaultMode::Pass), 24);
        assert!(
            results
                .iter()
                .any(|r| r.arm == "direct-mp-4kib-c6-blackhole")
        );
        assert!(cold.keys().is_empty());
    }

    /// Catches a dead cold store being an error of the run: its puts are failures with no
    /// status, the arm still ends, the cleanup copes, and no object is claimed.
    #[tokio::test]
    async fn a_dead_store_is_a_result_of_failed_puts() {
        let dir = temp("tb-cold-put-dead");
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, &dead)], Window::Quiet);
        let results = run_with(&ctx, &plan(&[4096]), Script::calm(), None, Some(2))
            .await
            .unwrap();
        for result in &results {
            let pass = &result.passes[1];
            assert_eq!(
                (pass.requests, pass.failures, pass.percentiles),
                (2, 2, None)
            );
        }
        let samples = std::fs::read_to_string(dir.path().join("samples.csv")).unwrap();
        assert!(
            samples.lines().skip(1).all(|l| l.ends_with(",,1")),
            "no status when no answer"
        );
    }

    /// Catches `run` doing anything with a run that configures no store of the cell.
    #[tokio::test]
    async fn the_run_verb_refuses_a_run_without_the_stores() {
        let dir = temp("tb-cold-put-none");
        let ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        let err = run(&ctx, &plan(&[])).await.unwrap_err();
        assert!(matches!(err, BenchError::Setup(_)), "{err}");
    }

    /// Catches a request of the cell whose record cannot be written being taken for a good one,
    /// at any of its requests (a put, a part, a delete, a listing of uploads): the log refuses
    /// from its n-th write, for every n, until a run is long enough not to meet it.
    #[tokio::test]
    async fn a_log_that_fails_at_any_request_stops_the_run() {
        let mut finished = false;
        for taken in 0..400 {
            let dir = temp("tb-cold-put-log");
            let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
            let stores = [
                (Role::Cold, cold.endpoint()),
                (Role::Instance, instance.endpoint()),
            ];
            let log = Arc::new(crate::requestlog::RequestLog::to_writer(
                testkit::FailAfter(taken),
            ));
            let ctx = testkit::ctx_with_log(dir.path(), &stores, Window::Busy, log);
            match run_with(&ctx, &plan(&[4096]), Script::calm(), None, Some(1)).await {
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
