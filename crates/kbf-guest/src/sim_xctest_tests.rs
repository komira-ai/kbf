//! Tests of the UI-test steps of `sim`: where they sit in the plan, their commands, the
//! reading of `xcresulttool`'s summary, and what a failing or skipped step does to them.

use super::*;
use crate::sim_xctest::{self as xt, XCTEST_NAME, XcTestFrame};

/// `xcrun xcresulttool get test-results summary` for a run of three tests of the fixture:
/// one passed, one failed (two assertions) and one skipped, as Xcode prints it (the keys
/// are sorted, the devices listed with their own counts).
const SUMMARY_FAILED: &str = r#"{
  "devicesAndConfigurations" : [
    {
      "device" : {
        "architecture" : "arm64",
        "deviceId" : "6F3A1B2C-0D4E-4F5A-8B6C-7D8E9F0A1B2C",
        "deviceName" : "kbf-sim",
        "modelName" : "iPhone 17 Pro",
        "osBuildNumber" : "24A434",
        "osVersion" : "27.0",
        "platform" : "iOS Simulator"
      },
      "expectedFailures" : 0,
      "failedTests" : 1,
      "passedTests" : 1,
      "skippedTests" : 1
    }
  ],
  "environmentDescription" : "IOSFixtureUITests-Runner : Built with macOS 27.0.1",
  "expectedFailures" : 0,
  "failedTests" : 1,
  "finishTime" : 1790000012.5,
  "passedTests" : 1,
  "result" : "Failed",
  "skippedTests" : 1,
  "startTime" : 1790000000.25,
  "statistics" : [ ],
  "testFailures" : [
    {
      "failureText" : "XCTAssertEqual failed: (\"ready\") is not equal to (\"tapped 1\")",
      "targetName" : "IOSFixtureUITests",
      "testIdentifier" : 2,
      "testIdentifierString" : "FixtureUITests/testTapChangesLabel()",
      "testIdentifierURL" : "test://com.apple.xcode/IOSFixture/IOSFixtureUITests/FixtureUITests/testTapChangesLabel",
      "testName" : "testTapChangesLabel()"
    },
    {
      "failureText" : "a second assertion of the same test",
      "targetName" : "IOSFixtureUITests",
      "testIdentifier" : 2,
      "testIdentifierString" : "FixtureUITests/testTapChangesLabel()",
      "testIdentifierURL" : "test://com.apple.xcode/IOSFixture/IOSFixtureUITests/FixtureUITests/testTapChangesLabel",
      "testName" : "testTapChangesLabel()"
    }
  ],
  "title" : "Test - IOSFixture",
  "topInsights" : [ ],
  "totalTestCount" : 3
}"#;

const SUMMARY_PASSED: &str = r#"{"expectedFailures":0,"failedTests":0,"passedTests":3,
 "result":"Passed","skippedTests":0,"statistics":[],"testFailures":[],"totalTestCount":3}"#;

fn exited(code: i32, stdout: &str) -> Outcome {
    Outcome {
        ended: Ended::Exited(code),
        ..ok_out(stdout)
    }
}

/// A host with the app and one `.xctestrun` staged, `xcresulttool` printing `summary`.
fn staged(summary: &str) -> Fake {
    let mut host = Fake::new();
    host.apps = vec!["Demo.app".to_owned()];
    host.tests = vec!["IOSFixture_iphonesimulator27.0-arm64.xctestrun".to_owned()];
    host.answer("xcresulttool", ok_out(summary));
    host
}

fn xctest_frame(host: &Fake) -> Option<XcTestFrame> {
    host.sent
        .borrow()
        .iter()
        .find(|f| f.name == XCTEST_NAME)
        .map(|f| serde_json::from_str(&f.body).expect("an xctest frame"))
}

fn step_of(frames: &[StepFrame], id: &str) -> StepFrame {
    frames
        .iter()
        .find(|f| f.step == id)
        .unwrap_or_else(|| {
            panic!(
                "{id} in {:?}",
                frames.iter().map(|f| &f.step).collect::<Vec<_>>()
            )
        })
        .clone()
}

fn inputs(app: bool, tests: bool) -> Inputs {
    Inputs {
        runtimes: Vec::new(),
        app: app.then(|| "Demo.app".to_owned()),
        tests: tests.then(|| "T.xctestrun".to_owned()),
    }
}

/// Catches the test steps landing before the launch (the app would not be on the device),
/// attached to the wrong step when no app is staged, present without staged tests, or
/// the summary step needing the tests to succeed (a failing test is what it reads).
#[test]
fn the_test_steps_follow_the_launch_or_the_boot_and_only_when_tests_are_staged() {
    let with_app = plan(&inputs(true, true));
    let order = ids(&with_app);
    let at = |id: &str| order.iter().position(|x| *x == id).expect(id);
    assert!(at("launch") < at("xctest") && at("xctest") + 1 == at("xctest-summary"));
    let step = |id: &str| with_app.iter().find(|s| s.id == id).expect(id).clone();
    assert_eq!(
        (step("xctest").needs, step("xctest").ran, step("xctest").who),
        (vec!["launch".to_owned()], Vec::new(), Who::User)
    );
    assert_eq!(
        (step("xctest-summary").needs, step("xctest-summary").ran),
        (Vec::new(), vec!["xctest".to_owned()])
    );
    assert_eq!(step("xctest").class, Class::XcTest);
    let no_app = plan(&inputs(false, true));
    let step = no_app.iter().find(|s| s.id == "xctest").expect("xctest");
    assert_eq!(step.needs, ["bootstatus"]);
    let none = plan(&inputs(true, false));
    assert!(ids(&none).iter().all(|id| !id.starts_with("xctest")));
    assert_eq!(none.len() + 2, with_app.len());
}

/// Catches an xcodebuild command that is not the one specified (the verb, the staged
/// `.xctestrun`, the device by id, the result bundle in the out directory), a summary command
/// with another path than the bundle the tests wrote, and a step built without its inputs.
#[test]
fn the_commands_are_the_specified_ones() {
    let state = State {
        udid: Some(UDID.into()),
        xctestrun: Some("T.xctestrun".into()),
        ..State::default()
    };
    assert_eq!(
        state.resolve(Late::XcTest).expect("argv"),
        [
            "/usr/bin/xcodebuild",
            "test-without-building",
            "-xctestrun",
            "/private/var/kbf-sim/tests/T.xctestrun",
            "-destination",
            &format!("id={UDID}"),
            "-resultBundlePath",
            "/private/var/kbf-sim/out/tests.xcresult",
        ]
    );
    assert_eq!(
        xt::summary_argv(),
        [
            "/usr/bin/xcrun",
            "xcresulttool",
            "get",
            "test-results",
            "summary",
            "--path",
            "/private/var/kbf-sim/out/tests.xcresult",
        ]
    );
    let result = xt::RESULT_PATH;
    assert!(result.starts_with(&format!("{OUT_DIR}/")) && result.ends_with("tests.xcresult"));
    for broken in [
        State {
            udid: None,
            ..state.clone()
        },
        State {
            xctestrun: None,
            ..state.clone()
        },
    ] {
        assert!(broken.resolve(Late::XcTest).is_err());
    }
}

/// Catches the tests running with the quick deadline (they take minutes), the flag being
/// ignored, and the summary step sharing the tests' deadline.
#[test]
fn the_tests_have_their_own_deadline_with_a_default_of_half_an_hour() {
    let host = staged(SUMMARY_PASSED);
    go(&host);
    let deadline = |host: &Fake, word: &str| {
        host.ran
            .borrow()
            .iter()
            .find(|(argv, _)| argv.iter().any(|a| a == word))
            .map(|(_, d)| *d)
            .expect(word)
    };
    assert_eq!(
        deadline(&host, "test-without-building"),
        Duration::from_secs(1800)
    );
    assert_eq!(deadline(&host, "summary"), Duration::from_secs(120));
    let host = staged(SUMMARY_PASSED);
    run(
        &host,
        &Config::from(&args(&["--xctest-secs", "90"])),
        &mut Vec::new(),
    );
    assert_eq!(
        deadline(&host, "test-without-building"),
        Duration::from_secs(90)
    );
    assert_eq!(deadline(&host, "summary"), Duration::from_secs(120));
}

/// Catches counts read from the wrong keys, a failing test listed without its first message
/// (or with the second), and a skipped test counted as passed.
#[test]
fn a_realistic_summary_gives_the_counts_and_the_failed_tests_first_message() {
    let f = xt::parse_summary(SUMMARY_FAILED, false).expect("parse");
    assert_eq!(
        (
            f.result.as_str(),
            f.passed,
            f.failed,
            f.skipped,
            f.expected_failures,
            f.total
        ),
        ("Failed", 1, 1, 1, 0, 3)
    );
    assert_eq!(f.omitted, 0);
    assert_eq!(f.failures.len(), 1);
    assert_eq!(f.failures[0].test, "FixtureUITests/testTapChangesLabel()");
    assert_eq!(
        f.failures[0].message,
        "XCTAssertEqual failed: (\"ready\") is not equal to (\"tapped 1\")"
    );
    assert!(!f.failures[0].message_truncated);
    let p = xt::parse_summary(SUMMARY_PASSED, false).expect("parse");
    assert_eq!((p.passed, p.failed, p.skipped, p.total), (3, 0, 0, 3));
    assert!(p.failures.is_empty());
}

fn many_failures(n: usize, text: &str) -> String {
    let failures: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            serde_json::json!({
                "failureText": text,
                "testIdentifierString": format!("Suite/test{i}()"),
            })
        })
        .collect();
    serde_json::json!({
        "passedTests": 0, "failedTests": n, "skippedTests": 0, "testFailures": failures,
    })
    .to_string()
}

/// Catches a message over the 512 byte cap, a cut in the middle of a character, a flood of
/// failed tests filling the frame, and a test counted twice for its second assertion.
#[test]
fn the_messages_and_the_list_of_failures_are_capped() {
    // 300 two-byte characters are 600 bytes: the cut at 512 is a character boundary.
    let f = xt::parse_summary(&many_failures(1, &"\u{e9}".repeat(300)), false).expect("parse");
    assert_eq!(f.failures[0].message.len(), 512);
    assert!(f.failures[0].message_truncated);
    // A cut that would fall inside a three-byte character falls before it.
    let f = xt::parse_summary(&many_failures(1, &"\u{20ac}".repeat(300)), false).expect("parse");
    assert_eq!(f.failures[0].message.len(), 510);
    let long_name = serde_json::json!({
        "passedTests": 0, "failedTests": 1, "skippedTests": 0,
        "testFailures": [{"failureText": "x", "testIdentifierString": "n".repeat(1000)}],
    })
    .to_string();
    assert_eq!(
        xt::parse_summary(&long_name, false)
            .expect("parse")
            .failures[0]
            .test
            .len(),
        xt::MAX_TEST_NAME_BYTES
    );
    let f = xt::parse_summary(&many_failures(70, "no"), false).expect("parse");
    assert_eq!(
        (f.failures.len(), f.omitted, f.failed),
        (xt::MAX_LISTED, 20, 70)
    );
    let frame = serde_json::to_string(
        &xt::parse_summary(&many_failures(70, &"m".repeat(600)), false).expect("parse"),
    )
    .expect("json");
    assert!(frame.len() < MAX_BODY_BYTES / 8, "{}", frame.len());
    // The same test failing three times is listed once, with its first message.
    let thrice = serde_json::json!({
        "passedTests": 0, "failedTests": 1, "skippedTests": 0,
        "testFailures": [
            {"failureText": "first", "testIdentifierString": "S/t()"},
            {"failureText": "second", "testIdentifierString": "S/t()"},
            {"failureText": "third", "testName": "S/t()"},
        ],
    })
    .to_string();
    let f = xt::parse_summary(&thrice, false).expect("parse");
    assert_eq!(f.failures.len(), 1);
    assert_eq!(f.failures[0].message, "first");
    // A failure record with no identifier is still listed.
    let anon = r#"{"passedTests":0,"failedTests":1,"skippedTests":0,"testFailures":[{}]}"#;
    assert_eq!(
        xt::parse_summary(anon, false).expect("parse").failures[0].test,
        "?"
    );
}

/// Catches output taken for a summary that is not one: text that is not JSON, JSON with no
/// counts, and output the capture cut.
#[test]
fn output_that_is_not_a_summary_is_an_error() {
    assert!(
        xt::parse_summary("Error: no result bundle", false)
            .expect_err("text")
            .contains("not JSON")
    );
    let e = xt::parse_summary(r#"{"failedTests":0,"skippedTests":0}"#, false).expect_err("missing");
    assert!(e.contains("passedTests"), "{e}");
    let e = xt::parse_summary(SUMMARY_PASSED, true).expect_err("cut");
    assert!(e.contains("longer than the capture"), "{e}");
    // The counts alone are enough; the total is then their sum.
    let f = xt::parse_summary(
        r#"{"passedTests":2,"failedTests":1,"skippedTests":4}"#,
        false,
    )
    .expect("parse");
    assert_eq!((f.total, f.result.as_str()), (7, "unknown"));
}

/// Catches the steps not running in order against the real flow, the counts not reaching the
/// host in a frame of their own, the step frame repeating the result bundle's output, and a
/// failing test being reported as a passing run.
#[test]
fn a_run_with_a_failing_test_still_summarizes_it_and_the_run_is_not_ok() {
    let host = staged(SUMMARY_FAILED);
    host.answer(
        "test-without-building",
        exited(65, "Test session results\n** TEST FAILED **\n"),
    );
    let summary = go(&host);
    let frames = host.step_frames();
    assert_eq!(step_of(&frames, "xctest").status, Status::Failed);
    assert_eq!(step_of(&frames, "xctest").exit, Some(65));
    let s = step_of(&frames, "xctest-summary");
    assert_eq!(s.status, Status::Ok);
    assert_eq!(s.stdout, "");
    assert!(
        s.note
            .expect("note")
            .contains("1 passed, 1 failed, 1 skipped")
    );
    let frame = xctest_frame(&host).expect("xctest frame");
    assert_eq!((frame.passed, frame.failed, frame.skipped), (1, 1, 1));
    // Only the counts and the failures: no part of the summary JSON beyond them.
    let body = host
        .sent
        .borrow()
        .iter()
        .find(|f| f.name == XCTEST_NAME)
        .map(|f| f.body.clone())
        .expect("body");
    assert!(!body.contains("devicesAndConfigurations") && !body.contains("environmentDescription"));
    assert!(!summary.ok);
    assert!(
        summary
            .steps
            .contains(&("xctest".to_owned(), Status::Failed))
    );
    // The launch is before the tests, the screenshot before them too.
    let cmds = host.commands();
    let pos = |w: &str| cmds.iter().position(|c| c.contains(w)).expect(w);
    assert!(pos("simctl launch") < pos("test-without-building"));
    assert!(pos("test-without-building") < pos("xcresulttool"));
    assert!(pos("xcresulttool") < pos("simctl shutdown"));
}

/// Catches a passing run reported as not ok, and the frame missing when every test passed.
#[test]
fn a_run_where_every_test_passes_is_ok_and_sends_the_counts() {
    let host = staged(SUMMARY_PASSED);
    let summary = go(&host);
    assert!(summary.ok, "{:?}", summary.steps);
    let frame = xctest_frame(&host).expect("frame");
    assert_eq!(
        (frame.passed, frame.failed, frame.result.as_str()),
        (3, 0, "Passed")
    );
    assert!(
        summary
            .steps
            .iter()
            .any(|(id, s)| id == "xctest" && *s == Status::Ok)
    );
}

/// Catches a test step that runs on a device with no app, and a summary that runs on a
/// bundle nobody wrote: a failed launch skips the tests, and the skip reaches the summary.
#[test]
fn a_failed_launch_skips_the_tests_and_their_summary_without_running_them() {
    let host = staged(SUMMARY_PASSED);
    host.answer("simctl launch", exited(1, ""));
    go(&host);
    let frames = host.step_frames();
    let t = step_of(&frames, "xctest");
    assert_eq!(
        (t.status, t.skipped_because.as_deref()),
        (Status::Skipped, Some("launch"))
    );
    let s = step_of(&frames, "xctest-summary");
    assert_eq!(
        (s.status, s.skipped_because.as_deref()),
        (Status::Skipped, Some("xctest"))
    );
    let cmds = host.commands();
    assert!(
        cmds.iter()
            .all(|c| !c.contains("test-without-building") && !c.contains("xcresulttool"))
    );
    assert!(xctest_frame(&host).is_none());
}

/// Catches a summary read from a bundle that a killed (timed out) or unstartable run left
/// half written: only a run that ended, passing or failing, is summarized (a timeout is
/// not a failure the summary may read).
#[test]
fn a_timed_out_or_unstartable_test_run_is_not_summarized() {
    for outcome in [
        ended(Ended::TimedOut),
        ended(Ended::SpawnFailed("no such file".to_owned())),
    ] {
        let host = staged(SUMMARY_PASSED);
        host.answer("test-without-building", outcome);
        go(&host);
        let frames = host.step_frames();
        let s = step_of(&frames, "xctest-summary");
        assert_eq!(
            (s.status, s.skipped_because.as_deref()),
            (Status::Skipped, Some("xctest"))
        );
        assert!(host.commands().iter().all(|c| !c.contains("xcresulttool")));
    }
}

/// Catches a summary that is not one being passed on as success, a result bundle the capture
/// cut being parsed, and a frame the host never received being reported as delivered.
#[test]
fn a_summary_that_cannot_be_read_or_sent_is_an_error_step() {
    let host = staged("not json at all");
    let summary = go(&host);
    let s = step_of(&host.step_frames(), "xctest-summary");
    assert_eq!(s.status, Status::Error);
    assert!(s.note.expect("note").contains("not JSON"));
    assert!(xctest_frame(&host).is_none() && !summary.ok);

    let host = staged(SUMMARY_PASSED);
    host.answer(
        "xcresulttool",
        Outcome {
            cut: true,
            ..ok_out(SUMMARY_PASSED)
        },
    );
    go(&host);
    assert_eq!(
        step_of(&host.step_frames(), "xctest-summary").status,
        Status::Error
    );

    let mut host = staged(SUMMARY_PASSED);
    host.refuse = Some(XCTEST_NAME.to_owned());
    let summary = go(&host);
    assert_eq!(
        step_of(&host.step_frames(), "xctest-summary").status,
        Status::Error
    );
    assert!(summary.undelivered.contains(&XCTEST_NAME.to_owned()));
}

/// Catches the result bundle's output filling the summary step's frame even when the tool
/// fails after printing it.
#[test]
fn the_summary_steps_frame_never_carries_the_tools_stdout() {
    let host = staged(SUMMARY_FAILED);
    host.answer("xcresulttool", exited(1, SUMMARY_FAILED));
    go(&host);
    let s = step_of(&host.step_frames(), "xctest-summary");
    assert_eq!((s.status, s.stdout.as_str()), (Status::Failed, ""));
}

/// Catches the tests being taken from names that are not `.xctestrun` files of a safe name,
/// the first of several not being chosen the way the app is, and the steps appearing
/// without a staged `.xctestrun`.
#[test]
fn the_staged_xctestrun_is_the_smallest_safe_name() {
    let mut host = staged(SUMMARY_PASSED);
    host.tests = vec![
        "Debug-iphonesimulator".to_owned(),
        "b.xctestrun".to_owned(),
        "a b.xctestrun".to_owned(),
        ".hidden.xctestrun".to_owned(),
        "a.xctestrun".to_owned(),
    ];
    go(&host);
    assert!(
        host.commands()
            .iter()
            .any(|c| c.contains("/tests/a.xctestrun"))
    );
    let mut host = staged(SUMMARY_PASSED);
    host.tests = vec!["Debug-iphonesimulator".to_owned()];
    go(&host);
    assert!(
        host.commands()
            .iter()
            .all(|c| !c.contains("test-without-building"))
    );
    assert!(
        host.step_frames()
            .iter()
            .all(|f| !f.step.starts_with("xctest"))
    );
}

/// Catches an echoed note cut inside a character (the frame would not be valid text) or over
/// the cap.
#[test]
fn an_echoed_note_is_cut_at_a_character_boundary() {
    let note = crate::sim_parse::echo(&"\u{20ac}".repeat(300));
    assert_eq!(note, format!("{}{CUT_MARK}", "\u{20ac}".repeat(170)));
    assert!(note.len() <= MAX_ECHO_BYTES + CUT_MARK.len());
    assert_eq!(crate::sim_parse::echo("short"), "short");
}

/// Catches a result bundle of an earlier run left where xcodebuild is to write one (it
/// refuses, and the old counts would be reported as this run's): it is removed before the
/// tests start, not after.
#[test]
fn an_earlier_result_bundle_is_removed_before_the_tests_run() {
    let host = staged(SUMMARY_PASSED);
    host.stale_bundle.set(true);
    let summary = go(&host);
    assert!(summary.ok, "{:?}", summary.steps);
    let cmds = host.commands();
    let at = cmds
        .iter()
        .position(|c| c.contains("test-without-building"))
        .expect("tests ran");
    assert_eq!(host.cleared_at.get(), Some(at));
}

/// Catches the summary of a bundle this run did not write: xcodebuild failing before it
/// wrote one (with an older bundle on disk) must not report the old counts.
#[test]
fn a_run_that_wrote_no_bundle_does_not_summarize_an_old_one() {
    let mut host = staged(SUMMARY_FAILED);
    host.stale_bundle.set(true);
    host.no_bundle_written = true;
    host.answer("test-without-building", exited(70, "xcodebuild: error"));
    let summary = go(&host);
    let frames = host.step_frames();
    assert_eq!(step_of(&frames, "xctest").status, Status::Failed);
    let s = step_of(&frames, "xctest-summary");
    assert_eq!(s.status, Status::Error);
    assert!(s.note.expect("note").contains("wrote no result bundle"));
    assert!(host.commands().iter().all(|c| !c.contains("xcresulttool")));
    assert!(xctest_frame(&host).is_none() && !summary.ok);
}

/// Catches a bundle that cannot be removed being ignored: the tests must not run over it.
#[test]
fn a_result_bundle_that_cannot_be_removed_fails_the_step_and_runs_nothing() {
    let mut host = staged(SUMMARY_PASSED);
    host.clear_fails = true;
    go(&host);
    let frames = host.step_frames();
    let t = step_of(&frames, "xctest");
    assert_eq!(t.status, Status::Error);
    assert!(t.note.expect("note").contains("Operation not permitted"));
    assert_eq!(
        step_of(&frames, "xctest-summary")
            .skipped_because
            .as_deref(),
        Some("xctest")
    );
    assert!(
        host.commands()
            .iter()
            .all(|c| !c.contains("test-without-building"))
    );
}

/// Catches a removal that follows a link (it would delete what the link names), leaves part
/// of a bundle, or fails on a path that is not there.
#[test]
fn the_result_bundle_is_removed_without_following_links() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-xt-rm");
    std::fs::create_dir_all(dir.join("keep")).expect("dirs");
    std::fs::write(dir.join("keep/precious"), "x").expect("write");
    let bundle = dir.join("tests.xcresult");
    std::fs::create_dir_all(bundle.join("Data")).expect("bundle");
    std::fs::write(bundle.join("Data/f"), "x").expect("file");
    assert!(xt::is_real_dir(&bundle));
    xt::remove_no_follow(&bundle).expect("removed");
    assert!(!bundle.exists());
    xt::remove_no_follow(&bundle).expect("absent is fine");

    std::os::unix::fs::symlink(dir.join("keep"), &bundle).expect("link");
    assert!(!xt::is_real_dir(&bundle));
    xt::remove_no_follow(&bundle).expect("link removed");
    assert!(std::fs::symlink_metadata(&bundle).is_err());
    assert!(dir.join("keep/precious").exists());

    std::fs::write(&bundle, "a file").expect("file");
    xt::remove_no_follow(&bundle).expect("file removed");
    assert!(!bundle.exists());
}

/// Catches a total computed with `+` from counts a hostile result could make overflow.
#[test]
fn huge_counts_do_not_overflow_the_total() {
    let max = u64::MAX;
    let text = format!(
        r#"{{"passedTests":{max},"failedTests":{max},"skippedTests":{max},"expectedFailures":{max}}}"#
    );
    let f = xt::parse_summary(&text, false).expect("parse");
    assert_eq!((f.total, f.passed), (max, max));
}

/// Catches a frame of 50 control-character-heavy failures (each character escaped to six
/// bytes in JSON) outgrowing the body limit, and a frame that is too large not being
/// shortened by dropping failures from the end.
#[test]
fn the_worst_case_frame_fits_the_body_limit_and_an_oversized_one_is_shortened() {
    let name = "\u{1}".repeat(1000);
    let msg = "\u{1f}".repeat(1000);
    let failures: Vec<serde_json::Value> = (0..200)
        .map(|i| {
            serde_json::json!({
                "testIdentifierString": format!("{i:03}{name}"),
                "failureText": msg,
            })
        })
        .collect();
    let text = serde_json::json!({
        "passedTests": 0, "failedTests": 200, "skippedTests": 0, "testFailures": failures,
    })
    .to_string();
    let f = xt::parse_summary(&text, false).expect("parse");
    assert_eq!(f.failures.len(), xt::MAX_LISTED);
    let size = serde_json::to_string(&f).expect("json").len();
    assert!(size < MAX_BODY_BYTES / 2, "{size}");
    assert_eq!(xt::fit(f.clone(), xt::FRAME_LIMIT), f);
    let small = xt::fit(f.clone(), 10_000);
    assert!(serde_json::to_string(&small).expect("json").len() <= 10_000);
    assert!(small.failures.len() < f.failures.len());
    assert_eq!(
        small.failures.len() as u64 + small.omitted,
        f.failures.len() as u64 + f.omitted
    );
    assert_eq!(small.failures, f.failures[..small.failures.len()]);
}
