//! `prove-farm`: the flags it accepts, the configuration it renders, the way it reads Buck2's
//! summary line (on recorded samples) and its verdict, with Buck2 replaced by a closure; and
//! the binary for the notice and the exit codes. The proof against a real farm is the
//! release workflow's.

use std::cell::RefCell;
use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;
use xtask::Error;
use xtask::pkg_linux::Out;
use xtask::prove_farm::{
    Args, CONFIG_FILE, Counts, Problem, Settings, build_args, judge, parse_commands,
    parse_properties, process, render_buckconfig, run, run_with,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-prove-farm"),
        name,
    )
}

fn args(project: &Path) -> Args {
    Args {
        address: "grpc://farm.example:8980".to_owned(),
        instance: "main".to_owned(),
        tls: "true".to_owned(),
        properties: "OSFamily=linux,ISA=x86-64".to_owned(),
        buck2: "buck2".into(),
        project: project.to_owned(),
        nonce: "77-1".to_owned(),
    }
}

fn pair(k: &str, v: &str) -> (String, String) {
    (k.to_owned(), v.to_owned())
}

/// A summary line as Buck2 prints it after a build.
const REMOTE_ONLY: &str =
    "BUILD SUCCEEDED\nCache hits: 0%\nCommands: 4 (cached: 0, remote: 4, local: 0)\n";

fn out(ok: bool, stdout: &str, stderr: &str) -> Result<Out, Error> {
    Ok(Out {
        ok,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    })
}

#[test]
fn properties_are_a_comma_list_of_key_value_pairs() {
    // Catches: a platform set read wrongly (an `=` inside a value cut, spaces kept, order lost).
    assert_eq!(
        parse_properties("OSFamily=linux, ISA = x86-64 ,container-image=docker://x/y:1=2"),
        Ok(vec![
            pair("OSFamily", "linux"),
            pair("ISA", "x86-64"),
            pair("container-image", "docker://x/y:1=2"),
        ])
    );
    assert_eq!(parse_properties("a=b"), Ok(vec![pair("a", "b")]));
}

#[test]
fn properties_that_are_empty_or_malformed_are_refused_by_name() {
    // Catches: an empty variable accepted as "no properties" (a platform no worker matches), a
    // trailing comma, an entry without `=`, an empty key or value, a repeated key, and a value
    // that would end the configuration line.
    assert_eq!(parse_properties(""), Err(Problem::NoProperties));
    assert_eq!(parse_properties("  "), Err(Problem::NoProperties));
    for bad in ["a", "a=", "=b", "a=b,", ",a=b", "a=b,,c=d", "a=b,c"] {
        assert!(
            matches!(parse_properties(bad), Err(Problem::Property(_))),
            "{bad}"
        );
    }
    assert_eq!(
        parse_properties("a=b,c=d,a=e"),
        Err(Problem::DuplicateProperty("a".to_owned()))
    );
    for bad in ["a=b c", "a b=c", "a=b\nc=d"] {
        assert!(
            matches!(
                parse_properties(bad),
                Err(Problem::Unsafe { .. } | Problem::Property(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        parse_properties("a=b\u{7}"),
        Err(Problem::Unsafe { .. })
    ));
}

#[test]
fn the_settings_are_checked_before_anything_is_written() {
    // Catches: a `grpcs://` or schemeless address (Buck2 refuses it mid-build), TLS left empty
    // or spelled loosely, a nonce that is empty or could split a line, an instance name or
    // address carrying a newline (a way to add configuration), and valid flags refused.
    let dir = Path::new("p");
    let ok = Settings::of(&args(dir)).expect("valid");
    assert_eq!(
        ok,
        Settings {
            address: "grpc://farm.example:8980".to_owned(),
            instance: "main".to_owned(),
            tls: true,
            properties: vec![pair("OSFamily", "linux"), pair("ISA", "x86-64")],
            nonce: "77-1".to_owned(),
        }
    );
    let with = |f: &dyn Fn(&mut Args)| {
        let mut a = args(dir);
        f(&mut a);
        Settings::of(&a)
    };
    for bad in [
        "grpcs://farm:1",
        "farm:1",
        "http://farm:1",
        "grpc://",
        "grpc://farm:1/path",
        "",
    ] {
        assert!(
            matches!(
                with(&|a| a.address = bad.to_owned()),
                Err(Problem::Address(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        with(&|a| a.address = "grpc://farm:1\n[x]".to_owned()),
        Err(Problem::Unsafe { .. })
    ));
    for bad in ["", "yes", "True", "1"] {
        assert_eq!(
            with(&|a| a.tls = bad.to_owned()),
            Err(Problem::Tls(bad.to_owned()))
        );
    }
    assert!(!with(&|a| a.tls = "false".to_owned()).expect("valid").tls);
    assert!(matches!(
        with(&|a| a.instance = "a\nb".to_owned()),
        Err(Problem::Unsafe { .. })
    ));
    assert_eq!(with(&|a| a.nonce = " ".to_owned()), Err(Problem::NoNonce));
    assert!(matches!(
        with(&|a| a.nonce = "1 2".to_owned()),
        Err(Problem::Unsafe { .. })
    ));
    assert_eq!(
        with(&|a| a.properties = String::new()),
        Err(Problem::NoProperties)
    );
    assert_eq!(
        with(&|a| a.instance = String::new())
            .expect("valid")
            .instance,
        ""
    );
}

#[test]
fn the_buckconfig_turns_remote_on_and_local_off_with_the_farm_and_the_nonce() {
    // Catches: local execution left on, the properties or nonce missing from the file the
    // platform rule reads, a TLS flag that does not follow the variable, an empty instance
    // name written as a setting, and one of the three REAPI addresses forgotten.
    let settings = Settings::of(&args(Path::new("p"))).expect("valid");
    assert_eq!(
        render_buckconfig(&settings),
        "[buck2_re_client]\n  engine_address = grpc://farm.example:8980\n  action_cache_address = grpc://farm.example:8980\n  cas_address = grpc://farm.example:8980\n  tls = true\n  instance_name = main\n\n[build]\n  execution_platforms = root//platforms:remote\n\n[kbf_gamma]\n  remote_enabled = true\n  local_enabled = false\n  properties = OSFamily=linux,ISA=x86-64\n  nonce = 77-1\n"
    );
    let mut plain = args(Path::new("p"));
    plain.tls = "false".to_owned();
    plain.instance = String::new();
    let text = render_buckconfig(&Settings::of(&plain).expect("valid"));
    assert!(text.contains("  tls = false\n"), "{text}");
    assert!(!text.contains("instance_name"), "{text}");
}

#[test]
fn the_summary_line_is_read_from_buck2s_output() {
    // Catches: the counts read from the wrong field, a line in the middle of other output
    // missed, the first of several lines used (the last is the build's), and a missing
    // `cached` field refused.
    assert_eq!(
        parse_commands(REMOTE_ONLY),
        Ok(Counts {
            total: 4,
            cached: 0,
            remote: 4,
            local: 0
        })
    );
    assert_eq!(
        parse_commands("Cache hits: 25%. Commands: 8 (cached: 2, remote: 5, local: 1)  \n"),
        Ok(Counts {
            total: 8,
            cached: 2,
            remote: 5,
            local: 1
        })
    );
    assert_eq!(
        parse_commands("Commands: 1 (cached: 0, remote: 1, local: 0)\nBUILD\nCommands: 3 (cached: 1, remote: 2, local: 0)\n")
            .expect("parses")
            .total,
        3
    );
    assert_eq!(
        parse_commands("Commands: 2 (remote: 2, local: 0, other: 9)"),
        Ok(Counts {
            total: 2,
            cached: 0,
            remote: 2,
            local: 0
        })
    );
}

#[test]
fn a_summary_that_is_missing_or_unreadable_is_not_a_pass() {
    // Catches: a build that printed no counts (or a changed format) treated as zero local
    // actions, which would pass a proof that proved nothing.
    for bad in [
        "",
        "BUILD SUCCEEDED\n",
        "Commands:\n",
        "Commands: 3\n",
        "Commands: 3 (cached: 0)\n",
        "Commands: 3 (cached: 0, remote: 3)\n",
        "Commands: 3 (remote: 3, local: x)\n",
        "Commands: many (remote: 3, local: 0)\n",
        "Commands: 3 (remote 3, local 0)\n",
    ] {
        assert_eq!(parse_commands(bad), Err(Problem::NoSummary), "{bad:?}");
    }
}

#[test]
fn the_verdict_needs_a_remote_action_and_no_local_one() {
    // Catches: a farm that ran nothing (all cached) passing, and a build that ran any action
    // on the runner passing as a proof of the farm.
    let counts = |cached, remote, local| Counts {
        total: cached + remote + local,
        cached,
        remote,
        local,
    };
    assert_eq!(judge(counts(0, 4, 0)), Ok(()));
    assert_eq!(judge(counts(2, 1, 0)), Ok(()));
    assert_eq!(
        judge(counts(4, 0, 0)),
        Err(Problem::NothingRemote(counts(4, 0, 0)))
    );
    assert_eq!(
        judge(counts(0, 3, 1)),
        Err(Problem::RanLocally(counts(0, 3, 1)))
    );
    assert_eq!(
        judge(counts(0, 0, 1)),
        Err(Problem::RanLocally(counts(0, 0, 1)))
    );
    let text = Problem::RanLocally(counts(0, 3, 1)).to_string();
    assert!(text.contains("1 action(s) ran locally"), "{text}");
    assert!(text.contains("remote: 3, local: 1"), "{text}");
}

#[test]
fn a_remote_only_build_writes_the_config_runs_the_build_and_passes() {
    // Catches: the configuration not written before the build, the build not given the
    // project's target, Buck2's output hidden from the log, and a good build refused.
    let dir = scratch("pass");
    let seen = RefCell::new(Vec::new());
    let mut buck2 = |a: &[String]| {
        seen.borrow_mut().push(a.to_vec());
        let written = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).expect("config");
        assert!(written.contains("local_enabled = false"), "{written}");
        out(
            true,
            "out\n",
            "BUILD SUCCEEDED\nCommands: 4 (cached: 0, remote: 4, local: 0)\n",
        )
    };
    let mut log = Vec::new();
    let lines = run_with(&args(dir.path()), &mut buck2, &mut log).expect("proves");
    assert_eq!(seen.borrow().as_slice(), &[build_args()]);
    assert_eq!(build_args().last().map(String::as_str), Some("//:proof"));
    let log = String::from_utf8(log).expect("utf-8");
    assert!(log.contains("BUILD SUCCEEDED"), "{log}");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("ok: 4 remote action(s) on grpc://farm.example:8980"));
}

#[test]
fn a_build_that_ran_locally_or_nowhere_or_failed_fails_the_verb() {
    // Catches: local > 0 passing (the proof would be of the runner), no remote action passing,
    // a failed build whose last summary line looks fine, and no summary passing.
    let dir = scratch("fail");
    let run_it = |ok: bool, stdout: &str, stderr: &str| {
        let mut buck2 = |_: &[String]| out(ok, stdout, stderr);
        run_with(&args(dir.path()), &mut buck2, &mut Vec::new())
    };
    let local = "Commands: 4 (cached: 0, remote: 3, local: 1)\n";
    let err = run_it(true, "", local).expect_err("local");
    assert!(err.to_string().contains("1 action(s) ran locally"), "{err}");
    let err = run_it(true, local, "").expect_err("local on stdout");
    assert!(err.to_string().contains("ran locally"), "{err}");
    let cached = "Commands: 4 (cached: 4, remote: 0, local: 0)\n";
    let err = run_it(true, "", cached).expect_err("nothing remote");
    assert!(err.to_string().contains("no action ran remotely"), "{err}");
    let err = run_it(false, "", REMOTE_ONLY).expect_err("build failed");
    assert!(err.to_string().contains("buck2 build failed"), "{err}");
    let err = run_it(true, "BUILD SUCCEEDED\n", "").expect_err("no summary");
    assert!(err.to_string().contains("no `Commands:` line"), "{err}");
}

#[test]
fn bad_flags_stop_the_verb_before_buck2_is_started() {
    // Catches: Buck2 started with a configuration that was never checked, and a project
    // directory that does not exist reported as success.
    let dir = scratch("bad");
    let mut a = args(dir.path());
    a.tls = "maybe".to_owned();
    let mut never = |_: &[String]| -> Result<Out, Error> { panic!("buck2 started") };
    let err = run_with(&a, &mut never, &mut Vec::new()).expect_err("bad tls");
    assert!(err.to_string().contains("--tls"), "{err}");
    assert!(!dir.path().join(CONFIG_FILE).exists());
    let missing = args(&dir.path().join("no-such-project"));
    let err = run_with(&missing, &mut never, &mut Vec::new()).expect_err("no project");
    assert!(err.to_string().contains("write"), "{err}");
}

#[test]
fn an_unset_address_prints_a_notice_and_passes() {
    // Catches: a fork or a repository with no farm failing its release, or the notice lost
    // (the log would show a green job that proved nothing and said nothing).
    for address in ["", "  "] {
        let mut a = args(Path::new("p"));
        a.address = address.to_owned();
        let lines = run(&a).expect("passes");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("::notice::the gamma proof is off")
                && lines[0].contains("FARM_ADDRESS"),
            "{lines:?}"
        );
        let output = Command::new(XTASK)
            .args(["prove-farm", "--address", address])
            .args(["--buck2", "buck2", "--project", "p", "--nonce", "1"])
            .output()
            .expect("run");
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).starts_with("::notice::the gamma proof is off")
        );
    }
}

#[test]
fn the_binary_exits_1_for_a_bad_flag_and_for_a_build_without_counts() {
    // Catches: the verb's errors not reaching the exit status, and the real process runner
    // not reading a program's output (here `echo`, which prints no summary line).
    let dir = scratch("binary");
    let base = |tls: &str, buck2: &str| {
        Command::new(XTASK)
            .args(["prove-farm", "--address", "grpc://farm.example:8980"])
            .args(["--tls", tls, "--properties", "a=b", "--nonce", "1"])
            .args(["--buck2", buck2, "--project"])
            .arg(dir.path())
            .output()
            .expect("run")
    };
    let bad = base("maybe", "/bin/echo");
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--tls"));
    let none = base("false", "/bin/echo");
    assert_eq!(none.status.code(), Some(1), "{none:?}");
    assert!(String::from_utf8_lossy(&none.stdout).contains("build --console simple //:proof"));
    assert!(String::from_utf8_lossy(&none.stderr).contains("no `Commands:` line"));
    let missing = base("false", "/no/such/buck2");
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn the_process_runner_runs_the_program_in_the_project() {
    // Catches: the build run in the runner's working directory instead of the project (it
    // would not find the project's `.buckconfig`).
    let dir = scratch("process");
    let mut echo = process(Path::new("/bin/pwd"), dir.path());
    let done = echo(&[]).expect("runs");
    assert!(done.ok);
    let printed = std::fs::canonicalize(done.stdout.trim()).expect("a path");
    assert_eq!(printed, std::fs::canonicalize(dir.path()).expect("dir"));
    let mut missing = process(Path::new("/no/such/program"), dir.path());
    assert!(missing(&[]).is_err());
}
