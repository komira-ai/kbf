use clap::{CommandFactory, Parser};
use serde_json::Value;

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::ProxyArgs;
use crate::layout::{Layout, Platform};
use crate::state::{Locked, PendingConfirm, State};
use crate::testkit::{A, B, c, host_root_on};

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
/// are not built say so with the phase; the two that are built are not here.
fn stubs() -> Vec<(Vec<&'static str>, &'static str, &'static str)> {
    let commit: &'static str = B;
    vec![
        (vec!["setup", "--mode=daemon"], "setup", "P10"),
        (vec!["status"], "status", "P4"),
        (
            vec!["update", "--commit", commit, "--frontend"],
            "update",
            "P13",
        ),
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

/// Catches the proxy verb asking for root (it runs as the service user), a refused table
/// leaving as anything but a usage error with its path, and `--json` losing the reason.
#[test]
fn the_proxy_verb_runs_unprivileged_and_refuses_a_bad_table_as_usage() {
    let dir = crate::testkit::host_root("proxy-dispatch").1;
    let file = dir.join("installer.toml");
    fs::write(&file, "min_generation = 1\n").expect("write");
    let path = file.display().to_string();
    let (exit, out, err) = run_as(1000, &["proxy", "--check", "--config", &path]);
    assert_eq!(exit, Exit::Usage);
    assert!(out.is_empty());
    assert!(
        err.starts_with(&format!("kbf-installer: proxy: {path}: ")) && err.contains("no [proxy]"),
        "{err}"
    );
    let (exit, out, _) = run_as(1000, &["--json", "proxy", "--check", "--config", &path]);
    assert_eq!(exit, Exit::Usage);
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(
        (v["verb"].as_str(), v["error"].as_str()),
        (Some("proxy"), Some("usage"))
    );
    assert!(
        v["message"]
            .as_str()
            .is_some_and(|m| m.contains("no [proxy]"))
    );

    fs::write(
        &file,
        "[proxy]\noperator_token_file = \"/a\"\nserver_cert_file = \"/b\"\nserver_key_file = \"/c\"\nclient_ca_file = \"/d\"\n[proxy.upstream]\nbase_port = 18000\n",
    )
    .expect("write");
    let (exit, out, err) = run_as(1000, &["proxy", "--check", "--config", &path]);
    assert_eq!((exit, out.as_str(), err.as_str()), (Exit::Ok, "", ""));
    // Not a mutating verb: it has no dry run to leave out.
    assert!(
        !Verb::Proxy(ProxyArgs {
            config: file,
            check: true
        })
        .needs_root()
    );
}

/// Catches `--json` printing prose, or putting the object on the error stream where a
/// script reading standard output would miss it.
#[test]
fn json_is_one_object_on_standard_output() {
    let (exit, out, err) = run_as(0, &["--json", "--apply", "status"]);
    assert_eq!((exit, err.as_str()), (Exit::Failed, ""));
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(
        v,
        serde_json::json!({"verb": "status", "apply": true, "error": "not_implemented", "phase": "P4"})
    );
    let (_, out, _) = run_as(1000, &["--json", "setup", "--mode=daemon"]);
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
            "check",
            "proxy"
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
    assert_eq!((cli.apply, cli.json), (true, true));
}

/// A host root with release A current and its order spool in place, as the installer finds a host.
/// The installer under test lays a host out for the platform it was built for, so the
/// scratch host is laid out for that platform too (`/var/db/kbf-installer` on macOS).
fn seeded(name: &str) -> (Layout, PathBuf) {
    let (layout, root) = host_root_on(&format!("app-{name}"), Platform::current());
    fs::create_dir_all(root.join("var/lib/kbf-installer/orders")).expect("order spool");
    let release = layout.join(&layout.release_dir(&c(A)));
    fs::create_dir_all(&release).expect("release");
    fs::write(release.join("kbf"), b"x").expect("binary");
    Locked::acquire(&layout)
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    (layout, root)
}

fn on(root: &Path, args: &[&str]) -> (Exit, String) {
    let mut full = vec!["--root", root.to_str().expect("text")];
    full.extend_from_slice(args);
    let (exit, _, err) = run_as(1000, &full);
    (exit, err)
}

const RUN: [&str; 5] = ["run", "--passes", "1", "--unit", "kbf.service"];

/// Catches `run` not doing a pass in this build of the crate (the real binary does; this is
/// the same code run in the unit tests, so a change to it is seen by both): a pass on a host
/// reports and exits 0 even as an ordinary user, with an order record that is not one the
/// pass still ends, an unreadable state stops it before any pass (exit 1), and a memory file
/// it cannot read stops the loop with the file named.
///
/// Mutants planted: the pass count ignored (the test would hang); `recover` errors dropped
/// (the ruined state is then not an exit 1); the cycle error dropped (helper.json is then not
/// reported).
#[test]
fn run_does_its_passes_and_stops_on_a_state_it_cannot_read() {
    let (layout, root) = seeded("run");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Ok, "{err}");
    let status = fs::read(root.join("var/lib/kbf-installer/status.json")).expect("status.json");
    let status: Value = serde_json::from_slice(&status).expect("json");
    assert_eq!(status["state"], "refused", "not root: the pass says so");
    let record = kbf_update::record::file_name(1, 2);
    fs::write(
        root.join("var/lib/kbf-installer/orders").join(record),
        b"not an order",
    )
    .expect("record");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Ok, "{err}");
    fs::write(
        layout.join(&layout.state_dir()).join("helper.json"),
        b"not json",
    )
    .expect("ruin");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("helper.json"), "{err}");
    fs::remove_file(layout.join(&layout.state_dir()).join("helper.json")).expect("remove");
    fs::write(layout.join(&layout.state_file()), b"not json").expect("ruin");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(
        exit,
        Exit::NeedsOperator,
        "no usable state: only an operator goes on"
    );
    assert!(err.starts_with("kbf-installer:"), "{err}");
}

/// Catches a boot check that does not count a start or that guesses on a state it cannot
/// read: with a swap on trial the first start is counted (exit 0), the last start's revert
/// needs root and so refuses an ordinary user (exit 1), a ruined state refuses (exit 1), and
/// a relative root is refused for both verbs before anything is read.
///
/// Mutant planted: the decision printed but the error arm returning `Ok` (the refusals
/// above are then exit 0).
#[test]
fn boot_check_counts_a_start_and_refuses_what_it_cannot_read() {
    let (layout, root) = seeded("boot");
    let release = root.join("var/lib/kbf-installer/releases").join(B);
    fs::create_dir_all(&release).expect("release");
    fs::write(release.join("kbf"), b"x").expect("binary");
    let lock = Locked::acquire(&layout).expect("lock");
    lock.write(&State {
        current: Some(c(B)),
        previous: Some(c(A)),
        pending_confirm: Some(PendingConfirm {
            commit: c(B),
            previous: Some(c(A)),
            activation: "5".to_owned(),
            deadline_wall: None,
            deadline_mono_s: None,
            starts_left: Some(1),
        }),
        ..State::default()
    })
    .expect("state");
    drop(lock);
    let (exit, err) = on(&root, &["boot-check", "--window-secs", "600"]);
    assert_eq!(exit, Exit::Ok, "{err}");
    let (exit, err) = on(&root, &["boot-check"]);
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("as root"), "{err}");
    for file in [layout.state_file(), layout.state_backup()] {
        fs::write(layout.join(&file), b"not json").expect("ruin");
    }
    let (exit, err) = on(&root, &["boot-check"]);
    assert_eq!(exit, Exit::NeedsOperator, "{err}");
    for verb in [&RUN[..], &["boot-check"][..]] {
        let mut full = vec!["--root", "relative"];
        full.extend_from_slice(verb);
        let (exit, _, _) = run_as(1000, &full);
        assert_eq!(exit, Exit::Failed, "{verb:?}");
    }
}

/// Catches an exit that does not tell an operator's job from a retry: only an unusable state
/// (both files damaged) ends `needs an operator` (5); every other state error, a memory file
/// that cannot be read, and a plain message end `failed` (1). Both of the error types a verb
/// can end with are checked.
///
/// Mutant planted: the guard dropped (every state error asks for an operator).
#[test]
fn only_an_unusable_state_asks_for_an_operator() {
    use crate::helper::cycle::HelperError;
    use crate::release::ReleaseError;
    use crate::state::StateError;
    let unusable = || StateError::Unusable {
        primary: "p".to_owned(),
        backup: "b".to_owned(),
    };
    let other = || StateError::NoSchema {
        path: PathBuf::from("/x"),
    };
    let exit = |f: Failure| f.exit;
    assert_eq!(
        exit(HelperError::State(unusable()).into()),
        Exit::NeedsOperator
    );
    assert_eq!(exit(HelperError::State(other()).into()), Exit::Failed);
    assert_eq!(
        exit(HelperError::Memory("m".to_owned()).into()),
        Exit::Failed
    );
    assert_eq!(
        exit(ReleaseError::State(unusable()).into()),
        Exit::NeedsOperator
    );
    assert_eq!(exit(ReleaseError::State(other()).into()), Exit::Failed);
    assert_eq!(exit(ReleaseError::Pending(c(A)).into()), Exit::Failed);
    assert_eq!(exit("message".to_owned().into()), Exit::Failed);
}

/// Catches the loop of `run` going on with a file that was replaced under it: the file it
/// watches is another file (here one that is no program), so the turn says re-exec, the
/// loop tries to start it, and the failure is the reason it ends with (exit 1).
///
/// Mutants planted: the watch never consulted; the failed start taken for a success.
#[test]
fn a_loop_whose_file_was_replaced_tries_the_new_one_and_ends_when_it_cannot_start() {
    let (_, root) = seeded("replaced");
    let dir = root.join("copy");
    fs::create_dir_all(&dir).expect("dir");
    let me = dir.join("kbf-installer");
    fs::write(&me, b"the program").expect("the program");
    let watch = FileIdentity::of(&me).expect("identity");
    let other = dir.join("other");
    fs::write(&other, b"not a program").expect("a file that is no program");
    fs::rename(&other, &me).expect("replaced");
    let cli = parse(&["run", "--passes", "3", "--unit", "kbf.service"]);
    let Verb::Run(args) = &cli.verb else {
        panic!("a run");
    };
    let Err(failure) = run_from(root, args, &me, watch) else {
        panic!("the new file cannot start");
    };
    assert_eq!(failure.exit, Exit::Failed);
    assert!(failure.why.contains("cannot start"), "{}", failure.why);
}

/// Catches the operator's verbs not being wired to the dispatcher, or their exits being
/// remapped on the way: through the dispatcher on a host with nothing on trial, `confirm`,
/// `revert` and `rollback` need the operator (5), `quiesce` is at rest (0) and a dry-run `gc`
/// succeeds; each failure of the operator keeps its exit code through the conversion.
///
/// Mutants planted: a verb dispatched to another verb's function; the refused exit folded
/// into failed.
#[test]
fn the_operators_verbs_run_through_the_dispatcher_with_their_own_exits() {
    use crate::helper::operator::Failure as Operator;
    let (_, root) = seeded("operator-verbs");
    let with = |verb: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = verb.iter().map(ToString::to_string).collect();
        v.extend(["--unit".to_owned(), "kbf.service".to_owned()]);
        v
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        on(&root, &refs).0
    };
    assert_eq!(run(with(&["confirm", "--id=1-5"])), Exit::NeedsOperator);
    assert_eq!(run(with(&["revert", "--id=1-5"])), Exit::NeedsOperator);
    assert_eq!(run(with(&["rollback"])), Exit::NeedsOperator);
    assert_eq!(run(vec!["quiesce".to_owned()]), Exit::Ok);
    assert_eq!(run(vec!["gc".to_owned()]), Exit::Ok);
    let exit = |f: Operator| Failure::from(f).exit;
    assert_eq!(exit(Operator::Failed("x".to_owned())), Exit::Failed);
    assert_eq!(exit(Operator::Refused("x".to_owned())), Exit::Refused);
    assert_eq!(exit(Operator::Busy("x".to_owned())), Exit::Busy);
    assert_eq!(
        exit(Operator::NeedsOperator("x".to_owned())),
        Exit::NeedsOperator
    );
}
