//! `probe-bench` against scripted helpers.

use serde_json::json;

use crate::bench::{self, Run};
use crate::fake::{FakeSystem, Script, ctx, tmp, work_with_base};
use crate::guest::{READY_AQUA, READY_LOGIN, READY_SYSTEM};
use crate::report::Status;

fn ready() -> Script {
    Script::booted()
        .with_guest_file(READY_SYSTEM, "{}")
        .with_guest_file(READY_LOGIN, "{}")
}

/// Catches statistics computed over the wrong column (median of the clone times reported
/// as boot times), a marker that never appeared counted as zero, and a missing column
/// printed as an object instead of null.
#[test]
fn stats_summarize_each_column_over_the_runs_that_have_it() {
    let runs = [
        Run {
            clone: 1.0,
            running: 100.0,
            system: Some(1000.0),
            login: None,
            aqua: None,
            stop_to_exit: 5.0,
            failure: None,
        },
        Run {
            clone: 3.0,
            running: 300.0,
            system: Some(3000.0),
            login: None,
            aqua: Some(9.0),
            stop_to_exit: 15.0,
            failure: Some("helper ended".into()),
        },
        Run {
            clone: 2.0,
            running: 200.0,
            system: None,
            login: None,
            aqua: None,
            stop_to_exit: 10.0,
            failure: None,
        },
    ];
    let s = bench::stats(&runs);
    assert_eq!(s["runs"], 3);
    assert_eq!(s["clone_ms"]["median_ms"], 2.0);
    assert_eq!(s["boot_to_running_ms"]["max_ms"], 300.0);
    assert_eq!(s["boot_to_system_ready_ms"]["n"], 2);
    assert_eq!(s["boot_to_system_ready_ms"]["median_ms"], 2000.0);
    assert!(s["boot_to_login_ms"].is_null());
    assert_eq!(s["boot_to_aqua_ready_ms"]["n"], 1);
    assert_eq!(s["stop_to_exit_ms"]["median_ms"], 10.0);
    assert_eq!(s["failed_runs"], 1);
    assert_eq!(s["failures"], json!(["helper ended"]));
    assert!(
        s.get("slot_reusable_ms").is_none(),
        "no measure claims to time slot reuse"
    );
}

/// Catches a bench that purges the cache on the warm runs or not on the cold ones, a
/// wrong number of runs, a missing slot time on the last run, and markers not read.
#[test]
fn a_bench_runs_cold_then_warm_and_reports_every_measure() {
    let work = work_with_base("bench-ok");
    let sys = FakeSystem::new();
    for _ in 0..6 {
        sys.script(ready());
    }
    let mut c = ctx(&sys, &work);
    c.graphics = true;
    let r = bench::run(&c, 3);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    assert!(
        sys.starts
            .borrow()
            .iter()
            .all(|(s, _)| s.graphics && s.tagged.len() == 2)
    );
    assert_eq!(sys.purges.get(), 3);
    assert_eq!(sys.starts.borrow().len(), 6);
    for mode in ["cold", "warm"] {
        let m = &r.details[mode];
        assert_eq!(m["runs"], 3, "{mode}");
        assert_eq!(m["boot_to_system_ready_ms"]["n"], 3, "{mode}");
        assert_eq!(m["boot_to_login_ms"]["n"], 3, "{mode}");
        assert_eq!(m["failed_runs"], 0, "{mode}");
        assert!(m["boot_to_aqua_ready_ms"].is_null(), "{mode}");
        assert!(m["clone_ms"]["median_ms"].as_f64().expect("ms") >= 0.0);
    }
    assert!(!work.join("bench-cold-0").exists(), "runs are removed");
    assert!(r.summary.expect("summary").contains("and the same cold"));
}

/// Catches an Aqua marker the bench never looks for.
#[test]
fn an_aqua_marker_is_reported_when_the_image_has_one() {
    let work = work_with_base("bench-aqua");
    let sys = FakeSystem::new();
    for _ in 0..2 {
        sys.script(ready().with_guest_file(READY_AQUA, "{}"));
    }
    let r = bench::run(&ctx(&sys, &work), 1);
    assert_eq!(r.details["warm"]["boot_to_aqua_ready_ms"]["n"], 1);
}

/// Catches a bench that reports a guest it never saw: without the markers the host-side
/// numbers stand and the status is needs-manual with the reason.
#[test]
fn a_bench_without_guest_markers_needs_a_person_and_keeps_the_host_numbers() {
    let work = work_with_base("bench-nomarkers");
    let sys = FakeSystem::new();
    let r = bench::run(&ctx(&sys, &work), 2);
    assert_eq!(r.status, Status::NeedsManual);
    assert!(r.reason.expect("reason").contains("launch daemons"));
    // One silent run ends the batch: more would each wait out the whole guest timeout.
    assert_eq!(r.details["warm"]["boot_to_running_ms"]["n"], 1);
    assert_eq!(sys.starts.borrow().len(), 1 + 1);
    assert!(
        r.details["warm"]["incomplete"]
            .as_str()
            .expect("why")
            .contains("first run")
    );
    assert!(r.details["warm"]["boot_to_system_ready_ms"].is_null());
}

/// Catches a cold purge failure (not root) aborting the whole bench: the warm runs still
/// happen and the cold section says why it is missing.
#[test]
fn cold_runs_need_root_and_their_absence_is_explained() {
    let work = work_with_base("bench-nopurge");
    let sys = FakeSystem::new();
    *sys.purge.borrow_mut() = Err("must be root".into());
    for _ in 0..2 {
        sys.script(ready());
    }
    let r = bench::run(&ctx(&sys, &work), 2);
    assert_eq!(r.status, Status::Ok);
    assert_eq!(r.details["cold"]["status"], json!("needs-manual"));
    assert!(
        r.details["cold"]["reason"]
            .as_str()
            .expect("text")
            .contains("must be root")
    );
    assert_eq!(r.details["warm"]["runs"], 2);
    assert!(!r.summary.expect("summary").contains("cold"));
}

/// Catches a bench that goes on without a base, with zero runs, or after a start fails.
#[test]
fn a_bench_refuses_what_it_cannot_measure() {
    let sys = FakeSystem::new();
    let r = bench::run(&ctx(&sys, &tmp("bench-nobase")), 2);
    assert_eq!(r.status, Status::NeedsManual);
    let work = work_with_base("bench-zero");
    assert_eq!(bench::run(&ctx(&sys, &work), 0).status, Status::Error);
    *sys.start_error.borrow_mut() = Some("cannot spawn".into());
    let r = bench::run(&ctx(&sys, &work), 1);
    assert_eq!(r.status, Status::Error);
    assert!(
        r.reason
            .expect("reason")
            .contains("warm runs: cannot spawn")
    );
}

/// Catches a marker pair read as complete when only the first appeared: with only the
/// system marker the login time is missing, not copied from it.
#[test]
fn a_partial_marker_set_reports_only_what_appeared() {
    let work = work_with_base("bench-partial");
    let sys = FakeSystem::new();
    for _ in 0..2 {
        sys.script(Script::booted().with_guest_file(READY_SYSTEM, "{}"));
    }
    let r = bench::run(&ctx(&sys, &work), 1);
    assert_eq!(r.details["warm"]["boot_to_system_ready_ms"]["n"], 1);
    assert!(r.details["warm"]["boot_to_login_ms"].is_null());
}

/// Catches a clone failure inside a run being dropped.
#[test]
fn a_clone_failure_stops_the_bench() {
    let work = work_with_base("bench-clone-fail");
    let sys = FakeSystem::new();
    *sys.clone_fails_on.borrow_mut() = Some("aux.img");
    let r = bench::run(&ctx(&sys, &work), 1);
    assert_eq!(r.status, Status::Error);
    assert!(r.reason.expect("reason").contains("EXDEV"));
}

/// Catches a helper that died being waited on until the guest timeout (a missing
/// payload would then cost the whole timeout per run), and a failure not counted.
#[test]
fn a_helper_that_dies_ends_the_run_and_is_counted() {
    let work = work_with_base("bench-dies");
    let sys = FakeSystem::new();
    for _ in 0..2 {
        sys.script(Script::booted().then(crate::system::Poll::Gone {
            code: Some(1),
            signal: None,
        }));
    }
    let mut c = ctx(&sys, &work);
    c.timing.guest_timeout = std::time::Duration::from_secs(60);
    let r = bench::run(&c, 1);
    assert!(
        sys.clock.now() < c.timing.guest_timeout,
        "the run waited out the guest timeout: the clock reads {:?}",
        sys.clock.now()
    );
    assert_eq!(r.details["warm"]["failed_runs"], 1);
    assert!(
        r.details["warm"]["failures"][0]
            .as_str()
            .expect("text")
            .contains("helper ended")
    );
}

/// Catches finished runs thrown away when a later one breaks the batch, and a report
/// that does not say it is incomplete.
#[test]
fn runs_that_finished_are_kept_when_a_later_one_breaks() {
    let work = work_with_base("bench-partial-batch");
    let sys = FakeSystem::new();
    *sys.purge.borrow_mut() = Err("must be root".into());
    for _ in 0..2 {
        sys.script(ready());
    }
    // Each run clones four files: two runs fit, the third breaks.
    sys.clone_budget.set(Some(8));
    let r = bench::run(&ctx(&sys, &work), 3);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    assert_eq!(r.details["warm"]["runs"], 2);
    assert!(
        r.details["warm"]["incomplete"]
            .as_str()
            .expect("why")
            .contains("no more clones")
    );
}

/// Catches a report that leaves out how the numbers were taken: the polling intervals
/// that bound their precision, the idle host, the order of the runs, the force stop.
#[test]
fn the_report_says_how_the_numbers_were_taken() {
    let work = work_with_base("bench-method");
    let sys = FakeSystem::new();
    for _ in 0..4 {
        sys.script(ready());
    }
    let r = bench::run(&ctx(&sys, &work), 2);
    let m = &r.details["method"];
    assert_eq!(m["runs_requested"], 2);
    assert_eq!(m["guest_poll_ms"], 500);
    assert_eq!(m["host_poll_ms"], 1);
    assert!(m["load"].as_str().expect("load").contains("idle"));
    assert_eq!(m["stop"], "force stop");
}
