use clap::{CommandFactory, Parser};
use serde_json::Value;

use super::*;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::ProxyArgs;
use crate::fixtures::{FakeRunner, ran};
use crate::layout::{Layout, Platform};
use crate::run::Ran;
use crate::setup::raw_from;
use crate::state::{Locked, PendingConfirm, State};
use crate::testkit::{A, B, c, host_root_on};

pub(super) fn parse(args: &[&str]) -> Cli {
    let mut full = vec!["kbf-installer"];
    full.extend_from_slice(args);
    Cli::try_parse_from(full).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

pub(super) fn run_as(euid: u32, args: &[&str]) -> (Exit, String, String) {
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

/// Asserts that `proxy` still accepts the retired `--update-dir`, `--orders-dir` and
/// `--kbf-binary` (a unit written before two-slot supervision was removed passes them): without
/// the flags they are unset, with them they are what was typed.
#[test]
fn proxy_accepts_its_retired_flags_and_leaves_them_unset_otherwise() {
    let Verb::Proxy(args) = parse(&["proxy"]).verb else {
        panic!("proxy");
    };
    assert_eq!(
        (args.update_dir, args.orders_dir, args.kbf_binary),
        (None, None, None)
    );
    let argv = [
        "proxy",
        "--update-dir",
        "/srv/u",
        "--orders-dir",
        "/srv/o",
        "--kbf-binary",
        "/usr/bin/kbf",
    ];
    let Verb::Proxy(args) = parse(&argv).verb else {
        panic!("proxy");
    };
    assert_eq!(args.update_dir, Some("/srv/u".into()));
    assert_eq!(args.orders_dir, Some("/srv/o".into()));
    assert_eq!(args.kbf_binary, Some("/usr/bin/kbf".into()));
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
        "[proxy]\noperator_token_file = \"/a\"\nserver_cert_file = \"/b\"\nserver_key_file = \"/c\"\nclient_ca_file = \"/d\"\n[[proxy.frontend]]\nname = \"a\"\nbase_port = 18000\n",
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
            kbf_binary: None,
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

/// Asserts that each of the proxy's flags is refused by `setup` as a usage error (exit 2) that
/// names the flag and says why (two-slot supervision was removed), before anything else is
/// checked: the refusal comes to an ordinary user and to root alike, with nothing on standard
/// output, and not as the root check's "run it with sudo".
#[test]
fn setup_refuses_each_proxy_flag_with_the_reason_before_it_checks_anything() {
    let flags = [
        "--proxy",
        "--proxy-base-port=18000",
        "--buck2-auth=none",
        "--buck2-token-file=/etc/kbf/bearer",
        "--storage-port=8990",
    ];
    for flag in flags {
        let name = flag.split('=').next().unwrap_or_default();
        for euid in [0, 1000] {
            let (exit, out, err) = run_as(euid, &["setup", "--mode=frontend", flag]);
            assert_eq!(exit, Exit::Usage, "{flag} as {euid}");
            assert!(out.is_empty(), "{flag} as {euid}: {out}");
            let start = format!("kbf-installer: setup: {name}: two-slot supervision was removed");
            assert!(err.starts_with(&start), "{flag} as {euid}: {err}");
            assert!(err.contains("[[proxy.frontend]]"), "{err}");
            assert!(!err.contains("sudo"), "{flag} as {euid}: {err}");
        }
    }
}

/// Asserts that the refusal of the proxy's flags is one JSON object on standard output with
/// `--json`, naming `setup`, the word `invalid_flags` and the reason, and nothing on standard
/// error.
#[test]
fn the_refusal_of_a_proxy_flag_is_one_json_object_with_json() {
    let args = ["--json", "setup", "--mode=frontend", "--proxy"];
    let (exit, out, err) = run_as(1000, &args);
    assert_eq!((exit, err.as_str()), (Exit::Usage, ""));
    let v: Value = serde_json::from_str(out.trim_end()).expect("one json object");
    assert_eq!(
        (v["verb"].as_str(), v["error"].as_str()),
        (Some("setup"), Some("invalid_flags"))
    );
    let message = v["message"].as_str().unwrap_or_default();
    assert!(message.starts_with("--proxy: two-slot supervision was removed"));
}

/// Asserts that the parser of `setup` still takes the proxy's flags, with their values (they are
/// hidden from the help, which `tests/help_golden.rs` pins, and taken only to be refused), and
/// that a command line without them gives no refusal.
#[test]
fn the_parser_of_setup_still_takes_the_proxy_flags() {
    let cli = parse(&["setup", "--mode=frontend", "--proxy"]);
    let Verb::Setup(all) = &cli.verb else {
        panic!("not the setup verb");
    };
    assert!(all.proxy.proxy);
    assert!(all.proxy.refusal().is_some());
    let port = parse(&["setup", "--mode=frontend", "--storage-port=8990"]);
    let Verb::Setup(all) = &port.verb else {
        panic!("not the setup verb");
    };
    assert_eq!(all.proxy.storage_port, Some(8990));
    let plain = parse(&["setup", "--mode=frontend"]);
    let Verb::Setup(all) = &plain.verb else {
        panic!("not the setup verb");
    };
    assert!(all.proxy.refusal().is_none());
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

/// Catches a positional argument (rule: every argument other than the verb is a named
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
            "confirm",
            "revert",
            "rollback",
            "record-release",
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

/// Asserts that required flags stay required: a confirm without the activation it names, a
/// bad id, an unknown mode or a flag value that is not a number.
#[test]
fn flags_are_required_and_checked_by_the_parser() {
    let bad: [&[&str]; 6] = [
        &["confirm"],
        &["confirm", "--id", "42"],
        &["setup"],
        &["setup", "--mode=node"],
        &["setup", "--mode=daemon", "--min-generation=x"],
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
pub(super) fn seeded(name: &str) -> (Layout, PathBuf) {
    let (layout, root) = host_root_on(&format!("app-{name}"), Platform::current());
    crate::testkit::mkdirs(&root, &layout.join(&layout.orders_dir()), 0o755);
    let release = layout.join(&layout.release_dir(&c(A)));
    crate::testkit::mkdirs(&root, &release, 0o755);
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

pub(super) fn on(root: &Path, args: &[&str]) -> (Exit, String) {
    let mut full = vec!["--root", root.to_str().expect("text")];
    full.extend_from_slice(args);
    let (exit, _, err) = run_as(1000, &full);
    (exit, err)
}

pub(super) const RUN: [&str; 5] = ["run", "--passes", "1", "--unit", "kbf.service"];

#[path = "app_gc_tests.rs"]
mod gc_keep;
#[path = "app_run_config_tests.rs"]
mod run_config;
#[path = "app_run_tests.rs"]
mod run_loop;
