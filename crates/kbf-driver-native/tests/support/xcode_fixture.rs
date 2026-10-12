//! The XCUITest fixtures (`tools/spike/macos/fixtures`): the `xcodebuild` command that
//! builds one and runs its UI tests, and the check of the result bundle a run leaves.
//!
//! A run passes only when `xcodebuild` exited 0, exactly the three expected tests
//! passed, and the bundle kept their three screenshots (`window` and `screen` from
//! `testScreenshotShowsPattern`, `window-no-pattern` from
//! `testPatternLeftOutIsNotFound`), each exported to a non-empty file. A run that
//! tested nothing, or whose screenshots were deleted (a passing test without attachment
//! lifetime `keepAlways`), fails.
//!
//! The iOS fixture runs on a simulator: the first iPhone of the newest iOS runtime
//! `simctl` lists. The macOS fixture runs in this login session. The bundle is read
//! with `xcresulttool` by the caller, outside any sandbox ([`report`]): reading it is
//! the test's work, not the action's.
//!
//! The parsers here are pure and tested in this module; [`report`], [`destination`] and
//! [`pattern_checker_selftest`] run Xcode's tools.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The fixtures in this checkout.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/spike/macos/fixtures")
}

/// Which fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Ios,
    Macos,
}

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Macos => "macos",
        }
    }

    /// The project and its scheme.
    pub fn project(self) -> &'static str {
        match self {
            Self::Ios => "IOSFixture",
            Self::Macos => "MacFixture",
        }
    }
}

/// The `xcodebuild` action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Build, then run the tests.
    Test,
    /// Only build, into the derived data directory.
    BuildForTesting,
    /// Run the tests of an earlier [`Action::BuildForTesting`].
    TestWithoutBuilding,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::BuildForTesting => "build-for-testing",
            Self::TestWithoutBuilding => "test-without-building",
        }
    }

    /// Whether the action runs tests, and so leaves a result bundle.
    pub fn tests(self) -> bool {
        self != Self::BuildForTesting
    }
}

/// The bundle a testing run of `platform` leaves in `out`.
pub fn bundle(out: &Path, platform: Platform) -> PathBuf {
    out.join(format!("{}.xcresult", platform.name()))
}

/// Where the run's `xcodebuild` output goes.
pub fn log(out: &Path, platform: Platform, action: Action) -> PathBuf {
    match action {
        Action::Test => out.join(format!("{}-xcodebuild.log", platform.name())),
        _ => out.join(format!(
            "{}-{}-xcodebuild.log",
            platform.name(),
            action.name()
        )),
    }
}

/// `xcodebuild`'s arguments for `action` on the fixture of `platform` in `fixtures`,
/// with its derived data, and its bundle for a testing action, in `out`.
pub fn xcodebuild_args(
    fixtures: &Path,
    platform: Platform,
    action: Action,
    destination: &str,
    out: &Path,
) -> Vec<String> {
    let project = platform.project();
    let mut args = vec![
        action.name().to_owned(),
        "-project".to_owned(),
        path(&fixtures.join(format!("{project}.xcodeproj"))),
        "-scheme".to_owned(),
        project.to_owned(),
        "-destination".to_owned(),
        destination.to_owned(),
        "-derivedDataPath".to_owned(),
        path(&out.join(format!("{}-derived", platform.name()))),
    ];
    if action.tests() {
        args.push("-resultBundlePath".to_owned());
        args.push(path(&bundle(out, platform)));
    }
    args
}

fn path(p: &Path) -> String {
    p.to_str().expect("a UTF-8 path").to_owned()
}

/// A simulator to run the iOS fixture on.
#[derive(Debug, PartialEq, Eq)]
pub struct Simulator {
    pub udid: String,
    /// The runtime's last name segment (`iOS-18-5`).
    pub runtime: String,
    pub name: String,
}

/// The first iPhone of the newest iOS runtime in `xcrun simctl list devices available
/// -j`'s output, by the runtime's version numbers (`iOS-18-10` is newer than
/// `iOS-18-9`); `None` when no runtime has an iPhone.
pub fn pick_simulator(list: &Value) -> Option<Simulator> {
    let devices = list.get("devices")?.as_object()?;
    let mut runtimes: Vec<(Vec<u32>, &String, &Value)> = devices
        .iter()
        .filter_map(|(key, devices)| {
            let (_, version) = key.rsplit_once(".iOS-")?;
            let version: Option<Vec<u32>> = version.split('-').map(|p| p.parse().ok()).collect();
            Some((version?, key, devices))
        })
        .collect();
    runtimes.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    runtimes.iter().rev().find_map(|(_, key, devices)| {
        let phone = devices.as_array()?.iter().find(|d| {
            d.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.starts_with("iPhone"))
        })?;
        Some(Simulator {
            udid: phone.get("udid")?.as_str()?.to_owned(),
            runtime: key
                .rsplit_once('.')
                .map_or(key.as_str(), |(_, r)| r)
                .to_owned(),
            name: phone.get("name")?.as_str()?.to_owned(),
        })
    })
}

/// The `-destination` for `platform`: this Mac, or the simulator [`pick_simulator`]
/// chooses.
pub fn destination(platform: Platform) -> Result<String, String> {
    match platform {
        Platform::Macos => {
            let arch = match std::env::consts::ARCH {
                "aarch64" => "arm64",
                other => other,
            };
            Ok(format!("platform=macOS,arch={arch}"))
        }
        Platform::Ios => {
            let list = tool_json(&["simctl", "list", "devices", "available", "-j"])?;
            let sim = pick_simulator(&list).ok_or("no iPhone simulator is available")?;
            eprintln!("simulator: {} ({})", sim.name, sim.runtime);
            Ok(format!("platform=iOS Simulator,id={}", sim.udid))
        }
    }
}

/// `xcrun ARGS`'s stdout as JSON.
fn tool_json(args: &[&str]) -> Result<Value, String> {
    let out = Command::new("/usr/bin/xcrun")
        .args(args)
        .output()
        .map_err(|e| format!("xcrun {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "xcrun {args:?}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("xcrun {args:?}: not JSON: {e}"))
}

/// The three tests the fixtures hold.
pub const TESTS: [&str; 3] = [
    "testTapChangesLabel()",
    "testScreenshotShowsPattern()",
    "testPatternLeftOutIsNotFound()",
];

/// The screenshots the tests keep: (test, attachment name).
pub const SCREENSHOTS: [(&str, &str); 3] = [
    ("FixtureUITests/testScreenshotShowsPattern()", "window"),
    ("FixtureUITests/testScreenshotShowsPattern()", "screen"),
    (
        "FixtureUITests/testPatternLeftOutIsNotFound()",
        "window-no-pattern",
    ),
];

/// Each test case in `xcresulttool get test-results tests`'s output, with its result,
/// at any depth.
pub fn test_cases(results: &Value) -> Vec<(String, String)> {
    fn walk(node: &Value, out: &mut Vec<(String, String)>) {
        if node.get("nodeType").and_then(Value::as_str) == Some("Test Case") {
            let field = |k| node.get(k).and_then(Value::as_str).unwrap_or("?");
            out.push((field("name").to_owned(), field("result").to_owned()));
        }
        for child in node
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(child, out);
        }
    }
    let mut cases = Vec::new();
    for node in results
        .get("testNodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        walk(node, &mut cases);
    }
    cases
}

/// Exactly the three [`TESTS`] ran, and each passed.
pub fn the_three_passed(cases: &[(String, String)]) -> bool {
    let passed: BTreeSet<&str> = cases
        .iter()
        .filter(|(_, result)| result == "Passed")
        .map(|(name, _)| name.as_str())
        .collect();
    cases.len() == TESTS.len() && passed == BTreeSet::from(TESTS)
}

/// The attachment name inside Xcode's export name `<name>_<index>_<UUID>.png`.
pub fn screenshot_name(exported_as: &str) -> Option<&str> {
    let stem = exported_as.strip_suffix(".png")?;
    let (stem, uuid) = stem.rsplit_once('_')?;
    let (name, index) = stem.rsplit_once('_')?;
    let uuid_ok = !uuid.is_empty() && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    let index_ok = !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit());
    (uuid_ok && index_ok && !name.is_empty()).then_some(name)
}

/// The [`SCREENSHOTS`] missing from the export in `dir`, whose `manifest.json` is
/// `manifest`: one counts when the manifest lists it under its test and the file it
/// names in `dir` is not empty.
pub fn missing_screenshots(manifest: &Value, dir: &Path) -> Vec<String> {
    let mut got = BTreeSet::new();
    for test in manifest.as_array().into_iter().flatten() {
        let id = test
            .get("testIdentifier")
            .and_then(Value::as_str)
            .unwrap_or("?");
        for a in test
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = |k| a.get(k).and_then(Value::as_str).unwrap_or("?");
            let kept = std::fs::metadata(dir.join(field("exportedFileName")))
                .is_ok_and(|m| m.is_file() && m.len() > 0);
            if let Some(name) = screenshot_name(field("suggestedHumanReadableName"))
                && kept
            {
                got.insert((id.to_owned(), name.to_owned()));
            }
        }
    }
    SCREENSHOTS
        .iter()
        .filter(|(test, name)| !got.contains(&((*test).to_owned(), (*name).to_owned())))
        .map(|(test, name)| format!("{test} {name}"))
        .collect()
}

/// The `KBF-FIXTURE <name>=PASS|FAIL <detail>` lines the tests printed into `log`.
pub fn fixture_lines(log: &str) -> Vec<&str> {
    let lines: BTreeSet<&str> = log
        .lines()
        .filter_map(|l| l.find("KBF-FIXTURE ").map(|i| &l[i..]))
        .collect();
    lines.into_iter().collect()
}

/// What a testing run's bundle holds.
#[derive(Debug)]
pub struct Report {
    pub cases: Vec<(String, String)>,
    /// The screenshots missing from the bundle, or why it could not be read.
    pub missing: Vec<String>,
}

impl Report {
    pub fn passed(&self) -> bool {
        the_three_passed(&self.cases) && self.missing.is_empty()
    }
}

/// Reads the bundle a testing run of `platform` left in `out`, exporting its
/// attachments to `out/<platform>-attachments`.
pub fn report(out: &Path, platform: Platform) -> Report {
    let bundle = bundle(out, platform);
    let bundle_arg = path(&bundle);
    if !bundle.is_dir() {
        return Report {
            cases: Vec::new(),
            missing: vec![format!("no bundle at {bundle_arg}")],
        };
    }
    let cases = match tool_json(&[
        "xcresulttool",
        "get",
        "test-results",
        "tests",
        "--path",
        &bundle_arg,
    ]) {
        Ok(results) => test_cases(&results),
        Err(e) => vec![("xcresulttool get test-results".to_owned(), e)],
    };
    let dir = out.join(format!("{}-attachments", platform.name()));
    let _ = std::fs::remove_dir_all(&dir);
    let dir_arg = path(&dir);
    let exported = Command::new("/usr/bin/xcrun")
        .args([
            "xcresulttool",
            "export",
            "attachments",
            "--path",
            &bundle_arg,
        ])
        .args(["--output-path", &dir_arg])
        .status()
        .is_ok_and(|s| s.success());
    let manifest = std::fs::read(dir.join("manifest.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let missing = match manifest {
        Some(manifest) if exported => missing_screenshots(&manifest, &dir),
        _ => vec![format!(
            "xcresulttool export attachments to {dir_arg} failed"
        )],
    };
    Report { cases, missing }
}

/// Builds the pixel checker's selftest (`selftest/main.swift`) into `out` with the
/// active Xcode's `swiftc`, runs it, and returns its exit status and output.
pub fn pattern_checker_selftest(out: &Path) -> Result<(bool, String), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let fixtures = fixtures();
    let exe = out.join("patterncheck-selftest");
    let built = Command::new("/usr/bin/xcrun")
        .args(["swiftc", "-O", "-o"])
        .arg(&exe)
        .arg(fixtures.join("Common/Pattern.swift"))
        .arg(fixtures.join("UITests/PatternCheck.swift"))
        .arg(fixtures.join("selftest/main.swift"))
        .output()
        .map_err(|e| format!("xcrun swiftc: {e}"))?;
    if !built.status.success() {
        return Err(format!(
            "swiftc: {}: {}",
            built.status,
            String::from_utf8_lossy(&built.stderr)
        ));
    }
    let ran = Command::new(&exe)
        .output()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    Ok((ran.status.success(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The arguments for each action: a testing action names the bundle, a build does
    /// not (an `xcodebuild build-for-testing -resultBundlePath` would leave a bundle the
    /// later test-only run then refuses to overwrite); each names the fixture's own
    /// project and scheme and keeps derived data in `out`, inside the lease.
    #[test]
    fn xcodebuild_gets_the_fixture_the_destination_and_the_out_dir() {
        let args = xcodebuild_args(
            Path::new("/l/root/fixtures"),
            Platform::Ios,
            Action::Test,
            "platform=iOS Simulator,id=U",
            Path::new("/l/root/out"),
        );
        assert_eq!(
            args,
            [
                "test",
                "-project",
                "/l/root/fixtures/IOSFixture.xcodeproj",
                "-scheme",
                "IOSFixture",
                "-destination",
                "platform=iOS Simulator,id=U",
                "-derivedDataPath",
                "/l/root/out/ios-derived",
                "-resultBundlePath",
                "/l/root/out/ios.xcresult",
            ]
        );
        let build = xcodebuild_args(
            Path::new("/f"),
            Platform::Macos,
            Action::BuildForTesting,
            "d",
            Path::new("/o"),
        );
        assert_eq!(build[0], "build-for-testing");
        assert_eq!(build[2], "/f/MacFixture.xcodeproj");
        assert!(!build.iter().any(|a| a == "-resultBundlePath"), "{build:?}");
        let tests = xcodebuild_args(
            Path::new("/f"),
            Platform::Macos,
            Action::TestWithoutBuilding,
            "d",
            Path::new("/o"),
        );
        assert_eq!(tests[0], "test-without-building");
        assert_eq!(tests.last().map(String::as_str), Some("/o/macos.xcresult"));
        assert_eq!(
            log(Path::new("/o"), Platform::Macos, Action::Test),
            Path::new("/o/macos-xcodebuild.log")
        );
        assert_eq!(
            log(Path::new("/o"), Platform::Ios, Action::BuildForTesting),
            Path::new("/o/ios-build-for-testing-xcodebuild.log")
        );
    }

    /// The newest runtime by version numbers, not by string order (`iOS-18-10` after
    /// `iOS-18-9`), and the first iPhone in it, skipping iPads; a runtime without an
    /// iPhone falls back to an older one; nothing at all is `None`. Catches a pick by
    /// string order, an iPad chosen, and a runtime without phones taken anyway.
    #[test]
    fn the_simulator_is_the_first_iphone_of_the_newest_ios_runtime() {
        let rt = "com.apple.CoreSimulator.SimRuntime.";
        let list = json!({"devices": {
            format!("{rt}iOS-18-9"): [{"name": "iPhone 16", "udid": "OLD"}],
            format!("{rt}iOS-18-10"): [
                {"name": "iPad Pro", "udid": "PAD"},
                {"name": "iPhone 16 Pro", "udid": "NEW"},
                {"name": "iPhone 16", "udid": "SECOND"},
            ],
            format!("{rt}watchOS-11-0"): [{"name": "iPhone lookalike", "udid": "WATCH"}],
        }});
        assert_eq!(
            pick_simulator(&list),
            Some(Simulator {
                udid: "NEW".to_owned(),
                runtime: "iOS-18-10".to_owned(),
                name: "iPhone 16 Pro".to_owned(),
            })
        );
        let pads_only = json!({"devices": {
            format!("{rt}iOS-19-0"): [{"name": "iPad mini", "udid": "PAD"}],
            format!("{rt}iOS-18-0"): [{"name": "iPhone SE", "udid": "SE"}],
        }});
        assert_eq!(
            pick_simulator(&pads_only).map(|s| s.udid),
            Some("SE".to_owned())
        );
        assert_eq!(pick_simulator(&json!({"devices": {}})), None);
        assert_eq!(pick_simulator(&json!({})), None);
    }

    fn case(name: &str, result: &str) -> Value {
        json!({"nodeType": "Test Case", "name": name, "result": result})
    }

    /// Cases are found at any depth (plan, then bundle, then suite); the run passes only
    /// with exactly the three tests, each `Passed`. Catches a check that counts passes
    /// without the names, accepts a fourth test or a missing one, or reads only the
    /// top level.
    #[test]
    fn exactly_the_three_tests_must_pass() {
        let tree = |cases: Vec<Value>| {
            json!({"testNodes": [{"nodeType": "Test Plan", "children": [
                {"nodeType": "Unit test bundle", "children": [
                    {"nodeType": "Test Suite", "children": cases}]}]}]})
        };
        let all = tree(TESTS.iter().map(|t| case(t, "Passed")).collect());
        let cases = test_cases(&all);
        assert_eq!(cases.len(), 3, "{cases:?}");
        assert!(the_three_passed(&cases));

        let failed = tree(vec![
            case(TESTS[0], "Passed"),
            case(TESTS[1], "Failed"),
            case(TESTS[2], "Passed"),
        ]);
        assert!(!the_three_passed(&test_cases(&failed)));
        let two = tree(vec![case(TESTS[0], "Passed"), case(TESTS[1], "Passed")]);
        assert!(!the_three_passed(&test_cases(&two)));
        let other = tree(vec![
            case(TESTS[0], "Passed"),
            case(TESTS[1], "Passed"),
            case("testSomethingElse()", "Passed"),
        ]);
        assert!(!the_three_passed(&test_cases(&other)));
        let mut four: Vec<Value> = TESTS.iter().map(|t| case(t, "Passed")).collect();
        four.push(case("testSomethingElse()", "Passed"));
        assert!(!the_three_passed(&test_cases(&tree(four))));
        assert!(test_cases(&json!({})).is_empty());
    }

    /// Xcode's export name `<name>_<index>_<UUID>.png`; anything else is no screenshot.
    #[test]
    fn a_screenshot_is_named_by_its_export_name() {
        assert_eq!(screenshot_name("window_0_1A2B-3C4D.png"), Some("window"));
        assert_eq!(
            screenshot_name("window-no-pattern_2_ab-12.png"),
            Some("window-no-pattern")
        );
        assert_eq!(screenshot_name("a_b_1_ff.png"), Some("a_b"));
        assert_eq!(screenshot_name("window_0_1A2B.jpg"), None);
        assert_eq!(screenshot_name("window_x_1A2B.png"), None);
        assert_eq!(screenshot_name("window_0_XYZ.png"), None);
        assert_eq!(screenshot_name("window_0_.png"), None);
        assert_eq!(screenshot_name("window__1A.png"), None);
        assert_eq!(screenshot_name("_0_1A.png"), None);
        assert_eq!(screenshot_name("window.png"), None);
    }

    /// All three screenshots, each listed under its own test and exported to a
    /// non-empty file. Catches a check that passes with a screenshot listed but
    /// missing or empty on disk (a test without `keepAlways`), one listed under the
    /// wrong test, or a missing one.
    #[test]
    fn the_three_screenshots_must_be_kept() {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("xcode_fixture_screenshots");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        for f in ["w.png", "s.png", "n.png"] {
            std::fs::write(dir.join(f), b"png").expect("write");
        }
        std::fs::write(dir.join("empty.png"), b"").expect("write");
        let att = |human: &str, file: &str| json!({"suggestedHumanReadableName": human, "exportedFileName": file});
        let shows = "FixtureUITests/testScreenshotShowsPattern()";
        let left = "FixtureUITests/testPatternLeftOutIsNotFound()";
        let manifest = |screen_file: &str, left_test: &str| {
            json!([
                {"testIdentifier": shows, "attachments": [
                    att("window_0_AB.png", "w.png"), att("screen_1_CD.png", screen_file)]},
                {"testIdentifier": left_test, "attachments": [
                    att("window-no-pattern_0_EF.png", "n.png")]},
            ])
        };
        assert!(missing_screenshots(&manifest("s.png", left), &dir).is_empty());
        assert_eq!(
            missing_screenshots(&manifest("empty.png", left), &dir),
            [format!("{shows} screen")]
        );
        assert_eq!(
            missing_screenshots(&manifest("gone.png", left), &dir),
            [format!("{shows} screen")]
        );
        assert_eq!(
            missing_screenshots(&manifest("s.png", shows), &dir),
            [format!("{left} window-no-pattern")]
        );
        assert_eq!(missing_screenshots(&json!([]), &dir).len(), 3);
        assert_eq!(missing_screenshots(&json!({}), &dir).len(), 3);
        let report = |cases: Vec<(String, String)>, missing: Vec<String>| Report { cases, missing };
        let passed: Vec<(String, String)> = TESTS
            .iter()
            .map(|t| ((*t).to_owned(), "Passed".to_owned()))
            .collect();
        assert!(report(passed.clone(), Vec::new()).passed());
        assert!(!report(passed, vec!["x".to_owned()]).passed());
        assert!(!report(Vec::new(), Vec::new()).passed());
    }

    /// Each `KBF-FIXTURE` line once, from wherever xcodebuild's log puts it.
    #[test]
    fn the_fixture_lines_are_read_from_the_log() {
        let log = "Test Case started.\n\
                   t=1.0 KBF-FIXTURE tap=PASS tapped 1\n\
                   KBF-FIXTURE tap=PASS tapped 1\n\
                   KBF-FIXTURE screenshot=FAIL black\n";
        assert_eq!(
            fixture_lines(log),
            [
                "KBF-FIXTURE screenshot=FAIL black",
                "KBF-FIXTURE tap=PASS tapped 1"
            ]
        );
    }
}
