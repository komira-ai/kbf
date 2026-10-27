//! Tests of the UI-test counts in the `probe e3` report.

use super::*;
use kbf_guest::sim_xctest::{Failure, XCTEST_NAME, XcTestFrame};

fn counts(passed: u64, failed: u64, skipped: u64) -> (String, String) {
    let f = XcTestFrame {
        result: "Failed".to_owned(),
        passed,
        failed,
        skipped,
        expected_failures: 0,
        total: passed + failed + skipped,
        failures: vec![Failure {
            test: "FixtureUITests/testTapChangesLabel()".to_owned(),
            message: "XCTAssertEqual failed".to_owned(),
            message_truncated: false,
        }],
        omitted: 0,
    };
    (
        XCTEST_NAME.to_owned(),
        serde_json::to_string(&f).expect("json"),
    )
}

/// Catches the counts the guest sent being dropped from the report, reported under the wrong
/// arm, or missing from the one line the run prints.
#[test]
fn the_report_of_each_arm_carries_the_ui_test_counts() {
    let mut lines = Vec::new();
    for (mode, name) in [
        (Graphics::On, "e3-xctest-on"),
        (Graphics::Off, "e3-xctest-off"),
    ] {
        let work = work_with_base(name);
        let sys = FakeSystem::new();
        let mut frames = all_ok();
        frames.push(counts(2, 1, 3));
        sys.script(guest_sending(&frames));
        let r = e3(&guest_ctx(&sys, &work), mode);
        assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
        let x = &arm(&r, 0)["xctest"];
        assert!(
            holds(&[
                x["passed"] == json!(2),
                x["failed"] == json!(1),
                x["skipped"] == json!(3),
                x["total"] == json!(6),
                x["failures"][0]["test"] == json!("FixtureUITests/testTapChangesLabel()"),
                x["failures"][0]["message"] == json!("XCTAssertEqual failed"),
            ]),
            "{x}"
        );
        lines.push(r.summary.expect("summary"));
    }
    for line in lines {
        assert!(
            line.contains("UI tests 2 passed, 1 failed, 3 skipped"),
            "{line}"
        );
    }
}

/// Catches a run without tests reporting counts of zero (it ran none), and a frame that is not
/// the counts being taken for them.
#[test]
fn a_run_without_the_frame_has_no_counts_and_a_bad_frame_is_listed() {
    let work = work_with_base("e3-xctest-none");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&all_ok()));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    assert_eq!(arm(&r, 0)["xctest"], Value::Null);
    assert!(!r.summary.expect("summary").contains("UI tests"));

    let work = work_with_base("e3-xctest-bad");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    frames.push((XCTEST_NAME.to_owned(), r#"{"passed":"many"}"#.to_owned()));
    sys.script(guest_sending(&frames));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    let a = arm(&r, 0);
    assert_eq!(a["xctest"], Value::Null);
    assert_eq!(a["unreadable_frames"][0]["frame"], json!(XCTEST_NAME));
}
