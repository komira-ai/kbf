//! The simulator probe's UI tests: `xcodebuild test-without-building` on the device the run
//! booted, and the summary of its result bundle.
//!
//! Two user steps follow `launch` (see [`crate::sim`]). `xctest` runs the staged
//! `*.xctestrun` ([`simpaths::TESTS_DIR`], from `image setup --sim-tests`) with a result
//! bundle in [`RESULT_PATH`]. `xctest-summary` runs `xcresulttool get test-results summary`
//! on it and sends [`XCTEST_NAME`]: the counts, and for each failed test its name and the
//! first failure message, nothing else. A failed test makes `xctest` exit non-zero and
//! still leaves a bundle, so the summary step runs once `xctest` has run (`ran`), not only
//! when it succeeded.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sim::{Argv, Class, Late, Step, Who};
use crate::simpaths;

/// The name of the frame that carries the counts.
pub const XCTEST_NAME: &str = "sim-xctest.json";
/// The suffix of the file `xcodebuild build-for-testing` writes beside the products.
pub const XCTESTRUN_SUFFIX: &str = ".xctestrun";
/// The result bundle `xcodebuild` writes (in the out directory, which the user can write).
pub const RESULT_PATH: &str = "/private/var/kbf-sim/out/tests.xcresult";
/// The step that runs the tests, and the step that reads their result bundle.
pub const XCTEST_STEP: &str = "xctest";
pub const SUMMARY_STEP: &str = "xctest-summary";
/// The longest failure message a frame carries, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 512;
/// The longest test name a frame carries, in bytes.
pub const MAX_TEST_NAME_BYTES: usize = 256;
/// The most failed tests a frame lists; the count of the rest is in `omitted`.
pub const MAX_LISTED: usize = 50;

const XCODEBUILD: &str = "/usr/bin/xcodebuild";
const XCRUN: &str = "/usr/bin/xcrun";

/// The path of the staged `.xctestrun` in the guest.
#[must_use]
pub fn testrun_path(name: &str) -> String {
    format!("{}/{name}", simpaths::TESTS_DIR)
}

/// The command of the `xctest` step.
#[must_use]
pub fn test_argv(xctestrun: &str, udid: &str) -> Vec<String> {
    [
        XCODEBUILD,
        "test-without-building",
        "-xctestrun",
        &testrun_path(xctestrun),
        "-destination",
        &format!("id={udid}"),
        "-resultBundlePath",
        RESULT_PATH,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The command of the `xctest-summary` step. The summary is JSON by default (this is the
/// form the repository's own XCUITest spike runs).
#[must_use]
pub fn summary_argv() -> Vec<String> {
    [
        XCRUN,
        "xcresulttool",
        "get",
        "test-results",
        "summary",
        "--path",
        RESULT_PATH,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The two steps, after the step `after` (`launch`, or `bootstatus` when no app was staged).
#[must_use]
pub fn steps(after: &str) -> Vec<Step> {
    vec![
        Step {
            id: XCTEST_STEP.to_owned(),
            who: Who::User,
            class: Class::XcTest,
            needs: vec![after.to_owned()],
            ran: Vec::new(),
            argv: Argv::Late(Late::XcTest),
        },
        Step {
            id: SUMMARY_STEP.to_owned(),
            who: Who::User,
            class: Class::Quick,
            needs: Vec::new(),
            ran: vec![XCTEST_STEP.to_owned()],
            argv: Argv::Fixed(summary_argv()),
        },
    ]
}

/// One failed test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub test: String,
    /// The first failure message of the test, cut to [`MAX_MESSAGE_BYTES`].
    pub message: String,
    pub message_truncated: bool,
}

/// The body of [`XCTEST_NAME`]: what the host learns of the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XcTestFrame {
    /// Xcode's verdict (`Passed`, `Failed`, ...), as reported.
    pub result: String,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub expected_failures: u64,
    pub total: u64,
    pub failures: Vec<Failure>,
    /// Failed tests past [`MAX_LISTED`] that are not listed.
    pub omitted: u64,
}

/// `text` cut to at most `max` bytes at a character boundary; whether it was cut.
fn cap(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn count(v: &Value, key: &str) -> Result<u64, String> {
    v[key]
        .as_u64()
        .ok_or_else(|| format!("not an xcresulttool test-results summary: no number `{key}`"))
}

/// Reads the output of `xcresulttool get test-results summary`.
///
/// # Errors
/// `cut` (the output was longer than the capture allowed), or the text is not that JSON: not
/// JSON, or without the `passedTests`, `failedTests` and `skippedTests` counts.
pub fn parse_summary(text: &str, cut: bool) -> Result<XcTestFrame, String> {
    if cut {
        return Err("the xcresulttool output is longer than the capture allows".to_owned());
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let (passed, failed, skipped) = (
        count(&v, "passedTests")?,
        count(&v, "failedTests")?,
        count(&v, "skippedTests")?,
    );
    let expected_failures = v["expectedFailures"].as_u64().unwrap_or(0);
    let total = v["totalTestCount"]
        .as_u64()
        .unwrap_or(passed + failed + skipped + expected_failures);
    let mut failures: Vec<Failure> = Vec::new();
    let mut omitted = std::collections::BTreeSet::new();
    for f in v["testFailures"].as_array().into_iter().flatten() {
        let name = f["testIdentifierString"]
            .as_str()
            .or_else(|| f["testName"].as_str())
            .unwrap_or("?");
        let (test, _) = cap(name, MAX_TEST_NAME_BYTES);
        // Only the first message of a test.
        if failures.iter().any(|x| x.test == test) {
            continue;
        }
        if failures.len() == MAX_LISTED {
            omitted.insert(test);
            continue;
        }
        let (message, message_truncated) =
            cap(f["failureText"].as_str().unwrap_or(""), MAX_MESSAGE_BYTES);
        failures.push(Failure {
            test,
            message,
            message_truncated,
        });
    }
    Ok(XcTestFrame {
        result: cap(v["result"].as_str().unwrap_or("unknown"), 32).0,
        passed,
        failed,
        skipped,
        expected_failures,
        total,
        failures,
        omitted: omitted.len() as u64,
    })
}
