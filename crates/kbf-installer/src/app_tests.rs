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
/// are not built say so with the phase; the ones that are built (`run`, `setup` and the operator's) are not here.
fn stubs() -> Vec<(Vec<&'static str>, &'static str, &'static str)> {
    vec![
        (vec!["status"], "status", "P4"),
        (vec!["check"], "check", "P10"),
        (vec!["stage"], "stage", "P8"),
        (vec!["activate"], "activate", "P5"),
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

/// The fetch child is built (P7) and runs as the fetch user, not as root: it is not gated on
/// root, it prints nothing on standard output but the component's bytes (never JSON, whatever
/// the global flag), and it says why it fetched nothing in one line on standard error.
///
/// Catches the child inheriting the "needs root" gate (it would never start as `kbf-fetch`) and
/// a child that prints prose where the parent counts bytes.
#[test]
fn the_fetch_child_is_no_root_verb_and_reports_in_one_line() {
    let digest = "a".repeat(64);
    for json in [false, true] {
        let mut args = vec!["fetch-child"];
        if json {
            args.insert(0, "--json");
        }
        args.extend([
            "--kind",
            "url",
            "--url",
            "http://h.example",
            "--sha256",
            &digest,
            "--limit",
            "10",
        ]);
        let (exit, out, err) = run_as(1000, &args);
        assert_eq!(exit, Exit::Failed, "{err}");
        assert!(out.is_empty(), "{out}");
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(err.starts_with("fetch-child: "), "{err}");
    }
}

/// The fetch child is asked for a blob (`--sha256`) or for what a release holds (`--describe`),
/// one of the two; `--platform` belongs to a description.
///
/// Catches a child started with both (it would not know which to do), with neither, or with a
/// platform and nothing to apply it to: all refused by the parser, before the child's own
/// check of its flags. (A platform with a blob passes the parser, which drops a requirement a
/// conflict rules out; the child refuses it itself.)
#[test]
fn the_fetch_child_takes_a_blob_or_a_release() {
    let digest = "a".repeat(64);
    let base = [
        "fetch-child",
        "--kind",
        "url",
        "--url",
        "https://h.example",
        "--limit",
        "10",
    ];
    let with = |extra: &[&str]| {
        let mut full = vec!["kbf-installer"];
        full.extend(base);
        full.extend_from_slice(extra);
        Cli::try_parse_from(full)
    };
    assert!(with(&["--sha256", &digest]).is_ok());
    assert!(with(&["--describe", A]).is_ok());
    assert!(with(&["--describe", A, "--platform", "linux-x86_64"]).is_ok());
    assert!(with(&[]).is_err(), "neither");
    assert!(
        with(&["--sha256", &digest, "--describe", A]).is_err(),
        "both"
    );
    assert!(
        with(&["--platform", "linux-x86_64"]).is_err(),
        "a platform alone"
    );
}

/// Catches `run` watching the Linux state directory on a Mac: without `--update-dir` and
/// `--orders-dir` the directories are the layout's, the ones `setup` and the Mac provisioner
/// write, for each platform; the flags still win.
#[test]
fn run_defaults_its_directories_to_the_layouts_on_every_platform() {
    for platform in [Platform::Linux, Platform::MacOs] {
        let (layout, _) = crate::testkit::host_root_on("run-dirs", platform);
        let Verb::Run(args) = parse(&["run"]).verb else {
            panic!("run");
        };
        assert_eq!(
            args.update_dir_in(&layout),
            layout.update_dir(),
            "{platform:?}"
        );
        assert_eq!(
            args.orders_dir_in(&layout),
            layout.orders_dir(),
            "{platform:?}"
        );
    }
    let Verb::Run(args) = parse(&["run", "--update-dir", "/srv/u", "--orders-dir", "/srv/o"]).verb
    else {
        panic!("run");
    };
    let (layout, _) = crate::testkit::host_root_on("run-dirs-flags", Platform::MacOs);
    assert_eq!(args.update_dir_in(&layout).as_str(), "/srv/u");
    assert_eq!(args.orders_dir_in(&layout).as_str(), "/srv/o");
}

/// Catches the parser putting the Linux directories back as `proxy` defaults: without the flags
/// they are unset (so the layout's apply), with them they are what was typed.
#[test]
fn proxy_leaves_its_directories_to_the_layout_unless_flagged() {
    let Verb::Proxy(args) = parse(&["proxy"]).verb else {
        panic!("proxy");
    };
    assert_eq!((args.update_dir, args.orders_dir), (None, None));
    let Verb::Proxy(args) =
        parse(&["proxy", "--update-dir", "/srv/u", "--orders-dir", "/srv/o"]).verb
    else {
        panic!("proxy");
    };
    assert_eq!(args.update_dir, Some("/srv/u".into()));
    assert_eq!(args.orders_dir, Some("/srv/o".into()));
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
        "database = \"postgres:///kbf?host=/nonexistent\"\n[proxy]\noperator_token_file = \"/a\"\nserver_cert_file = \"/b\"\nserver_key_file = \"/c\"\nclient_ca_file = \"/d\"\n[proxy.upstream]\nbase_port = 18000\n",
    )
    .expect("write");
    let (exit, out, err) = run_as(1000, &["proxy", "--check", "--config", &path]);
    assert_eq!((exit, out.as_str(), err.as_str()), (Exit::Ok, "", ""));
    // Not a mutating verb: it has no dry run to leave out.
    assert!(
        !Verb::Proxy(ProxyArgs {
            config: Some(file),
            check: true,
            update_dir: Some("/var/lib/kbf-installer".into()),
            orders_dir: Some("/var/lib/kbf-installer/orders".into()),
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
            "record-release",
            "gc",
            "boot-check",
            "migrate",
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
    fs::create_dir_all(layout.join(&layout.orders_dir())).expect("order spool");
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
    let status =
        fs::read(layout.join(&layout.update_dir()).join("status.json")).expect("status.json");
    let status: Value = serde_json::from_slice(&status).expect("json");
    assert_eq!(status["state"], "refused", "not root: the pass says so");
    let record = kbf_update::record::file_name(1, 2);
    fs::write(
        layout.join(&layout.orders_dir()).join(record),
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
    let release = layout.join(&layout.release_dir(&c(B)));
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

/// Catches `record-release` not running through the dispatcher, or its refusals leaving with
/// another exit: as root, files that are not a release (a release that was never placed, a
/// binary that is not the executable this host runs, a host root that is not absolute) end with
/// the exit of the refusal and the program on the line; a release error keeps the exit of
/// the release verbs.
///
/// Mutants planted: the verb dispatched to `gc`; the refusal folded into the failure exit.
#[test]
fn record_release_runs_through_the_dispatcher_and_refuses_what_is_not_a_release() {
    use crate::record_release::RecordError;
    use crate::release::ReleaseError;
    let (_, root) = seeded("record-release");
    let root_arg = root.to_str().expect("text");
    for commit in [A, B] {
        let (exit, _, err) = run_as(
            0,
            &[
                "--root",
                root_arg,
                "--apply",
                "record-release",
                "--commit",
                commit,
            ],
        );
        assert_eq!(exit, Exit::Refused, "{commit}: {err}");
        assert!(err.starts_with("kbf-installer: "), "{err}");
    }
    let (exit, _, err) = run_as(0, &["--root", "relative", "record-release", "--commit", A]);
    assert_eq!(exit, Exit::Failed, "{err}");

    let release = |e: ReleaseError| Failure::from(RecordError::Release(e)).exit;
    assert_eq!(release(ReleaseError::NotFirst(c(A))), Exit::Failed);
    let read = io::Error::from(io::ErrorKind::NotFound);
    let refused = Failure::from(RecordError::Read {
        path: "p".to_owned(),
        source: read,
    });
    assert_eq!(refused.exit, Exit::Refused);
    assert!(refused.why.contains("cannot read p"), "{}", refused.why);
}

/// Catches a frontend or all-in-one unit that activates releases with no database migration
/// and no warning: `run` for the unit of either mode refuses (exit 2, one line) unless
/// `--installer-config` is given, before it touches the host; a unit of a mode with no
/// database (or a launchd label, which names no mode) still runs without one; and a config
/// that cannot be read stops the service rather than starting it without the step.
///
/// Mutants seen red: the unit check removed (the all-in-one unit ran its pass); the unit
/// check matching only `kbf-frontend@` (the plain `kbf-frontend.service`, the unit setup enables on
/// a frontend host, ran its pass without the migration);
/// the all-in-one template (`kbf-all-in-one@1.service`) is in the test list.
#[test]
fn run_on_a_frontend_or_all_in_one_unit_needs_the_installer_config() {
    let (layout, root) = seeded("run-database");
    for unit in [
        "kbf-all-in-one.service",
        "kbf-all-in-one@1.service",
        "kbf-frontend@main.service",
        "kbf-frontend.service",
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
            !layout
                .join(&layout.update_dir())
                .join("status.json")
                .exists(),
            "{unit}: nothing was done before the refusal"
        );
    }
    for unit in ["kbf.service", "kbf-daemon.service"] {
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

/// Catches `run` building a helper that cannot tell about leases (the stub every build had
/// until a lease source was wired): the helper `run` builds reads the lease cgroups of the
/// unit it restarts, below the host root's `/sys/fs/cgroup`, so a populated lease of
/// `kbf-daemon.service` is one live lease. A unit that never ran (no cgroup), a unit of no
/// mode and a launchd label (the Mac source, which looks under the host root for
/// `sandbox-exec`) each cannot tell, with the reason: the activation is refused
/// (fail closed), and `run` itself still starts for them.
///
/// Mutants seen red: `leases::NotWired` left in `build_helper` (the daemon's lease reads as
/// "cannot tell"); the cgroup root taken as the real `/sys/fs/cgroup` instead of the host
/// root's.
#[test]
fn run_builds_a_helper_whose_lease_source_reads_the_restarted_units_cgroup() {
    let (_, root) = seeded("run-lease-source");
    let lease = root.join("sys/fs/cgroup/kbf.slice/kbf-daemon.service/actions/kbf-lease-1-1");
    fs::create_dir_all(&lease).expect("lease cgroup");
    fs::write(lease.join("cgroup.events"), "populated 1\n").expect("events");
    let source = |flags: &[&str]| {
        let mut argv = vec!["--root", root.to_str().expect("text"), "run"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        match build_helper(root.clone(), &args) {
            Ok(helper) => helper.leases().live(),
            Err(f) => panic!("{}", f.why),
        }
    };
    assert_eq!(source(&["--unit", "kbf-daemon.service"]), Ok(1));
    let err = source(&["--unit", "kbf-frontend@a.service"]).expect_err("never ran");
    assert!(err.contains("no such cgroup"), "{err}");
    let err = source(&["--unit", "kbf.service"]).expect_err("no mode's unit");
    assert!(err.contains("is no unit of any mode"), "{err}");
    let err = source(&["--launchd-label", "com.example.kbf"]).expect_err("a Mac source");
    assert!(err.contains("sandbox-exec"), "{err}");
    assert!(!err.contains("no lease source wired"), "{err}");
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

fn run_args(extra: &[&str]) -> RunArgs {
    let mut argv = RUN.to_vec();
    argv.extend_from_slice(extra);
    let Verb::Run(args) = parse(&argv).verb else {
        panic!("a run");
    };
    args
}

/// The file the fetcher reads is `--installer-config` when it is given and the file `setup`
/// writes when it is not.
///
/// Catches the flag ignored (the fetcher would read a file the operator did not name) and a
/// default that is not `setup`'s file.
///
/// Mutants planted: the flag's arm returning the default.
#[test]
fn the_fetcher_reads_the_flags_installer_toml_else_the_one_setup_writes() {
    let root = || kbf_host::Root::new("/").expect("root");
    let linux = Layout::new(root(), Platform::Linux);
    let mac = Layout::new(root(), Platform::MacOs);
    assert_eq!(
        installer_toml_of(&run_args(&[]), &linux).as_str(),
        "/etc/kbf/installer.toml"
    );
    assert_eq!(
        installer_toml_of(&run_args(&[]), &mac).as_str(),
        "/usr/local/etc/kbf/installer.toml"
    );
    let flagged = run_args(&["--installer-config", "/srv/kbf/installer.toml"]);
    assert_eq!(
        installer_toml_of(&flagged, &linux).as_str(),
        "/srv/kbf/installer.toml"
    );
}

/// `run` builds its helper with the host's fetcher, not the build's `NotWired`: a fetch on a
/// scratch host with no `installer.toml` (and no fetch user, on a box that has none) is
/// `Unavailable` with a reason, where the unwired fetcher says it has no fetcher at all.
///
/// Catches `build_helper` going back to `fetcher::NotWired` (every fetch refused with no reason
/// to act on).
///
/// Mutants planted: `Box::new(fetcher::NotWired)` in place of the host fetcher.
#[test]
fn run_builds_its_helper_with_the_host_fetcher_not_the_unwired_one() {
    let (_, root) = seeded("run-fetcher");
    let helper = build_helper(root, &run_args(&[])).unwrap_or_else(|f| panic!("{}", f.why));
    let rec = crate::helper::fetcher::ReleaseLocator {
        commit: c(B),
        components: Vec::new(),
    };
    let got = helper.fetcher().fetch(&rec);
    assert!(
        matches!(&got, Err(crate::helper::fetcher::FetchError::Unavailable(why)) if !why.is_empty()),
        "{got:?}"
    );
}

/// Catches `update` being gated on root (it is a client of the update API and runs as
/// whoever can read the token file) or not reaching its own code: a bad address is its own
/// usage error, as an ordinary user, and prints no summary.
#[test]
fn update_runs_without_root_and_reports_its_own_usage_errors() {
    let args = ["update", "--commit", B, "--frontend", "--api", "nowhere"];
    let (exit, out, err) = run_as(1000, &args);
    assert_eq!(exit, Exit::Usage);
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("update: --api") && err.contains("host:port"),
        "{err}"
    );
    assert!(!parse(&args).verb.needs_root());
    assert_eq!(parse(&args).verb.name(), "update");
}

/// Catches `update` taking a caller-chosen API address and token file as root: through sudo
/// (`SUDO_UID` of another account) it is refused as a usage error before the token file is
/// read or any address is dialed, and says why. Fails without the refusal: the run would go to
/// the address and report the unreadable token file instead.
#[test]
fn update_is_refused_through_sudo_before_it_reads_anything() {
    let Verb::Update(args) = parse(&[
        "update",
        "--commit",
        B,
        "--frontend",
        "--api",
        "127.0.0.1:1",
        "--token-file",
        "/nonexistent/token",
    ])
    .verb
    else {
        panic!("an update");
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(update_verb(&args, true, &mut out, &mut err), Exit::Usage);
    let err = String::from_utf8(err).expect("text");
    assert!(out.is_empty());
    assert!(err.contains("not accepted through sudo"), "{err}");
    assert!(!err.contains("/nonexistent"), "{err}");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    update_verb(&args, false, &mut out, &mut err);
    let err = String::from_utf8(err).expect("text");
    assert!(!err.contains("through sudo"), "{err}");
}

/// Catches a sudo test that takes root's own `SUDO_UID` or an unset one for another account
/// (an operator running as root would be refused), or a caller's uid for none.
#[test]
fn only_a_sudo_uid_of_another_account_is_a_sudo_caller() {
    use std::ffi::OsStr;
    assert!(!is_sudo_uid(None));
    assert!(!is_sudo_uid(Some(OsStr::new("0"))));
    assert!(is_sudo_uid(Some(OsStr::new("1000"))));
}

/// The Linux layout on `root`: the one the tests of the unit `run` derives from the mode pin,
/// whatever OS runs them, because the derivation is Linux's (a Mac has no unit to derive).
fn linux_on(root: &Path) -> Layout {
    Layout::linux(kbf_host::path::Root::new(root).expect("absolute"))
}

/// Writes `text` as the `kbf.toml` of the host `layout` describes.
fn write_kbf_toml(layout: &Layout, text: &str) {
    let file = layout.join(&layout.kbf_toml());
    fs::create_dir_all(file.parent().expect("a directory")).expect("config dir");
    fs::write(file, text).expect("kbf.toml");
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
    let linux = linux_on(&root);
    let service = |flags: &[&str]| {
        let mut argv = vec!["run", "--passes", "1"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        target_on(&root, &args, Platform::Linux)
    };
    for (mode, unit) in [
        ("frontend", "kbf-frontend.service"),
        ("storage", "kbf-storage.service"),
        ("daemon", "kbf-daemon.service"),
        ("storage-and-daemon", "kbf-storage-and-daemon.service"),
        ("all-in-one", "kbf-all-in-one.service"),
    ] {
        write_kbf_toml(&linux, &format!("mode = \"{mode}\"\n"));
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
    write_kbf_toml(&linux, "mode = \"all-in-one\"\n");
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
/// Mutants planted: the platform check dropped; the unknown mode mapped to a default unit; a
/// missing, unparseable, unreadable or mode-less kbf.toml mapped to a default unit.
#[test]
fn run_with_no_service_and_no_mode_to_derive_one_is_a_usage_error() {
    let (layout, root) = seeded("run-no-unit");
    let linux = linux_on(&root);
    let Verb::Run(args) = parse(&["run", "--passes", "1"]).verb else {
        panic!("a run");
    };
    let refused = |root: &Path| {
        target_on(root, &args, Platform::Linux)
            .err()
            .expect("refused")
    };
    let missing = refused(&root);
    assert_eq!(missing.exit, Exit::Usage);
    assert!(missing.why.contains("--unit"), "{}", missing.why);
    assert!(missing.why.contains("is not there"), "{}", missing.why);
    assert!(missing.why.contains("run setup"), "{}", missing.why);
    let kbf_toml = linux.join(&linux.kbf_toml());
    // Every refusal names the file, says what to do and is a usage exit: no panic, no default.
    let named = |f: &Failure, reason: &str| {
        assert_eq!(f.exit, Exit::Usage, "{reason}: {}", f.why);
        assert!(f.why.contains("/etc/kbf/kbf.toml"), "{reason}: {}", f.why);
        assert!(f.why.contains("--unit"), "{reason}: {}", f.why);
        assert!(f.why.contains("run setup"), "{reason}: {}", f.why);
        assert!(f.why.contains(reason), "{reason}: {}", f.why);
    };
    named(&missing, "is not there");
    write_kbf_toml(&linux, "other = 1\n");
    named(&refused(&root), "has no mode");
    write_kbf_toml(&linux, "mode = 3\n");
    named(&refused(&root), "has no mode");
    write_kbf_toml(&linux, "mode = [unclosed\n");
    named(&refused(&root), "is not TOML");
    fs::write(&kbf_toml, [0xff, 0xfe]).expect("bytes");
    named(&refused(&root), "is not UTF-8");
    fs::write(&kbf_toml, vec![b'#'; (1 << 20) + 1]).expect("big");
    named(&refused(&root), "is over");
    fs::remove_file(&kbf_toml).expect("rm");
    fs::create_dir(&kbf_toml).expect("dir");
    named(&refused(&root), "cannot be read");
    fs::remove_dir(&kbf_toml).expect("rmdir");
    write_kbf_toml(&linux, "mode = \"node\"\n");
    named(&refused(&root), "no mode of kbf");
    write_kbf_toml(&linux, "mode = \"\"\n");
    named(&refused(&root), "no mode of kbf");
    write_kbf_toml(&linux, "mode = \"daemon\"\n");
    let mac = derived_service(&layout, Platform::MacOs)
        .err()
        .expect("a Mac");
    assert_eq!(mac.exit, Exit::Usage);
    assert!(mac.why.contains("--launchd-label"), "{}", mac.why);
    // The whole verb, on the platform this test runs on (the `kbf.toml` is that platform's).
    if Platform::current() == Platform::Linux {
        write_kbf_toml(&layout, "mode = \"daemon\"\n");
        let (exit, err) = on(&root, &["run", "--passes", "1"]);
        assert_eq!(
            exit,
            Exit::Ok,
            "a daemon host does its pass on its unit: {err}"
        );
        write_kbf_toml(&layout, "mode = \"all-in-one\"\n");
        let (exit, err) = on(&root, &["run", "--passes", "1"]);
        assert_eq!(
            exit,
            Exit::Failed,
            "the database mode reads installer.toml: {err}"
        );
        assert!(err.contains("installer.toml"), "{err}");
        assert!(!err.contains("needs --installer-config"), "{err}");
    } else {
        // A Mac derives no label from the mode, whatever its kbf.toml says: it needs the flag.
        for mode in ["daemon", "all-in-one"] {
            write_kbf_toml(&layout, &format!("mode = \"{mode}\"\n"));
            let (exit, err) = on(&root, &["run", "--passes", "1"]);
            assert_eq!(exit, Exit::Usage, "{mode}: {err}");
            assert!(err.contains("--launchd-label"), "{mode}: {err}");
        }
    }
}

/// Catches the derived database-mode unit reading no installer.toml (the schema step then
/// silently absent, the very thing `run` refuses to start without): on a frontend host the
/// target carries `/etc/kbf/installer.toml`, on a daemon host none, and a flag is kept.
///
/// Mutants planted: the default dropped; the default given to every mode.
#[test]
fn a_derived_database_unit_reads_the_default_installer_toml_and_others_none() {
    let (_, root) = seeded("run-derived-config");
    let linux = linux_on(&root);
    let config = |mode: &str, flags: &[&str]| {
        write_kbf_toml(&linux, &format!("mode = \"{mode}\"\n"));
        let mut argv = vec!["run"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        target_on(&root, &args, Platform::Linux)
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

/// Catches the old helper and the installer both managing one host: while the old layout's
/// state is the host's and no migration finished, every verb that manages the host (`setup`,
/// `run`, the operator's verbs) refuses with exit 5 and names `migrate`, and does nothing
/// else; the verbs that change no release (`quiesce`, `boot-check`) and `migrate` itself are
/// not held. A host with no old state is not held at all.
///
/// Mutants planted: the guard not called; the guard called for `quiesce` too.
#[test]
fn the_verbs_that_manage_a_host_wait_for_its_migration() {
    use std::os::unix::fs::PermissionsExt;
    let (layout, root) = seeded("unmigrated");
    // The installer's directory as a host has it (the guard judges it like every file).
    fs::set_permissions(
        layout.join(&layout.state_dir()),
        fs::Permissions::from_mode(0o755),
    )
    .expect("mode");
    // The old layout's state is where the platform keeps it, as the guard looks.
    let old = layout.join(&crate::migrate::Roots::new(layout.platform()).old_state);
    fs::create_dir_all(&old).expect("old state dir");
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).expect("mode");
    let state = old.join("state.json");
    fs::write(
        &state,
        r#"{"schema":1,"current":null,"previous":null,"staged":null,"pending_confirm":null,"order_floor":0}"#,
    )
    .expect("old state");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).expect("mode");

    let held: [&[&str]; 5] = [
        &RUN,
        &["setup", "--mode=daemon"],
        &["gc"],
        &["confirm", "--id=1-5", "--unit", "kbf.service"],
        &["rollback", "--unit", "kbf.service"],
    ];
    for args in held {
        let (exit, err) = on(&root, args);
        assert_eq!(exit, Exit::NeedsOperator, "{args:?}: {err}");
        assert!(
            err.contains("kbf-installer migrate --apply"),
            "{args:?}: {err}"
        );
    }
    for args in [&["quiesce"][..], &["boot-check"]] {
        let (exit, err) = on(&root, args);
        assert_ne!(exit, Exit::NeedsOperator, "{args:?}: {err}");
    }

    fs::remove_file(&state).expect("rm");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Ok, "{err}");
}

/// Catches the migration guard's table drifting: a verb that manages the host (`setup`
/// included, now that all of it is built) left out of it, or one that changes no release put
/// in it. `fetch-child`, the unprivileged process a verb starts, manages nothing and is never
/// held. (The compiler makes every new verb say which it is; this pins what it says.)
///
/// Mutants to plant: `Verb::FetchChild` or `Verb::Migrate(_)` moved to the `true` arm; `Verb::Setup(_)` to the `false` one.
#[test]
fn the_guard_holds_the_verbs_that_manage_a_host_and_only_those() {
    let digest = "a".repeat(64);
    let fetch_child = [
        "fetch-child",
        "--kind",
        "url",
        "--url",
        "http://h.example",
        "--sha256",
        digest.as_str(),
        "--limit",
        "10",
    ];
    let held: [&[&str]; 9] = [
        &["setup", "--mode=frontend"],
        &RUN,
        &["confirm", "--id=1-5", "--unit", "kbf.service"],
        &["revert", "--id=1-5", "--unit", "kbf.service"],
        &["rollback", "--unit", "kbf.service"],
        &["record-release", "--commit", A],
        &["gc"],
        &["stage"],
        &["activate"],
    ];
    let free: [&[&str]; 7] = [
        // `update` is an unprivileged client of the proxy's API (new-layout hosts only); the
        // root half it asks for, `run`, is held.
        &[
            "update",
            "--commit",
            B,
            "--frontend",
            "--api",
            "127.0.0.1:1",
        ],
        &["status"],
        &["check"],
        &["boot-check"],
        &["quiesce"],
        &["migrate"],
        &fetch_child,
    ];
    for args in held {
        assert!(parse(args).verb.manages_host(), "{args:?} is held");
    }
    for args in free {
        assert!(!parse(args).verb.manages_host(), "{args:?} is not held");
    }
}

/// Catches the owner a migration trusts being root's only by accident: on a host it is
/// uid 0 whoever runs the verb; under `--root` it is the caller, who owns the rehearsal.
#[test]
fn the_owner_is_root_on_a_host_and_the_caller_in_a_rehearsal() {
    assert_eq!(owner_uid(false, 1000), 0);
    assert_eq!(owner_uid(true, 1000), 1000);
}

/// Catches the probe user's lookup failing quietly: a host that cannot answer is exit 1
/// with the reason, an unknown user exit 2 naming the user, a known one its ids.
#[test]
fn the_probe_user_is_looked_up_with_the_exit_codes_of_the_contract() {
    use kbf_host::testkit::{FakeExec, reply};
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        let who = cmd.args.last().map(String::as_str);
        Ok(match (cmd.program.as_str().ends_with("getent"), who) {
            (true, Some("kbf-fetch")) => {
                reply(0, "kbf-fetch:x:991:991::/nonexistent:/sbin/nologin\n", "")
            }
            (true, Some("broken")) => reply(1, "", "getent: no such service"),
            (true, _) => reply(2, "", ""),
            (false, _) => reply(0, "991\n", ""),
        })
    });
    assert_eq!(
        probe_user(&exec, "kbf-fetch", false, Platform::Linux)
            .expect("found")
            .uid,
        991
    );
    let missing = probe_user(&exec, "nobody-here", false, Platform::Linux).expect_err("unknown");
    assert_eq!(missing.exit, Exit::Usage);
    assert!(missing.why.contains("nobody-here"), "{}", missing.why);
    let broken = probe_user(&exec, "broken", false, Platform::Linux).expect_err("broken");
    assert_eq!(broken.exit, Exit::Failed);
    assert!(broken.why.contains("getent"), "{}", broken.why);
}

/// Catches a version probe that runs as root: on a host `--fetch-user root`, or any user
/// whose uid or gid is 0, is a usage error (exit 2) naming the user, for the probe runs a
/// binary nobody has trusted yet and `run_as` is the whole of what it is run with. A
/// rehearsal (`--root`, a tree of the caller's own) runs as whoever the caller is.
///
/// Mutants seen red: only the uid checked; only the gid checked; the rehearsal not excepted.
#[test]
fn the_version_probe_never_runs_as_root_on_a_host() {
    use kbf_host::testkit::{FakeExec, reply};
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        let who = cmd.args.last().map(String::as_str);
        Ok(match (cmd.program.as_str().ends_with("getent"), who) {
            (true, Some("root")) => reply(0, "root:x:0:0:root:/root:/bin/bash\n", ""),
            (true, Some("uid-zero")) => {
                reply(0, "uid-zero:x:0:991::/nonexistent:/sbin/nologin\n", "")
            }
            (true, Some("gid-zero")) => {
                reply(0, "gid-zero:x:991:0::/nonexistent:/sbin/nologin\n", "")
            }
            (true, Some("kbf-fetch")) => {
                reply(0, "kbf-fetch:x:991:991::/nonexistent:/sbin/nologin\n", "")
            }
            (true, _) => reply(2, "", ""),
            // `id -G`: the groups, which hold the primary one.
            (false, Some("root" | "gid-zero")) => reply(0, "0\n", ""),
            (false, _) => reply(0, "991\n", ""),
        })
    });
    for name in ["root", "uid-zero", "gid-zero"] {
        let error = probe_user(&exec, name, false, Platform::Linux).expect_err(name);
        assert_eq!(error.exit, Exit::Usage, "{name}");
        assert!(
            error.why.contains(name) && error.why.contains("root"),
            "{name}: {}",
            error.why
        );
    }
    assert_eq!(
        probe_user(&exec, "kbf-fetch", false, Platform::Linux)
            .expect("found")
            .uid,
        991
    );
    assert_eq!(
        probe_user(&exec, "root", true, Platform::Linux)
            .expect("a rehearsal")
            .uid,
        0
    );
}

/// Catches `migrate` missing from the command line or from the dispatcher: its flags parse
/// into the verb (every unit flag takes a unit name, and a name that is a path or has no
/// suffix is a usage error of the parser), the verb runs through `dispatch` on a host with
/// no old layout, says so on standard output and exits 0, and `--json` makes that one object.
///
/// Mutants planted: the `Verb::Migrate` arm of the dispatcher removed (the verb said "not
/// implemented"); a unit name taken unchecked.
#[test]
fn migrate_parses_its_flags_and_runs_through_the_dispatcher() {
    let cli = parse(&[
        "migrate",
        "--unit",
        "kbf-daemon.service",
        "--unit",
        "kbf-other.service",
        "--helper-unit",
        "kbf-helper.service",
        "--installer-unit",
        "kbf-installer.service",
        "--min-generation",
        "5",
        "--release-source",
        "frontend",
        "--rollback",
    ]);
    let Verb::Migrate(args) = &cli.verb else {
        panic!("not the migrate verb");
    };
    assert_eq!(args.units.len(), 2);
    assert!(args.helper_unit.is_some() && args.installer_unit.is_some() && args.rollback);
    assert_eq!(args.min_generation, Some(5));
    for bad in ["../x.service", "kbf", "A.service"] {
        for flag in ["--unit", "--helper-unit", "--installer-unit"] {
            let tried = Cli::try_parse_from(["kbf-installer", "migrate", flag, bad]);
            assert!(tried.is_err(), "{flag} {bad}");
        }
    }

    // A host with nothing on it: not `host_root_on`, which makes the installer's directories.
    let root = kbf_host::testkit::scratch("app-migrate");
    let id = std::process::Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .expect("id");
    let me = String::from_utf8(id.stdout)
        .expect("text")
        .trim()
        .to_owned();
    let root = root.to_str().expect("text");
    for json in [false, true] {
        let mut args = vec!["--root", root, "migrate", "--fetch-user", &me];
        if json {
            args.push("--json");
        }
        let (exit, out, err) = run_as(1000, &args);
        assert_eq!(exit, Exit::Ok, "{err}");
        assert!(out.contains("nothing to migrate"), "{out}");
        assert_eq!(out.trim_start().starts_with('{'), json, "{out}");
    }
}

/// Catches the report of `migrate` going to the wrong stream or in the wrong shape: it is the
/// result of the verb, so it is on standard output, as the text or, with `--json`, as the one
/// object (`verb`, `apply`, `message`), whatever the lines it holds.
///
/// Mutants to plant: the report written to `err`; `apply` or `message` dropped from the object.
#[test]
fn the_migrate_report_is_on_standard_output_as_text_or_one_object() {
    let mut out = Vec::new();
    assert_eq!(
        finish_migrate(false, true, "adopted 2 releases\nunits repointed", &mut out),
        Exit::Ok
    );
    assert_eq!(
        String::from_utf8(out).expect("text"),
        "adopted 2 releases\nunits repointed\n"
    );
    let mut out = Vec::new();
    assert_eq!(
        finish_migrate(true, false, "adopted 2 releases\nunits repointed", &mut out),
        Exit::Ok
    );
    let v: Value = serde_json::from_slice(&out).expect("one object");
    assert_eq!(
        v,
        serde_json::json!({
            "verb": "migrate",
            "apply": false,
            "message": "adopted 2 releases\nunits repointed",
        })
    );
}

/// Catches a failed `migrate` (here: a probe user the host does not have) not being said the
/// way every other verb's failure is: exit 2 for the usage error, the reason after the
/// program's name on standard error, or with `--json` the object `error: failed` on standard
/// output; nothing of a report.
///
/// Mutants to plant: the failure of the `Migrate` arm printed as a report (exit 0).
#[test]
fn a_migrate_that_fails_says_so_like_the_other_verbs() {
    let root = kbf_host::testkit::scratch("app-migrate-fails");
    let root = root.to_str().expect("text");
    let user = "no-such-user-for-kbf-migrate";
    let (exit, out, err) = run_as(1000, &["--root", root, "migrate", "--fetch-user", user]);
    assert_eq!(exit, Exit::Usage, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.starts_with("kbf-installer: ") && err.contains(user),
        "{err}"
    );
    let (exit, out, err) = run_as(
        1000,
        &["--json", "--root", root, "migrate", "--fetch-user", user],
    );
    assert_eq!((exit, err.as_str()), (Exit::Usage, ""));
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(
        (v["verb"].as_str(), v["error"].as_str()),
        (Some("migrate"), Some("failed"))
    );
    assert!(
        v["message"].as_str().is_some_and(|m| m.contains(user)),
        "{v}"
    );
}

/// Catches a host path joined to a root that is not absolute: `under` refuses it with the
/// reason. The dispatcher's migration guard looks at the root first, so it is this test, not
/// a verb's, that reaches the refusal.
#[test]
fn a_path_is_not_joined_to_a_relative_root() {
    let path = HostPath::new("/etc/kbf/installer.toml").expect("a host path");
    let failure = under(Path::new("relative/root"), &path).expect_err("refused");
    assert_eq!(failure.exit, Exit::Failed);
    assert!(!failure.why.is_empty());
    assert_eq!(
        under(Path::new("/srv/root"), &path).expect("joined"),
        PathBuf::from("/srv/root/etc/kbf/installer.toml")
    );
}
