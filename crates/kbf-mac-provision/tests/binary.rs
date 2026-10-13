//! The `kbf-mac-provision` binary: its verbs, flags and exit statuses (0 every item
//! passes, 1 one fails, 2 bad usage or a refused profile), and that it reads the Mac
//! under `--root`. No test here reads this machine's own settings.

mod support;

use std::process::{Command, Output};

use support::FakeMac;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-mac-provision");

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}

/// Catches: a binary that fails to start, or reports another name or version.
#[test]
fn prints_name_and_version() {
    let o = run(&["--version"]);
    assert!(o.status.success());
    assert_eq!(
        stdout(&o),
        format!("kbf-mac-provision {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Catches: bad usage accepted, or refused with another status than 2 (the script's
/// `t_usage`): no verb, an unknown verb, no profile, an unknown flag, a relative root.
#[test]
fn bad_usage_exits_2() {
    for args in [
        &[][..],
        &["frobnicate"],
        &["check"],
        &["check", "--profile"],
        &["check", "--profile", "x", "--bogus", "y"],
        &["print", "--profile", "x", "--root", "/"],
        &["apply", "--profile", "x"],
    ] {
        assert_eq!(run(args).status.code(), Some(2), "{args:?}");
    }
    let o = run(&["check", "--profile", "x", "--root", "relative"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("--root must be an absolute path"),
        "{}",
        stderr(&o)
    );
}

/// Catches: a refused profile exiting with another status than 2, or without saying
/// why on stderr; or anything printed on stdout for it.
#[test]
fn a_refused_profile_exits_2_and_says_why() {
    let mac = FakeMac::fresh("bin-refused");
    let path = mac.profile_file(&format!("{}sleep=0\n", mac.profile_text()));
    for verb in ["print", "check"] {
        let o = run(&[verb, "--profile", path.to_str().unwrap()]);
        assert_eq!(o.status.code(), Some(2), "{verb}");
        assert_eq!(
            stderr(&o),
            "kbf-mac-provision: profile line 42: unknown key: sleep\n"
        );
        assert!(o.stdout.is_empty());
    }
    let o = run(&["print", "--profile", "/nonexistent/profile"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("no readable profile file: /nonexistent/profile"));
}

/// Catches: `--keys` with an unknown item not exiting 2 (the script's `t_keys`).
#[test]
fn an_unknown_key_in_keys_exits_2() {
    let mac = FakeMac::fresh("bin-keys");
    let path = mac.profile_file(&mac.profile_text());
    let o = run(&[
        "check",
        "--profile",
        path.to_str().unwrap(),
        "--keys",
        "bogus",
    ]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unknown item: bogus"), "{}", stderr(&o));
}

/// Catches: `print` exiting non-zero or reading the Mac (the script's
/// `t_print_real_root`: print reads only the profile).
#[test]
fn print_reads_only_the_profile() {
    let mac = FakeMac::fresh("bin-print");
    let text = mac.profile_text();
    let path = mac.profile_file(&text);
    let o = run(&["print", "--profile", path.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let sha = support::sha256_hex(text.as_bytes());
    assert!(
        stdout(&o).ends_with(&format!(
            "profile_sha256={sha}\nnode_label=profile=mac-arm64-test@{}\n",
            &sha[..12]
        )),
        "{}",
        stdout(&o)
    );
}

/// Catches: `check` exiting 0 on a failing item (the script's mutant "check exits 0 on
/// drift"), or a check that reads this machine instead of the Mac under `--root`. An
/// empty root has none of the programs, so every item that reads one fails.
#[test]
fn check_on_an_empty_root_fails_and_exits_1() {
    let mac = FakeMac::fresh("bin-check");
    let path = mac.profile_file(&mac.profile_text());
    let empty = support::scratch("bin-check-empty-root");
    let o = run(&[
        "check",
        "--profile",
        path.to_str().unwrap(),
        "--root",
        &format!("{}/", empty.display()),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("FAIL macos_version: macOS unknown, the profile pins 27.0.1"),
        "{out}"
    );
    assert!(
        out.contains("FAIL firewall: firewall unknown, want on"),
        "{out}"
    );
    assert!(out.ends_with(" fail\n"), "{out}");
    assert_eq!(out.lines().count(), 31, "{out}");
}

/// Catches: `check` exiting 1 when every item it checks passes.
#[test]
fn check_exits_0_when_every_item_passes() {
    let mac = FakeMac::fresh("bin-pass");
    let path = mac.profile_file(&mac.profile_text());
    let empty = support::scratch("bin-pass-empty-root");
    // Nothing under an empty root is a simulator runtime.
    let o = run(&[
        "check",
        "--profile",
        path.to_str().unwrap(),
        "--root",
        empty.to_str().unwrap(),
        "--keys",
        "simulator_runtimes",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "PASS simulator_runtimes: none\ncheck: 1 pass, 0 fail\n"
    );
}

/// Catches: a changing verb run by a user who is not root, and not refused with exit
/// status 2 and why. This test runs as an ordinary user.
#[test]
fn the_changing_verbs_exit_2_without_root() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let mac = FakeMac::fresh("bin-not-root");
    let path = mac.profile_file(&mac.profile_text());
    let path = path.to_str().unwrap();
    for (args, verb) in [
        (
            &["apply", "--profile", path, "--root", &mac.root][..],
            "apply",
        ),
        (
            &["restart", "--profile", path, "--root", &mac.root],
            "restart",
        ),
        (&["revert", "--root", &mac.root], "revert"),
    ] {
        let o = run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert_eq!(
            stderr(&o),
            format!("kbf-mac-provision: {verb} runs as root\n")
        );
        assert!(o.stdout.is_empty());
    }
    assert_eq!(mac.writes(), Vec::<String>::new());
}

/// Catches: a verb that `execute` sends to another one, an exit status other than the
/// script's (apply 1 on a failure, 0 when it converges; restart and revert 0), or a
/// verb that changes another Mac than `--root`'s.
#[test]
fn execute_runs_each_verb_with_its_exit_status() {
    use clap::Parser as _;
    use kbf_mac_provision::cli::{Cli, execute};
    let mut mac = FakeMac::fresh("bin-execute");
    let path = mac.profile_file(&mac.profile_text());
    let path = path.to_str().unwrap();
    let parse = |args: &[&str]| {
        let cli = Cli::try_parse_from([&["kbf-mac-provision"][..], args].concat()).unwrap();
        assert_eq!(cli.verb.root(), if args[0] == "print" { "/" } else { "/r" });
        cli.verb
    };
    let verb = parse(&["check", "--profile", path, "--root", "/r"]);
    assert_eq!(execute(&verb, &mac).unwrap().1, 1);
    let verb = parse(&["apply", "--profile", path, "--root", "/r"]);
    assert_eq!(execute(&verb, &mac).unwrap().1, 0);
    assert_eq!(execute(&verb, &mac).unwrap(), (String::new(), 0));
    mac.state_mut().filevault = "On".into();
    assert_eq!(execute(&verb, &mac).unwrap().1, 1);
    let verb = parse(&["restart", "--profile", path, "--root", "/r"]);
    assert_eq!(execute(&verb, &mac).unwrap().1, 0);
    let verb = parse(&["revert", "--root", "/r"]);
    assert_eq!(execute(&verb, &mac).unwrap().1, 0);
    let verb = parse(&["print", "--profile", path]);
    assert_eq!(execute(&verb, &mac).unwrap().1, 0);
}
