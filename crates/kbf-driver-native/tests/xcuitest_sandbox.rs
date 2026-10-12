//! The XCUITest fixtures (`tools/spike/macos/fixtures`, `tests/support/xcode_fixture.rs`)
//! run as a native driver action that asks for the network: `xcodebuild` under the
//! sandbox profile the driver builds ([`Isolation::wrap`] with BASE_PROFILE,
//! NO_DEVICE_RULES and this user's folder rules: the library's bytes, not a copy), in a
//! lease directory with the lease's variables. The deny lines the sandbox logs for the
//! run's processes are read back with `log show` and checked.
//!
//! What the tests pin, measured on the hosted `macos-15` runner with Xcode 16.4
//! (`docs/spikes/macos-p0.md`):
//! - the iOS fixture builds and passes its three UI tests on a simulator under the
//!   profile, both built there and built outside it first;
//! - the macOS fixture's UI tests do not run: Launch Services is refused the test
//!   runner (`(deny lsopen)`), which the profile keeps on purpose (`network.rs`);
//! - every run under the profile is refused what the profile denies and Xcode asks
//!   for anyway: a launchd job, a preference write, CoreDevice and `usbmuxd`, and
//!   `xcresulttool`'s write into the user's temporary folder outside the names the
//!   profile opens there; and no run is refused Swift Build's scratch directory.
//!
//! So a profile that drops one of those denies, or opens more of the temporary folder,
//! turns a test red here as well as in the driver's unit and integration tests.
//!
//! Each run is `xcodebuild`, run as the driver runs an action: in the lease's input
//! root (which holds a copy of the fixtures, so the projects Xcode opens are inside the
//! lease), stdin from `/dev/null`, only `PATH=/usr/bin:/bin:/usr/sbin:/sbin` and the
//! lease's variables in its environment, leading its own process group. While it runs
//! the process table is sampled every 0.2 s (`procs::Tracker`, the driver's own), so
//! the run's processes are its root and every process seen descending from it; a
//! process that starts and ends between two samples is missed. A deny line from any
//! other process is counted, not checked.
//!
//! Controls: the same run under `(version 1)(deny default)`, through the same wrapping,
//! must fail with a deny line from its processes (else no sandbox was applied or the
//! log reader sees nothing), and each fixture must pass without a sandbox in the same
//! lease layout (else the instrument is broken, not the profile).
//!
//! macOS with Xcode and an iOS simulator runtime only, so every test that runs Xcode is
//! `#[ignore]`d; the `spike-xcuitest` workflow runs them with `--ignored`. They hold
//! one lock, so one Xcode run goes at a time, and no other thread of this binary spawns
//! while `log show` or `xcresulttool` output is read through a pipe (on macOS a child
//! spawned on another thread can inherit the pipe and hold it open: issue #481). Every
//! lease, log, bundle and deny list stays under `target/tmp/xcuitest` for the
//! workflow's artifact.

#![cfg(target_os = "macos")]

#[path = "support/xcode_fixture.rs"]
mod xcode_fixture;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kbf_driver_native::network::{Isolation, Network, SANDBOX_EXEC};
use kbf_driver_native::procs;
use kbf_driver_native::user_folders::UserFolders;
use xcode_fixture::{Action, Platform, Report};

/// The action's `PATH`.
const ACTION_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// How long one `xcodebuild` run may take before its processes are killed.
const RUN_TIMEOUT: Duration = Duration::from_secs(1200);

/// How long after a run its log window ends, so a report logged late is in it.
const LOG_SETTLE: Duration = Duration::from_secs(5);

/// How long the log is read again for deny lines that have not shown yet.
const LOG_WAIT: Duration = Duration::from_secs(30);

/// The control profile.
const DENY_DEFAULT: &str = "(version 1)\n(deny default)\n";

/// One Xcode run at a time (see the module docs).
fn xcode() -> MutexGuard<'static, ()> {
    static XCODE: Mutex<()> = Mutex::new(());
    XCODE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where every lease and result of this binary goes.
fn results() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("xcuitest")
}

/// A lease directory as the driver makes it, with the fixtures in its input root.
struct Lease {
    /// The lease directory, every link resolved: the profile's `KBF_LEASE`.
    dir: PathBuf,
    /// The input root: the run's working directory.
    root: PathBuf,
    /// The fixtures' copy in the input root.
    fixtures: PathBuf,
    /// Where the run's derived data, bundle and log go, in the input root.
    out: PathBuf,
    /// The lease's variables (`kbf_driver_native::home`).
    env: Vec<(&'static str, OsString)>,
}

impl Lease {
    /// A fresh lease `name` under [`results`].
    fn new(name: &str) -> Self {
        let dir = results().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("lease dir");
        let dir = std::fs::canonicalize(&dir).expect("lease dir");
        let env = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(kbf_driver_native::home::make(&dir))
            .expect("the lease's home");
        let root = dir.join("root");
        let fixtures = root.join("fixtures");
        copy_tree(&xcode_fixture::fixtures(), &fixtures);
        let out = root.join("out");
        std::fs::create_dir_all(&out).expect("out dir");
        Self {
            dir,
            root,
            fixtures,
            out,
            env,
        }
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("copy: dir");
    for entry in std::fs::read_dir(from).expect("copy: read dir") {
        let entry = entry.expect("copy: entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("copy: type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy: file");
        }
    }
}

/// What a run is wrapped in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sandbox {
    /// Nothing: a control.
    Off,
    /// The profile the driver builds for an action with the network on.
    Driver,
    /// `(deny default)`, handed to `sandbox-exec` the way the driver's profile is.
    DenyDefault,
}

/// The command that runs `xcodebuild args` in `lease` as the driver runs an action,
/// under `sandbox`, with its output to `log`.
fn command(lease: &Lease, sandbox: Sandbox, args: &[String], log: &Path) -> Command {
    let xcodebuild = PathBuf::from("/usr/bin/xcodebuild");
    let (program, args) = match sandbox {
        Sandbox::Off => (xcodebuild, args.to_vec()),
        Sandbox::Driver | Sandbox::DenyDefault => {
            let rules = UserFolders::detect()
                .as_ref()
                .map(UserFolders::rules)
                .unwrap_or_default();
            let (program, mut wrapped) = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC)).wrap(
                Network::On,
                &lease.dir,
                &rules,
                xcodebuild,
                args,
            );
            assert!(
                wrapped.len() > 4 && wrapped[0] == "-D" && wrapped[2] == "-p",
                "Isolation::wrap no longer starts -D <param> -p <profile>: {wrapped:?}"
            );
            if sandbox == Sandbox::DenyDefault {
                DENY_DEFAULT.clone_into(&mut wrapped[3]);
            }
            (program, wrapped)
        }
    };
    let log = File::create(log).expect("log file");
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .env("PATH", ACTION_PATH)
        .envs(lease.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&lease.root)
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("log file"))
        .stderr(log)
        .process_group(0);
    cmd
}

/// A finished run: its exit code (`None` when a signal ended it), every pid seen in its
/// process tree, and its log window in local time, as `log show` reads it.
struct Run {
    exit: Option<i32>,
    pids: BTreeSet<i32>,
    since: String,
    until: String,
}

/// This Mac's local time to the second, as `log show --start` takes it.
fn local_time() -> String {
    let out = Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .expect("date");
    String::from_utf8(out.stdout)
        .expect("date: UTF-8")
        .trim()
        .to_owned()
}

/// Runs `cmd`, sampling the process table until it ends, or killing every process seen
/// in its tree after [`RUN_TIMEOUT`].
fn tracked(mut cmd: Command) -> Run {
    let since = local_time();
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn");
    let root = i32::try_from(child.id()).expect("pid");
    let mut tracker = procs::Tracker::new(root, i32::try_from(std::process::id()).expect("pid"));
    let mut pids = BTreeSet::from([root]);
    let status = loop {
        let members = tracker.members(&procs::snapshot().expect("process table"));
        pids.extend(members.iter().map(|p| p.pid));
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if start.elapsed() > RUN_TIMEOUT {
            eprintln!("run timed out after {RUN_TIMEOUT:?}: killing its tree");
            procs::kill_all(&tracker, &members);
            break child.wait().expect("wait");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    std::thread::sleep(LOG_SETTLE);
    Run {
        exit: status.code(),
        pids,
        since,
        until: local_time(),
    }
}

/// One line the sandbox logged: `<process>(<pid>) <kind>(<n>) <operation> [<target>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SandboxLine {
    pid: i32,
    process: String,
    /// `deny` or `allow`.
    kind: String,
    operation: String,
    target: String,
}

/// The sandbox's report in one line of `log show --style compact`: what follows
/// `Sandbox: `, up to the first `(<digits>) deny` or `(<digits>) allow` (a process
/// name may hold parentheses and spaces), then the operation and the rest of the line
/// as its target.
fn parse_sandbox_line(line: &str) -> Option<SandboxLine> {
    let s = &line[line.find("Sandbox: ")? + "Sandbox: ".len()..];
    let mut from = 0;
    loop {
        let open = from + s[from..].find('(')?;
        let close = open + s[open..].find(')')?;
        let digits = &s[open + 1..close];
        let rest = &s[close + 1..];
        let kind = ["deny", "allow"].into_iter().find(|k| {
            rest.strip_prefix(' ')
                .and_then(|r| r.strip_prefix(k))
                .is_some_and(|r| r.is_empty() || r.starts_with(['(', ' ']))
        });
        if let (Some(kind), Ok(pid)) = (kind, digits.parse::<i32>())
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            let (_, after) = rest[1..].split_once(' ')?;
            let (operation, target) = after.split_once(' ').unwrap_or((after, ""));
            return Some(SandboxLine {
                pid,
                process: s[..open].to_owned(),
                kind: kind.to_owned(),
                operation: operation.to_owned(),
                target: target.trim().to_owned(),
            });
        }
        from = open + 1;
    }
}

/// The deny lines of one run: those from its processes, and how many came from others.
#[derive(Debug, Default)]
struct Denies {
    tree: Vec<SandboxLine>,
    other: usize,
}

/// The deny lines in `log`, split by whether their pid is in `pids`.
fn split_denies(log: &str, pids: &BTreeSet<i32>) -> Denies {
    let mut denies = Denies::default();
    for line in log.lines().filter_map(parse_sandbox_line) {
        if line.kind != "deny" {
            continue;
        }
        if pids.contains(&line.pid) {
            denies.tree.push(line);
        } else {
            denies.other += 1;
        }
    }
    denies
}

/// The deny lines `log show` holds for `run`'s window, read again every 2 s for up to
/// [`LOG_WAIT`] until `done` holds; written to `<results>/<name>-denies.txt`.
fn denies(name: &str, run: &Run, done: impl Fn(&Denies) -> bool) -> Denies {
    let waited = Instant::now();
    let denies = loop {
        let out = Command::new("/usr/bin/log")
            .args([
                "show", "--style", "compact", "--start", &run.since, "--end", &run.until,
            ])
            .args(["--predicate", "sender == \"Sandbox\""])
            .stdin(Stdio::null())
            .output()
            .expect("log show");
        let denies = split_denies(&String::from_utf8_lossy(&out.stdout), &run.pids);
        if done(&denies) || waited.elapsed() >= LOG_WAIT {
            break denies;
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let mut text = format!("# {} lines from other processes\n", denies.other);
    for l in &denies.tree {
        let _ = writeln!(
            text,
            "{}\t{}\t{}\t{}",
            l.pid, l.process, l.operation, l.target
        );
    }
    std::fs::write(results().join(format!("{name}-denies.txt")), text).expect("denies file");
    denies
}

/// A deny line a run under the driver's profile must show, and the rule it proves
/// still applies.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    /// `(deny job-creation)`.
    JobCreation,
    /// `(deny user-preference-write)`.
    PreferenceWrite,
    /// NO_DEVICE_RULES' CoreDevice prefix.
    CoreDevice,
    /// NO_DEVICE_RULES' `usbmuxd` socket.
    Usbmuxd,
    /// The user folder rules: a write into the temporary folder (this path) outside
    /// `TemporaryItems/`, `xcrun_db` and `swbuild.tmp.*`.
    OtherTempWrite(PathBuf),
    /// `(deny lsopen)`.
    Lsopen,
}

impl Expect {
    fn matches(&self, l: &SandboxLine) -> bool {
        match self {
            Self::JobCreation => l.operation == "job-creation",
            Self::PreferenceWrite => l.operation == "user-preference-write",
            Self::CoreDevice => {
                l.operation == "mach-lookup" && l.target.starts_with("com.apple.CoreDevice.")
            }
            Self::Usbmuxd => {
                l.operation == "network-outbound" && l.target == "/private/var/run/usbmuxd"
            }
            Self::OtherTempWrite(temp) => {
                let Some(entry) = Path::new(&l.target)
                    .strip_prefix(temp)
                    .ok()
                    .and_then(|p| p.iter().next())
                    .and_then(|e| e.to_str())
                else {
                    return false;
                };
                l.operation.starts_with("file-write")
                    && entry != "TemporaryItems"
                    && !entry.starts_with("xcrun_db")
                    && !entry.starts_with("swbuild.tmp.")
            }
            Self::Lsopen => l.operation == "lsopen",
        }
    }

    /// The defect a missing line points at.
    fn defect(&self) -> &'static str {
        match self {
            Self::JobCreation => "the profile no longer denies job-creation",
            Self::PreferenceWrite => "the profile no longer denies user-preference-write",
            Self::CoreDevice => "NO_DEVICE_RULES' CoreDevice rule is gone from the profile",
            Self::Usbmuxd => "NO_DEVICE_RULES' usbmuxd rule is gone from the profile",
            Self::OtherTempWrite(_) => {
                "the user folder rules open more of the temporary folder than its few names"
            }
            Self::Lsopen => "the profile no longer denies lsopen",
        }
    }
}

/// The expectations in `want` no line of `denies` meets.
fn missing<'a>(denies: &Denies, want: &'a [Expect]) -> Vec<&'a Expect> {
    want.iter()
        .filter(|e| !denies.tree.iter().any(|l| e.matches(l)))
        .collect()
}

/// A deny of Swift Build's scratch directory (`T/swbuild.tmp.<random>`), which the
/// user folder rules open.
fn swift_build_scratch_denied(denies: &Denies) -> Option<&SandboxLine> {
    denies
        .tree
        .iter()
        .find(|l| l.operation.starts_with("file-write") && l.target.contains("/swbuild.tmp."))
}

/// What one run of a fixture gave.
struct Arm {
    name: String,
    exit: Option<i32>,
    report: Option<Report>,
    denies: Denies,
    log: PathBuf,
}

impl Arm {
    /// `xcodebuild` exited 0 and, for a testing action, the bundle passed.
    fn passed(&self) -> bool {
        self.exit == Some(0) && self.report.as_ref().is_none_or(Report::passed)
    }

    /// What a failed assertion prints: the result, the tests, the fixtures' own lines,
    /// the end of the log and the run's distinct deny lines.
    fn describe(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let mut text = format!("{}: xcodebuild exit {:?}\n", self.name, self.exit);
        if let Some(report) = &self.report {
            let _ = writeln!(
                text,
                "tests: {:?}\nmissing: {:?}",
                report.cases, report.missing
            );
        }
        for l in xcode_fixture::fixture_lines(&log) {
            let _ = writeln!(text, "{l}");
        }
        let errors: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("error:") || l.contains("failed") || l.contains("FAIL"))
            .take(20)
            .collect();
        let _ = writeln!(text, "log errors:\n{}", errors.join("\n"));
        let distinct: BTreeSet<String> = self
            .denies
            .tree
            .iter()
            .map(|l| format!("{} {} {}", l.process, l.operation, l.target))
            .collect();
        let _ = writeln!(
            text,
            "{} deny lines from the run ({} distinct), {} from other processes:",
            self.denies.tree.len(),
            distinct.len(),
            self.denies.other
        );
        for l in distinct.iter().take(40) {
            let _ = writeln!(text, "  {l}");
        }
        text
    }

    /// Fails unless every line of `want` showed, and no Swift Build scratch deny did.
    fn assert_denies(&self, want: &[Expect]) {
        let missing = missing(&self.denies, want);
        assert!(
            missing.is_empty(),
            "missing deny lines: {:?}\n{}",
            missing.iter().map(|e| e.defect()).collect::<Vec<_>>(),
            self.describe()
        );
        if let Some(l) = swift_build_scratch_denied(&self.denies) {
            panic!(
                "Swift Build's scratch directory was denied ({l:?}): the user folder rules \
                 lost it\n{}",
                self.describe()
            );
        }
    }
}

/// Runs `action` on `platform`'s fixture in a fresh lease `name` under `sandbox`, after
/// an unsandboxed `build-for-testing` in the same lease when `action` is
/// [`Action::TestWithoutBuilding`] (which must pass, or the instrument is broken); then
/// reads the bundle outside the sandbox, and the run's deny lines until `want` shows.
fn arm(name: &str, platform: Platform, action: Action, sandbox: Sandbox, want: &[Expect]) -> Arm {
    let lease = Lease::new(name);
    let destination = xcode_fixture::destination(platform).expect("destination");
    eprintln!(
        "{name}: {} {} on {destination}",
        platform.project(),
        action.name()
    );
    if action == Action::TestWithoutBuilding {
        let build = Action::BuildForTesting;
        let log = xcode_fixture::log(&lease.out, platform, build);
        let args = xcode_fixture::xcodebuild_args(
            &lease.fixtures,
            platform,
            build,
            &destination,
            &lease.out,
        );
        let run = tracked(command(&lease, Sandbox::Off, &args, &log));
        assert_eq!(
            run.exit,
            Some(0),
            "BROKEN: the unsandboxed build-for-testing failed; see {}",
            log.display()
        );
    }
    let log = xcode_fixture::log(&lease.out, platform, action);
    let args =
        xcode_fixture::xcodebuild_args(&lease.fixtures, platform, action, &destination, &lease.out);
    let run = tracked(command(&lease, sandbox, &args, &log));
    let report = action
        .tests()
        .then(|| xcode_fixture::report(&lease.out, platform));
    let denies = denies(name, &run, |d| match sandbox {
        Sandbox::Off => true,
        Sandbox::DenyDefault => !d.tree.is_empty(),
        Sandbox::Driver => missing(d, want).is_empty(),
    });
    let arm = Arm {
        name: name.to_owned(),
        exit: run.exit,
        report,
        denies,
        log,
    };
    eprint!("{}", arm.describe());
    arm
}

/// The deny lines every run under the driver's profile shows, on either fixture.
fn every_run() -> Vec<Expect> {
    vec![
        Expect::JobCreation,
        Expect::PreferenceWrite,
        Expect::CoreDevice,
        Expect::Usbmuxd,
    ]
}

/// [`every_run`], and `xcresulttool`'s write into the temporary folder, which only the
/// iOS runs, whose tests all run, show every time.
fn ios_run() -> Vec<Expect> {
    let temp = UserFolders::detect()
        .expect("this user's folders")
        .temp()
        .to_owned();
    let mut want = every_run();
    want.push(Expect::OtherTempWrite(temp));
    want
}

/// [`every_run`], and Launch Services refused the macOS UI-test runner.
fn macos_run() -> Vec<Expect> {
    let mut want = every_run();
    want.push(Expect::Lsopen);
    want
}

/// The control for every sandboxed run: under `(deny default)`, handed over exactly as
/// the driver's profile is, `xcodebuild` cannot start, and the log shows a deny line
/// from the run. Catches a harness that applies no sandbox (every run would pass
/// unrefused) or whose log reader sees nothing (every expected deny would be
/// "missing" for the wrong reason).
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn under_deny_default_xcodebuild_is_refused_and_the_log_shows_it() {
    let _xcode = xcode();
    let arm = arm(
        "deny_default",
        Platform::Macos,
        Action::Test,
        Sandbox::DenyDefault,
        &[],
    );
    assert_ne!(
        arm.exit,
        Some(0),
        "BROKEN: no sandbox was applied\n{}",
        arm.describe()
    );
    assert!(
        !arm.denies.tree.is_empty(),
        "BROKEN: no deny line from the run under (deny default)\n{}",
        arm.describe()
    );
}

/// The iOS fixture builds and passes its three UI tests, screenshots kept, under the
/// driver's profile, with every expected deny line. Catches a profile that stops
/// `xcodebuild` (Swift Build's scratch directory refused, the failure PR #425 fixed),
/// and a profile missing a deny or opening the temporary folder wider.
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn the_ios_fixture_builds_and_passes_under_the_drivers_profile() {
    let _xcode = xcode();
    let arm = arm(
        "driver.ios",
        Platform::Ios,
        Action::Test,
        Sandbox::Driver,
        &ios_run(),
    );
    assert!(arm.passed(), "{}", arm.describe());
    arm.assert_denies(&ios_run());
}

/// The iOS UI tests alone, from an unsandboxed build, pass under the driver's profile:
/// what the tests need, past what the build needs. Catches what
/// [`the_ios_fixture_builds_and_passes_under_the_drivers_profile`] does, for the
/// test-only run.
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn the_ios_ui_tests_pass_under_the_drivers_profile_after_an_unsandboxed_build() {
    let _xcode = xcode();
    let want = ios_run();
    let arm = arm(
        "driver_tests.ios",
        Platform::Ios,
        Action::TestWithoutBuilding,
        Sandbox::Driver,
        &want,
    );
    assert!(arm.passed(), "{}", arm.describe());
    arm.assert_denies(&want);
}

/// The macOS fixture under the driver's profile: its UI tests do not run, because
/// Launch Services is refused the test runner, and every other expected deny shows.
/// Catches a profile that allows `lsopen` (which lets any action start an application
/// outside the sandbox, `tests/launchd.rs`; with it allowed the runner still exited
/// before checking in, so only the deny line tells), and one missing another deny.
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn the_macos_ui_tests_stop_on_lsopen_under_the_drivers_profile() {
    let _xcode = xcode();
    let want = macos_run();
    let arm = arm(
        "driver.macos",
        Platform::Macos,
        Action::Test,
        Sandbox::Driver,
        &want,
    );
    assert!(
        !arm.passed(),
        "the macOS UI tests passed under the profile\n{}",
        arm.describe()
    );
    arm.assert_denies(&want);
}

/// [`the_macos_ui_tests_stop_on_lsopen_under_the_drivers_profile`] for the tests alone,
/// from an unsandboxed build.
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn the_macos_ui_tests_stop_on_lsopen_after_an_unsandboxed_build() {
    let _xcode = xcode();
    let want = macos_run();
    let arm = arm(
        "driver_tests.macos",
        Platform::Macos,
        Action::TestWithoutBuilding,
        Sandbox::Driver,
        &want,
    );
    assert!(
        !arm.passed(),
        "the macOS UI tests passed under the profile\n{}",
        arm.describe()
    );
    arm.assert_denies(&want);
}

/// The controls: each fixture builds and passes its three UI tests, screenshots kept,
/// with no sandbox, in the same lease layout and environment. Catches a broken
/// instrument (fixture, runner image, simulator or bundle check), which would make
/// every sandboxed result meaningless.
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn both_fixtures_pass_without_a_sandbox() {
    let _xcode = xcode();
    for platform in [Platform::Ios, Platform::Macos] {
        let name = format!("control.{}", platform.name());
        let arm = arm(&name, platform, Action::Test, Sandbox::Off, &[]);
        assert!(
            arm.passed(),
            "BROKEN: an unsandboxed control failed\n{}",
            arm.describe()
        );
    }
}

/// The fixtures' pixel checker (`UITests/PatternCheck.swift`) against synthetic images
/// (`selftest/main.swift`): the pattern on a light and a dark background and among other
/// pattern-coloured pixels is found; black, a wallpaper of hue bands, the quadrants in
/// the wrong order, three quadrants and scattered quadrants are not. Catches a checker
/// that passes any screenshot, which would make `testScreenshotShowsPattern` prove
/// nothing.
#[test]
#[ignore = "macOS with Xcode and an iOS simulator; the spike-xcuitest workflow"]
fn the_pattern_checker_finds_the_pattern_and_only_the_pattern() {
    let _xcode = xcode();
    let (passed, output) =
        xcode_fixture::pattern_checker_selftest(&results().join("selftest")).expect("selftest");
    assert!(passed, "{output}");
}

mod pure {
    use super::*;

    fn line(text: &str) -> SandboxLine {
        parse_sandbox_line(text).unwrap_or_else(|| panic!("parses: {text}"))
    }

    /// A compact `log show` line with each shape the runs report: a target with
    /// spaces, none at all, a process name with parentheses and spaces, an allow line;
    /// anything without a `(<pid>) deny|allow` report is skipped. Catches a parser
    /// that cuts the target at its first space, takes `(Networking)` for the pid, or
    /// reads a non-sandbox line as a deny.
    #[test]
    fn a_sandbox_report_is_split_into_process_operation_and_target() {
        let pre = "2026-10-11 15:28:10.123 E  kernel[0:1a2b] (Sandbox) Sandbox: ";
        assert_eq!(
            line(&format!(
                "{pre}xcodebuild(4567) deny(1) mach-lookup com.apple.CoreDevice.CoreDeviceService (xpc)"
            )),
            SandboxLine {
                pid: 4567,
                process: "xcodebuild".to_owned(),
                kind: "deny".to_owned(),
                operation: "mach-lookup".to_owned(),
                target: "com.apple.CoreDevice.CoreDeviceService (xpc)".to_owned(),
            }
        );
        let lsopen = line(&format!("{pre}xcodebuild(12) deny(1) lsopen"));
        assert_eq!(
            (lsopen.operation.as_str(), lsopen.target.as_str()),
            ("lsopen", "")
        );
        let odd = line(&format!(
            "{pre}Web Content (Networking)(99) deny(2) file-write-create /a b"
        ));
        assert_eq!(odd.process, "Web Content (Networking)");
        assert_eq!((odd.pid, odd.target.as_str()), (99, "/a b"));
        assert_eq!(
            line(&format!("{pre}x(1) allow(0) file-read-data /f")).kind,
            "allow"
        );
        assert_eq!(parse_sandbox_line("no report here"), None);
        assert_eq!(
            parse_sandbox_line(&format!("{pre}x(1) denied something")),
            None
        );
        assert_eq!(parse_sandbox_line(&format!("{pre}x(abc) deny(1) op")), None);
        assert_eq!(parse_sandbox_line(&format!("{pre}x(1) deny(1)")), None);
    }

    /// Only deny lines count, and only those from the run's pids are the run's.
    /// Catches every process's denies counted as the run's (another job's denies
    /// would satisfy an expectation), and allow lines counted as denies.
    #[test]
    fn deny_lines_are_the_runs_only_by_pid() {
        let pre = "t Sandbox: ";
        let log = format!(
            "{pre}xcodebuild(10) deny(1) lsopen\n{pre}other(20) deny(1) lsopen\n\
             {pre}xcodebuild(10) allow(1) job-creation\nunrelated\n{pre}child(11) deny(1) job-creation\n"
        );
        let denies = split_denies(&log, &BTreeSet::from([10, 11]));
        let ops: Vec<&str> = denies.tree.iter().map(|l| l.operation.as_str()).collect();
        assert_eq!(ops, ["lsopen", "job-creation"]);
        assert_eq!(denies.other, 1);
    }

    fn deny(operation: &str, target: &str) -> SandboxLine {
        SandboxLine {
            pid: 1,
            process: "p".to_owned(),
            kind: "deny".to_owned(),
            operation: operation.to_owned(),
            target: target.to_owned(),
        }
    }

    /// Each expectation matches its rule's lines and nothing else; the temporary-folder
    /// one only a write to another entry of that folder. Catches an expectation that
    /// any line meets (it would never go red), and a temporary-folder one met by the
    /// names the profile opens or by a write elsewhere.
    #[test]
    fn each_expected_deny_matches_only_its_rule() {
        let t = PathBuf::from("/private/var/folders/ab/cd/T");
        let other = Expect::OtherTempWrite(t.clone());
        assert!(other.matches(&deny(
            "file-write-create",
            "/private/var/folders/ab/cd/T/1A2B-3C"
        )));
        for target in [
            "/private/var/folders/ab/cd/T/TemporaryItems/x",
            "/private/var/folders/ab/cd/T/xcrun_db-AbC",
            "/private/var/folders/ab/cd/T/swbuild.tmp.Xy12",
            "/private/var/folders/ab/cd/C/com.apple.DTServiceHub",
            "/private/var/folders/ab/cd/T",
        ] {
            assert!(
                !other.matches(&deny("file-write-create", target)),
                "{target}"
            );
        }
        assert!(!other.matches(&deny("file-read-data", "/private/var/folders/ab/cd/T/1A2B")));
        let cases = [
            (Expect::JobCreation, deny("job-creation", "")),
            (
                Expect::PreferenceWrite,
                deny("user-preference-write", "com.apple.dt.xcodebuild"),
            ),
            (
                Expect::CoreDevice,
                deny(
                    "mach-lookup",
                    "com.apple.CoreDevice.CoreDeviceService (xpc)",
                ),
            ),
            (
                Expect::Usbmuxd,
                deny("network-outbound", "/private/var/run/usbmuxd"),
            ),
            (Expect::Lsopen, deny("lsopen", "")),
        ];
        for (i, (expect, _)) in cases.iter().enumerate() {
            for (j, (_, l)) in cases.iter().enumerate() {
                assert_eq!(expect.matches(l), i == j, "{expect:?} on {l:?}");
            }
            assert!(!expect.defect().is_empty());
        }
        assert!(!Expect::CoreDevice.matches(&deny("mach-lookup", "com.apple.remoted")));
        assert!(!Expect::Usbmuxd.matches(&deny("network-outbound", "/var/run/usbmuxd")));
        assert!(!other.defect().is_empty());
    }

    /// A run is judged by what is missing, and a Swift Build scratch deny is found.
    #[test]
    fn missing_lists_each_unmet_expectation() {
        let denies = Denies {
            tree: vec![
                deny("lsopen", ""),
                deny("file-write-create", "/v/T/swbuild.tmp.Ab/x"),
            ],
            other: 3,
        };
        let want = [Expect::Lsopen, Expect::JobCreation];
        assert_eq!(missing(&denies, &want), [&Expect::JobCreation]);
        assert!(swift_build_scratch_denied(&denies).is_some());
        assert!(swift_build_scratch_denied(&Denies::default()).is_none());
    }
}
