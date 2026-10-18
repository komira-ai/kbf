use clap::{CommandFactory, Parser};
use serde_json::Value;

use super::*;
use crate::testkit::B;

fn parse(args: &[&str]) -> Cli {
    let mut full = vec!["kbf-installer"];
    full.extend_from_slice(args);
    Cli::try_parse_from(full).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

fn run_as(euid: u32, args: &[&str]) -> (Exit, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = dispatch(&parse(args), euid, &mut out, &mut err);
    (
        exit,
        String::from_utf8(out).expect("text"),
        String::from_utf8(err).expect("text"),
    )
}

/// Every verb of the contract's table and the pull request that builds it. The verbs that
/// are not built say so with the phase; the three that are built (`run`, `boot-check`, `setup`) are not here.
fn stubs() -> Vec<(Vec<&'static str>, &'static str, &'static str)> {
    let commit: &'static str = B;
    vec![
        (vec!["status"], "status", "P4"),
        (
            vec!["update", "--commit", commit, "--frontend"],
            "update",
            "P13",
        ),
        (vec!["confirm", "--id=3-42"], "confirm", "P5"),
        (vec!["revert", "--id=3-42"], "revert", "P5"),
        (vec!["rollback"], "rollback", "P5"),
        (vec!["gc"], "gc", "P5"),
        (vec!["quiesce"], "quiesce", "P5"),
        (vec!["check"], "check", "P10"),
        (vec!["stage"], "stage", "P8"),
        (vec!["activate"], "activate", "P5"),
        (vec!["fetch-child"], "fetch-child", "P7"),
    ]
}

/// Catches a verb of the contract that is missing from the command line, or one that runs
/// and claims to be done: each unbuilt verb exits 1 and names the pull request that builds
/// it, as root.
#[test]
fn every_unbuilt_verb_says_which_pull_request_builds_it() {
    for (args, name, phase) in stubs() {
        let (exit, out, err) = run_as(0, &args);
        assert_eq!(exit, Exit::Failed, "{args:?}");
        assert!(out.is_empty(), "{args:?}: {out}");
        assert_eq!(
            err,
            format!("kbf-installer: {name}: not implemented ({phase})\n")
        );
    }
}

/// Catches `setup` answering like a stub, or running without the flags its mode needs: the
/// verb is built, so it never says "not implemented"; a bare `--mode=daemon` (no frontend URL,
/// no token) is a usage error, exit 2, whoever runs it, and nothing is changed (no `--apply`).
#[test]
fn setup_is_built_and_refuses_flags_that_do_not_make_a_host() {
    let (exit, out, err) = run_as(0, &["setup", "--mode=daemon"]);
    assert_eq!(exit, Exit::Usage, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(!err.contains("not implemented"), "{err}");
    let (exit, _, err) = run_as(1000, &["setup", "--mode=daemon"]);
    assert_eq!(exit, Exit::Usage);
    assert!(err.contains("setup needs root"), "{err}");
    let (_, out, _) = run_as(0, &["--json", "setup", "--mode=daemon"]);
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(v["verb"], "setup");
    assert!(
        ["invalid_flags", "needs_root"].contains(&v["error"].as_str().expect("a word")),
        "{v}"
    );
}

/// Catches a verb that skips the root check, or one that needs root and does not need to:
/// everything but `status` and `check` is refused as an ordinary user with
/// exit 2, and those two get as far as their own (missing) work.
#[test]
fn only_status_and_check_run_without_root() {
    for (args, name, _) in stubs() {
        let (exit, _, err) = run_as(1000, &args);
        if ["status", "check"].contains(&name) {
            assert_eq!(exit, Exit::Failed, "{name}: {err}");
        } else {
            assert_eq!(exit, Exit::Usage, "{name}");
            assert!(
                err.contains("needs root") && err.contains(name),
                "{name}: {err}"
            );
        }
    }
}

/// Catches `--json` printing prose, or putting the object on the error stream where a
/// script reading standard output would miss it.
#[test]
fn json_is_one_object_on_standard_output() {
    let (exit, out, err) = run_as(0, &["--json", "--apply", "gc"]);
    assert_eq!((exit, err.as_str()), (Exit::Failed, ""));
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(
        v,
        serde_json::json!({"verb": "gc", "apply": true, "error": "not_implemented", "phase": "P5"})
    );
    let (_, out, _) = run_as(1000, &["--json", "gc"]);
    let v: Value = serde_json::from_str(out.trim_end()).expect("json");
    assert_eq!(
        (v["error"].as_str(), v["euid"].as_u64()),
        (Some("needs_root"), Some(1000))
    );
}

/// Catches a flag or verb that drifted from the contract: the exit codes, one by one.
#[test]
fn the_exit_codes_are_the_contracts() {
    let codes: Vec<u8> = Exit::ALL.iter().map(|e| e.code()).collect();
    assert_eq!(codes, [0, 1, 2, 3, 4, 5]);
}

/// Catches a positional argument (CEO rule: every argument other than the verb is a named
/// flag), anywhere in the command tree, hidden verbs included; and clap's own consistency
/// checks of the definition.
#[test]
fn no_command_takes_a_positional_argument() {
    fn walk(cmd: &clap::Command, path: &str, found: &mut Vec<String>) {
        for arg in cmd.get_arguments().filter(|a| a.is_positional()) {
            found.push(format!("{path}: {}", arg.get_id()));
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &format!("{path} {}", sub.get_name()), found);
        }
    }
    let cmd = Cli::command();
    cmd.clone().debug_assert();
    let mut found = Vec::new();
    walk(&cmd, "kbf-installer", &mut found);
    assert!(found.is_empty(), "{found:?}");
}

/// Catches the public verb list drifting from the contract's table (section 2): the visible
/// verbs, and that the three internal ones stay out of the help.
#[test]
fn the_visible_verbs_are_the_contracts() {
    let cmd = Cli::command();
    let visible: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| s.get_name().to_owned())
        .collect();
    assert_eq!(
        visible,
        [
            "setup",
            "status",
            "run",
            "update",
            "confirm",
            "revert",
            "rollback",
            "gc",
            "boot-check",
            "quiesce",
            "check"
        ]
    );
    let hidden: Vec<_> = cmd
        .get_subcommands()
        .filter(|s| s.is_hide_set())
        .map(|s| s.get_name().to_owned())
        .collect();
    assert_eq!(hidden, ["stage", "activate", "fetch-child"]);
}

/// Catches required flags becoming optional: a frontend update without `--frontend` (node
/// updates never come from the CLI), a confirm without the activation it names, a bad id,
/// a short commit, an unknown mode or a mirror flag value.
#[test]
fn flags_are_required_and_checked_by_the_parser() {
    let bad: [&[&str]; 9] = [
        &["update", "--commit", B],
        &["update", "--frontend"],
        &["update", "--commit", "abc", "--frontend"],
        &["confirm"],
        &["confirm", "--id", "42"],
        &["setup"],
        &["setup", "--mode=node"],
        &["setup", "--mode=daemon", "--release-source=ftp"],
        &["confirm", "3-42"],
    ];
    for args in bad {
        let mut full = vec!["kbf-installer"];
        full.extend_from_slice(args);
        assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
    }
}

/// `--apply` and `--json` are global: they may come after the verb.
#[test]
fn global_flags_may_follow_the_verb() {
    let cli = parse(&["setup", "--mode=all-in-one", "--apply", "--json"]);
    assert!(cli.apply && cli.json);
}
