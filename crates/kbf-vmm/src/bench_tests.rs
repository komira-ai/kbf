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
            slot_reusable: Some(7.0),
        },
        Run {
            clone: 3.0,
            running: 300.0,
            system: Some(3000.0),
            login: None,
            aqua: Some(9.0),
            stop_to_exit: 15.0,
            slot_reusable: None,
        },
        Run {
            clone: 2.0,
            running: 200.0,
            system: None,
            login: None,
            aqua: None,
            stop_to_exit: 10.0,
            slot_reusable: Some(9.0),
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
    assert_eq!(s["slot_reusable_ms"]["n"], 2);
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
    let r = bench::run(&ctx(&sys, &work), 3);
    assert_eq!(r.status, Status::Ok, "{r:?}");
    assert_eq!(sys.purges.get(), 3);
    assert_eq!(sys.starts.borrow().len(), 6);
    for mode in ["cold", "warm"] {
        let m = &r.details[mode];
        assert_eq!(m["runs"], 3, "{mode}");
        assert_eq!(m["boot_to_system_ready_ms"]["n"], 3, "{mode}");
        assert_eq!(m["boot_to_login_ms"]["n"], 3, "{mode}");
        assert_eq!(m["slot_reusable_ms"]["n"], 3, "{mode}");
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
    assert!(r.reason.expect("reason").contains("starts kbf-guest"));
    assert_eq!(r.details["warm"]["boot_to_running_ms"]["n"], 2);
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
