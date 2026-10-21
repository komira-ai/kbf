//! `cargo xtask gamma check-build` as the workflow runs it, with `xtask-fake` as komira's
//! `buck2` launcher. The fake leaves the arguments and the working directory it was given beside
//! it and prints the console a test wrote for it, so each test says what buck2 said and the
//! test reads what the verb did with it: what it passed, what it printed, what it appended to
//! the job summary and what it kept in the log.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const ENV: &str = "GAMMA_FARM_BUCKCONFIG";
const TARGETS: &str = "komira//src/a:a komira//src/b:b";

/// The farm's buckconfig as a test gives it: it names a host that no output may show.
const CONFIG: &str = "[buildbarn]\n  address = canary-config-host:8980\n";

/// The console of a build that ran everything on the farm or in its cache. The farm's name is
/// in the output, as it is in buck2's.
const CONSOLE: &str = "Connecting to canary-farm.tailnet.example:8980\n\
    Cache hits: 100%. Commands: 4 (cached: 4, remote: 0, local: 0)\n\
    BUILD SUCCEEDED\n";

/// What the verb prints and appends for [`CONSOLE`].
const SHOWN: &str = "Commands: 4 (cached: 4, remote: 0, local: 0)";

/// Words of the farm that nothing the verb prints or appends may hold.
const FARM: [&str; 3] = ["canary-farm", "canary-config-host", "tailnet.example"];

/// The exit code, stdout and stderr of a run.
type Ran = (Option<i32>, String, String);

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} is not in {text}");
}

fn lacks(text: &str, needle: &str) {
    assert!(!text.contains(needle), "{needle:?} is in {text}");
}

/// One test's world: a komira checkout whose `buck2` is the fake, saying `console`.
fn world(name: &str, console: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-gamma-check");
    let dir = TestDir::under(root, name);
    std::fs::create_dir_all(dir.join("komira")).expect("komira directory");
    std::fs::hard_link(FAKE, dir.join("komira/buck2")).expect("link the fake");
    std::fs::write(dir.join("komira/buck2-output"), console).expect("console");
    dir
}

/// The flags the workflow gives, with the files in `dir`.
fn flags(dir: &Path, targets: &str) -> Vec<String> {
    let path = |name: &str| s(&dir.join(name)).to_owned();
    vec![
        "gamma".to_owned(),
        "check-build".to_owned(),
        "--komira-dir".to_owned(),
        path("komira"),
        "--targets".to_owned(),
        targets.to_owned(),
        "--buckconfig-file".to_owned(),
        path("farm.buckconfig"),
        "--log".to_owned(),
        path("build.log"),
        "--summary".to_owned(),
        path("step-summary.md"),
    ]
}

/// `args` with the value of `flag` replaced.
fn with(mut args: Vec<String>, flag: &str, value: &str) -> Vec<String> {
    let at = args.iter().position(|a| a == flag).expect("the flag");
    args[at + 1] = value.to_owned();
    args
}

/// `cargo xtask <args>` with the farm's config in the environment (or not set).
fn xtask(args: &[String], config: Option<&str>) -> Ran {
    let mut cmd = Command::new(XTASK);
    cmd.args(args);
    match config {
        Some(text) => cmd.env(ENV, text),
        None => cmd.env_remove(ENV),
    };
    let out = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// The run of the workflow's flags in `dir`, with the config set.
fn check(dir: &Path) -> Ran {
    xtask(&flags(dir, TARGETS), Some(CONFIG))
}

/// Asserts that nothing of the farm is in what the verb printed or appended.
fn shows_no_farm(out: &str, err: &str, dir: &Path) {
    let summary = std::fs::read_to_string(dir.join("step-summary.md")).unwrap_or_default();
    for text in [out, err, summary.as_str()] {
        for name in FARM {
            lacks(text, name);
        }
    }
}

/// Proves the build the workflow asks for: buck2 runs in the checkout as `build --console
/// simple --config-file <the written config> <targets>` and nothing else (no flag that
/// bypasses the cache), the config is the environment's text, the output of both of its streams
/// is in the log and not in the step, and the step shows the Commands line and the verdict.
/// Catches: a wrong working directory, a flag added, the config not written or not passed, the
/// output printed, and one stream lost.
#[test]
fn a_build_that_ran_nothing_locally_passes_and_shows_only_the_counts() {
    let dir = world("passes", CONSOLE);
    std::fs::write(dir.join("komira/buck2-stdout"), "worker canary-farm ok\n").expect("stdout");
    let (code, out, err) = check(&dir);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, format!("{SHOWN}\nPASS\n"));
    assert_eq!(err, "");
    let want = format!("### gamma check\n\n{SHOWN}\n\nPASS\n");
    assert_eq!(read(&dir.join("step-summary.md")), want);
    assert_eq!(read(&dir.join("farm.buckconfig")), CONFIG);
    let komira = std::fs::canonicalize(dir.join("komira")).expect("komira");
    let config = s(&dir.join("farm.buckconfig")).to_owned();
    let want = format!(
        "{}: build --console simple --config-file {config} {TARGETS}\n",
        komira.display()
    );
    assert_eq!(read(&dir.join("komira/buck2-args")), want);
    let log = read(&dir.join("build.log"));
    has(&log, "canary-farm.tailnet.example");
    has(&log, "worker canary-farm ok");
    has(&log, "BUILD SUCCEEDED");
    shows_no_farm(&out, &err, &dir);
}

/// Catches: a success that ran three actions on this machine passing. The farm did not do the
/// work, so the check proved nothing; it fails, says why and still shows the counts.
#[test]
fn a_success_with_local_commands_fails_and_says_so() {
    let console = "Commands: 4 (cached: 1, remote: 0, local: 3)\nBUILD SUCCEEDED\n";
    let dir = world("local", console);
    let (code, out, err) = check(&dir);
    assert_eq!(code, Some(1));
    has(&err, "gamma check: local commands ran");
    assert_eq!(out, "");
    let summary = read(&dir.join("step-summary.md"));
    has(&summary, "Commands: 4 (cached: 1, remote: 0, local: 3)");
    has(&summary, "FAIL (local commands ran)");
}

/// Catches: `BUILD FAILED` passing, and a clean summary passing when buck2 itself exited with a
/// failure status (it printed `BUILD SUCCEEDED` and then failed).
#[test]
fn a_failed_build_and_a_failing_exit_both_fail() {
    let failed = "Commands: 2 (cached: 0, remote: 2, local: 0)\nBUILD FAILED\n";
    let dir = world("failed", failed);
    std::fs::write(dir.join("komira/buck2-exit"), "1").expect("exit");
    let (code, _, err) = check(&dir);
    assert_eq!(code, Some(1));
    has(&err, "gamma check: BUILD FAILED");
    has(&read(&dir.join("step-summary.md")), "FAIL (BUILD FAILED)");
    let dir = world("exit", CONSOLE);
    std::fs::write(dir.join("komira/buck2-exit"), "3").expect("exit");
    let (code, _, err) = check(&dir);
    assert_eq!(code, Some(1));
    has(&err, "buck2 exited with a failure status");
}

/// Catches: an output with no summary at all (buck2 killed, a crash before the end) passing, and
/// the step showing something in the place of the counts that are not there.
#[test]
fn an_output_with_no_summary_fails() {
    let dir = world("silent", "");
    let (code, _, err) = check(&dir);
    assert_eq!(code, Some(1));
    has(&err, "no BUILD SUCCEEDED or BUILD FAILED line");
    let summary = read(&dir.join("step-summary.md"));
    has(&summary, "Commands: not printed");
    has(&summary, "FAIL (no BUILD SUCCEEDED or BUILD FAILED line)");
}

/// Catches: `--no-remote-cache` reaching buck2 through the variable that names the targets, and
/// a flag in its place. The refusal is a usage error (exit 2) before anything is written or run:
/// there is no config, no log, no summary and no buck2 call.
#[test]
fn a_flag_among_the_targets_is_refused_before_anything_runs() {
    let refused = [
        "komira//a:a --no-remote-cache",
        "komira//a:a -c",
        "komira//a:a @argfile",
        " ",
    ];
    let files = [
        "komira/buck2-args",
        "farm.buckconfig",
        "build.log",
        "step-summary.md",
    ];
    for targets in refused {
        let dir = world("flag", CONSOLE);
        let (code, _, err) = xtask(&flags(&dir, targets), Some(CONFIG));
        assert_eq!(code, Some(2), "{targets}: {err}");
        has(&err, "--targets");
        for name in files {
            assert!(!dir.join(name).exists(), "{targets}: {name} exists");
        }
    }
}

/// Catches: a check that runs without the farm's config (it would build on this machine and
/// fail only by its local count, or pass a cached result of another config), a config path that
/// is relative to a directory the verb does not choose, and an empty config.
#[test]
fn the_config_must_be_given_and_its_path_absolute() {
    let dir = world("config", CONSOLE);
    let args = flags(&dir, TARGETS);
    for config in [None, Some(""), Some(" \n")] {
        let (code, _, err) = xtask(&args, config);
        assert_eq!(code, Some(2), "{config:?}: {err}");
        has(&err, ENV);
    }
    let relative = with(args, "--buckconfig-file", "farm.buckconfig");
    let (code, _, err) = xtask(&relative, Some(CONFIG));
    assert_eq!(code, Some(2));
    has(&err, "absolute");
    assert!(!dir.join("komira/buck2-args").exists());
}

/// Catches: a missing checkout, or a checkout without its launcher, passing or panicking; each
/// is an error that names what could not be done.
#[test]
fn a_missing_checkout_or_launcher_is_an_error() {
    let dir = world("missing", CONSOLE);
    let gone = s(&dir.join("no-such-checkout")).to_owned();
    let args = with(flags(&dir, TARGETS), "--komira-dir", &gone);
    let (code, _, err) = xtask(&args, Some(CONFIG));
    assert_eq!(code, Some(1));
    has(&err, "open");
    std::fs::remove_file(dir.join("komira/buck2")).expect("remove the launcher");
    let (code, _, err) = check(&dir);
    assert_eq!(code, Some(1));
    has(&err, "buck2");
}

/// Catches: an output file that cannot be written ending as a pass: the config, the log and the
/// summary each in a directory that is not there.
#[test]
fn a_file_that_cannot_be_written_is_an_error() {
    let dir = world("unwritable", CONSOLE);
    let nowhere = s(&dir.join("no/dir/file")).to_owned();
    for (flag, what) in [
        ("--buckconfig-file", "write"),
        ("--log", "write"),
        ("--summary", "append"),
    ] {
        let args = with(flags(&dir, TARGETS), flag, &nowhere);
        let (code, _, err) = xtask(&args, Some(CONFIG));
        assert_eq!(code, Some(1), "{flag}: {err}");
        has(&err, what);
        has(&err, "no/dir/file");
    }
}
