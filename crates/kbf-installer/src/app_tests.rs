use clap::{CommandFactory, Parser};
use serde_json::Value;

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

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
/// are not built say so with the phase; the ones that are built (`run`, `setup` and the operator's) are not here.
fn stubs() -> Vec<(Vec<&'static str>, &'static str, &'static str)> {
    let commit: &'static str = B;
    vec![
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
    let (_, out, _) = run_as(1000, &["--json", "activate"]);
    let v: Value = serde_json::from_str(out.trim_end()).expect("json");
    assert_eq!(
        (v["error"].as_str(), v["euid"].as_u64()),
        (Some("needs_root"), Some(1000))
    );
}

/// Catches `setup` skipping the root check or reporting in the wrong shape: as an ordinary
/// user it stops before any step with exit 2, as text on standard error or, with `--json`, as
/// one object on standard output naming the word `needs_root`.
#[test]
fn setup_is_root_only_and_says_so_as_text_or_json() {
    let args = [
        "setup",
        "--mode=daemon",
        "--frontend-url=https://f.example",
        "--token-file=/t",
    ];
    let (exit, out, err) = run_as(1000, &args);
    assert_eq!(exit, Exit::Usage);
    assert!(out.is_empty(), "{out}");
    assert!(
        err.starts_with(
            "kbf-installer: setup: setup writes /etc and manages services: run it with sudo"
        ),
        "{err}"
    );
    let mut json = vec!["--json"];
    json.extend_from_slice(&args);
    let (exit, out, err) = run_as(1000, &json);
    assert_eq!((exit, err.as_str()), (Exit::Usage, ""));
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(
        (v["verb"].as_str(), v["error"].as_str()),
        (Some("setup"), Some("needs_root"))
    );
}

/// Catches the steps of `setup` going to the wrong stream: they are for a person, so on
/// standard output, or with `--json` (where standard output is the one object) on standard
/// error; a failure prints them ahead of its own line, as text, and as the object with the
/// word and the message in JSON.
#[test]
fn setup_steps_go_to_the_person_and_the_object_to_the_script() {
    let steps = b"+ step one\n";
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        finish_setup(false, Ok(()), steps, &mut out, &mut err),
        Exit::Ok
    );
    assert_eq!((out.as_slice(), err.as_slice()), (&steps[..], &b""[..]));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        finish_setup(true, Ok(()), steps, &mut out, &mut err),
        Exit::Ok
    );
    assert_eq!((out.as_slice(), err.as_slice()), (&b""[..], &steps[..]));

    let fail = || Err(setup::SetupError::Database("down".to_owned()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        finish_setup(false, fail(), steps, &mut out, &mut err),
        Exit::Failed
    );
    assert_eq!(
        String::from_utf8(err).expect("text"),
        "+ step one\nkbf-installer: setup: database step: down\n"
    );
    assert!(out.is_empty());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        finish_setup(true, fail(), steps, &mut out, &mut err),
        Exit::Failed
    );
    assert!(err.is_empty());
    let v: Value = serde_json::from_slice(&out).expect("one object");
    assert_eq!(
        v,
        serde_json::json!({"verb": "setup", "error": "database", "message": "database step: down"})
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

/// Catches `run` not waiting between passes, or not stopping after the last: two passes on a
/// host take one `PASS_INTERVAL` (the only wait, none after the last pass) and exit 0.
///
/// Mutants to plant: the wait dropped (the test finishes in well under the interval); the
/// wait taken after the last pass too (it takes two intervals).
#[test]
fn run_waits_one_interval_between_two_passes_and_not_after_the_last() {
    let (_, root) = seeded("run-two-passes");
    let began = std::time::Instant::now();
    let (exit, err) = on(&root, &["run", "--passes", "2", "--unit", "kbf.service"]);
    let took = began.elapsed();
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(took >= PASS_INTERVAL, "{took:?}");
    assert!(took < PASS_INTERVAL * 2, "{took:?}");
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
    // A start that fails leaves this process as it was: `Command::exec` would have put
    // SIGPIPE back to its default (the standard library does that before it execs), and a
    // later write to a pipe nobody reads would then end the whole process by a signal
    // instead of failing with an error. Nothing else in the lib tests reaches the re-exec.
    assert_eq!(sigpipe_handler(), libc::SIG_IGN, "SIGPIPE is ignored again");
}

/// The disposition of SIGPIPE in this process.
fn sigpipe_handler() -> libc::sighandler_t {
    // SAFETY: sigaction with a null new action only reads the current one into `old`, which
    // a zeroed `sigaction` is valid for.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        assert_eq!(
            libc::sigaction(libc::SIGPIPE, std::ptr::null(), &raw mut old),
            0
        );
        old.sa_sigaction
    }
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

/// Catches a frontend or all-in-one unit that activates releases with no database migration
/// and no warning: `run` for the unit of either mode refuses (exit 2, one line) unless
/// `--installer-config` is given, before it touches the host; a unit of a mode with no
/// database (or a launchd label, which names no mode) still runs without one; and a config
/// that cannot be read stops the service rather than starting it without the step.
///
/// Mutants seen red: the unit check removed (the all-in-one unit ran its pass); the unit
/// check matching `kbf-frontend` without the `@` (the plain `kbf-frontend.service` refused);
/// the all-in-one template (`kbf-all-in-one@1.service`) is in the test list.
#[test]
fn run_on_a_frontend_or_all_in_one_unit_needs_the_installer_config() {
    let (_, root) = seeded("run-database");
    for unit in [
        "kbf-all-in-one.service",
        "kbf-all-in-one@1.service",
        "kbf-frontend@main.service",
    ] {
        let (exit, err) = on(&root, &["run", "--passes", "1", "--unit", unit]);
        assert_eq!(exit, Exit::Usage, "{unit}: {err}");
        assert_eq!(
            err,
            "kbf-installer: run needs --installer-config on a frontend or all-in-one host \
             (the database is migrated before a release is activated)\n",
            "{unit}"
        );
        assert!(
            !root.join("var/lib/kbf-installer/status.json").exists(),
            "{unit}: nothing was done before the refusal"
        );
    }
    for unit in ["kbf.service", "kbf-daemon.service", "kbf-frontend.service"] {
        let (exit, err) = on(&root, &["run", "--passes", "1", "--unit", unit]);
        assert_eq!(exit, Exit::Ok, "{unit}: {err}");
    }
    let (exit, err) = on(
        &root,
        &["run", "--passes", "1", "--launchd-label", "com.example.kbf"],
    );
    assert_eq!(exit, Exit::Ok, "a label names no mode: {err}");
    let (exit, err) = on(
        &root,
        &[
            "run",
            "--passes",
            "1",
            "--unit",
            "kbf-all-in-one.service",
            "--installer-config",
            "/etc/kbf/missing.toml",
        ],
    );
    assert_eq!(exit, Exit::Failed, "{err}");
    assert!(err.contains("missing.toml"), "{err}");
}

/// Catches a relative `--installer-config` (on `run` or `setup`) being quietly re-rooted
/// under `/` and read from somewhere the operator did not name: both flags take a host path
/// like `--update-dir` does, so a relative one is a usage error (clap, exit 2), not a read.
#[test]
fn the_installer_config_flag_must_be_an_absolute_host_path() {
    for args in [
        &[
            "run",
            "--unit",
            "kbf.service",
            "--installer-config",
            "etc/kbf/installer.toml",
        ][..],
        &[
            "setup",
            "--mode=frontend",
            "--installer-config",
            "./installer.toml",
        ][..],
    ] {
        let mut full = vec!["kbf-installer"];
        full.extend_from_slice(args);
        assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
    }
}

/// Catches `run --installer-config` not attaching the schema step to the helper it drives
/// (the helper tests attach a fake directly, and the Postgres tests never go through `run`):
/// the helper `run` builds from a config naming a database refuses a release's schema with
/// the database's own error (the socket directory does not exist, so no server is needed),
/// and one built without the flag has no step.
///
/// Mutants seen red: the `set_schema_step` call removed from `build_helper`.
#[test]
fn run_with_an_installer_config_drives_a_helper_that_migrates_first() {
    let (_, root) = seeded("run-schema-step");
    fs::create_dir_all(root.join("etc/kbf")).expect("etc");
    fs::write(
        root.join("etc/kbf/installer.toml"),
        "database = \"postgres:///kbf?host=/nonexistent-kbf-test-socket\"\n",
    )
    .expect("config");
    let with = |extra: &[&str]| {
        let mut argv = vec!["--root", root.to_str().expect("text")];
        argv.extend_from_slice(&RUN);
        argv.extend_from_slice(extra);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        match build_helper(root.clone(), &args) {
            Ok(helper) => helper,
            Err(f) => panic!("{}", f.why),
        }
    };
    let mut helper = with(&["--installer-config", "/etc/kbf/installer.toml"]);
    let step = helper.schema_step_mut().expect("the step is attached");
    let why = step.before_start(1).expect_err("no server to migrate");
    assert!(!why.is_empty());
    assert!(with(&[]).schema_step_mut().is_none(), "no flag, no step");
}

/// Catches `run --installer-config` reading the config under a host root that is not an
/// absolute path (the root is the sandbox flag's; a relative one is refused with its reason
/// rather than reading from the working directory).
///
/// Mutants to plant: the `Root::new` error in `under` swallowed.
#[test]
fn run_with_an_installer_config_refuses_a_relative_host_root() {
    let mut argv = RUN.to_vec();
    argv.extend_from_slice(&["--installer-config", "/etc/kbf/installer.toml"]);
    let Verb::Run(args) = parse(&argv).verb else {
        panic!("a run");
    };
    let refused = build_helper(PathBuf::from("relative-root"), &args).err();
    assert!(refused.is_some_and(|f| !f.why.is_empty()));
}

/// Writes `mode = "<mode>"` as the host's `kbf.toml` under `root`.
fn write_kbf_toml(root: &Path, text: &str) {
    fs::create_dir_all(root.join("etc/kbf")).expect("etc");
    fs::write(root.join("etc/kbf/kbf.toml"), text).expect("kbf.toml");
}

/// Catches the packaged `kbf-installer.service` (one unit for every mode, no `--unit`) not
/// learning what to restart: with no flag naming the service, `run` restarts the unit
/// `setup` enabled for the `mode` of the host's kbf.toml, one per mode, and a flag still
/// wins over the file.
///
/// Mutants planted: `named_service` ignoring the flags; the first unit of the mode taken
/// from another mode's row.
#[test]
fn run_without_a_service_flag_restarts_the_unit_of_the_hosts_mode() {
    let (_, root) = seeded("run-derived-unit");
    let service = |flags: &[&str]| {
        let mut argv = vec!["run", "--passes", "1"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        target_of(&root, &args)
    };
    for (mode, unit) in [
        ("frontend", "kbf-frontend@a.service"),
        ("storage", "kbf-storage.service"),
        ("daemon", "kbf-daemon.service"),
        ("storage-and-daemon", "kbf-storage-and-daemon.service"),
        ("all-in-one", "kbf-all-in-one.service"),
    ] {
        write_kbf_toml(&root, &format!("mode = \"{mode}\"\n"));
        let derived = service(&[])
            .unwrap_or_else(|f| panic!("{mode}: {}", f.why))
            .service;
        assert_eq!(derived.unit.as_deref(), Some(unit), "{mode}");
        assert_eq!(derived.launchd_label, None, "{mode}");
    }
    let named = service(&["--unit", "kbf-daemon.service"])
        .expect("named")
        .service;
    assert_eq!(named.unit.as_deref(), Some("kbf-daemon.service"));
    write_kbf_toml(&root, "mode = \"all-in-one\"\n");
    let named = service(&["--unit", "kbf-daemon.service"])
        .expect("the flag wins")
        .service;
    assert_eq!(named.unit.as_deref(), Some("kbf-daemon.service"));
    let label = service(&["--launchd-label", "com.komira.kbf-daemon"])
        .expect("label")
        .service;
    assert_eq!(label.unit, None);
    assert_eq!(
        label.launchd_label.as_deref(),
        Some("com.komira.kbf-daemon")
    );
}

/// Catches a `run` that starts with no service it can restart: no kbf.toml, no `mode`, a
/// mode kbf does not offer (`node`), or a Mac without a label each end it with exit 2 and the
/// reason, before any pass.
///
/// Mutants planted: the platform check dropped; the unknown mode mapped to a default unit.
#[test]
fn run_with_no_service_and_no_mode_to_derive_one_is_a_usage_error() {
    let (layout, root) = seeded("run-no-unit");
    let Verb::Run(args) = parse(&["run", "--passes", "1"]).verb else {
        panic!("a run");
    };
    let refused = |root: &Path| target_of(root, &args).err().expect("refused");
    let missing = refused(&root);
    assert_eq!(missing.exit, Exit::Usage);
    assert!(missing.why.contains("--unit"), "{}", missing.why);
    assert!(missing.why.contains("is not there"), "{}", missing.why);
    write_kbf_toml(&root, "other = 1\n");
    assert!(refused(&root).why.contains("has no mode"));
    write_kbf_toml(&root, "mode = \"node\"\n");
    assert!(refused(&root).why.contains("no mode of kbf"));
    write_kbf_toml(&root, "mode = \"daemon\"\n");
    let mac = derived_service(&layout, Platform::MacOs)
        .err()
        .expect("a Mac");
    assert_eq!(mac.exit, Exit::Usage);
    assert!(mac.why.contains("--launchd-label"), "{}", mac.why);
    let (exit, err) = on(&root, &["run", "--passes", "1"]);
    assert_eq!(
        exit,
        Exit::Ok,
        "a daemon host does its pass on its unit: {err}"
    );
    write_kbf_toml(&root, "mode = \"all-in-one\"\n");
    let (exit, err) = on(&root, &["run", "--passes", "1"]);
    assert_eq!(
        exit,
        Exit::Failed,
        "the database mode reads installer.toml: {err}"
    );
    assert!(err.contains("installer.toml"), "{err}");
    assert!(!err.contains("needs --installer-config"), "{err}");
}

/// Catches the derived database-mode unit reading no installer.toml (the schema step then
/// silently absent, the very thing `run` refuses to start without): on a frontend host the
/// target carries `/etc/kbf/installer.toml`, on a daemon host none, and a flag is kept.
///
/// Mutants planted: the default dropped; the default given to every mode.
#[test]
fn a_derived_database_unit_reads_the_default_installer_toml_and_others_none() {
    let (_, root) = seeded("run-derived-config");
    let config = |mode: &str, flags: &[&str]| {
        write_kbf_toml(&root, &format!("mode = \"{mode}\"\n"));
        let mut argv = vec!["run"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        target_of(&root, &args)
            .expect("target")
            .installer_config
            .map(|p| p.to_string())
    };
    let default = Some("/etc/kbf/installer.toml".to_owned());
    assert_eq!(config("frontend", &[]), default);
    assert_eq!(config("all-in-one", &[]), default);
    assert_eq!(config("daemon", &[]), None);
    assert_eq!(config("storage", &[]), None);
    let other = Some("/etc/kbf/other.toml".to_owned());
    assert_eq!(
        config("all-in-one", &["--installer-config", "/etc/kbf/other.toml"]),
        other
    );
}
