use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::*;
use crate::testkit::{FakeExec, reply};

fn argv(args: &[&str]) -> Vec<OsString> {
    std::iter::once("kbf-deploy")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect()
}

const BUCKETS: &[&str] = &[
    "minio",
    "buckets",
    "--endpoint",
    "http://127.0.0.1:9000",
    "--bucket",
    "kbf-cas",
    "--policy",
    "kbf-linux-rw",
    "--account",
    "kbf-linux-server",
    "--mc",
    "/opt/kbf/bin/mc",
];

const NODEPORT: &[&str] = &[
    "minio",
    "nodeport",
    "--kubectl",
    "/usr/local/bin/kubectl",
    "--kubeconfig",
    "/srv/admin/kubeconfig",
    "--namespace",
    "minio",
    "--service",
    "minio-hdd",
    "--node-port",
    "30900",
    "--remove",
];

fn admin_env(key: &str) -> Option<OsString> {
    Some(OsString::from(if key == "MINIO_ROOT_USER" {
        "kbf-admin"
    } else {
        "pw"
    }))
}

/// Exit status, stdout and stderr of `run` over `exec`.
fn cli(
    exec: &mut FakeExec,
    env: &dyn Fn(&str) -> Option<OsString>,
    args: &[&str],
) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv(args), exec, env, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// Catches a parser that takes a bad flag value (a name that is not kbf's, a path that
/// is not absolute, a node port outside the NodePort range, an owner without a key
/// file) instead of exiting 2 before any query, and help that goes to stderr.
#[test]
fn bad_flags_exit_2_before_anything_runs() {
    let mut exec = FakeExec::new(501);
    let (code, out, _) = cli(&mut exec, &admin_env, &["--help"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(out.contains("minio"), "{out}");
    let swap = |from: &str, to: &str, base: &[&str]| -> Vec<String> {
        base.iter()
            .map(|a| if *a == from { to } else { a }.to_owned())
            .collect()
    };
    let mut bad: Vec<Vec<String>> = vec![
        swap("kbf-cas", "cas", BUCKETS),
        swap("/opt/kbf/bin/mc", "mc", BUCKETS),
        swap("http://127.0.0.1:9000", "http://127.0.0.1:9000/x", BUCKETS),
        swap("30900", "8080", NODEPORT),
        swap("minio-hdd", "Minio", NODEPORT),
        vec!["frobnicate".to_owned()],
    ];
    bad.push(
        BUCKETS
            .iter()
            .map(|a| (*a).to_owned())
            .chain(["--key-owner".to_owned(), "kbf".to_owned()])
            .collect(),
    );
    for args in bad {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, out, err) = cli(&mut exec, &admin_env, &args);
        assert_eq!(code, ExitCode::from(2), "{args:?}");
        assert_eq!(out, "", "{args:?}");
        assert_ne!(err, "", "{args:?}");
    }
    assert!(
        exec.queries.borrow().is_empty(),
        "a refused command line ran a query"
    );
}

/// Catches an admin key pair taken from anywhere but the environment, or taken when it
/// is empty (mc would run with no credentials and every check would read "absent").
#[test]
fn the_admin_pair_must_be_set_and_not_empty() {
    let no_user = |key: &str| (key == "MINIO_ROOT_PASSWORD").then_some(OsString::from("pw"));
    let empty_password = |key: &str| {
        Some(OsString::from(if key == "MINIO_ROOT_USER" {
            "u"
        } else {
            ""
        }))
    };
    type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;
    let cases: [(Env<'_>, &str); 2] = [
        (&no_user, "MINIO_ROOT_USER"),
        (&empty_password, "MINIO_ROOT_PASSWORD"),
    ];
    for (env, key) in cases {
        let mut exec = FakeExec::new(501);
        let (code, _, err) = cli(&mut exec, env, BUCKETS);
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(
            err,
            format!(
                "kbf-deploy: {key} is not set in the environment (the admin key pair is \
                 never read from a flag or a file)\n"
            )
        );
    }
}

/// Catches `mc` given a config directory others can read (it would hold the admin
/// URL if mc ever saved an alias) or one left behind after the run, and an apply flag
/// that does not reach the plan.
#[test]
fn buckets_runs_mc_in_a_private_scratch_directory_removed_after() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut exec = FakeExec::new(501);
    let record = Rc::clone(&seen);
    exec.answer = Box::new(move |cmd| {
        if let Some((_, dir)) = cmd.env.iter().find(|(k, _)| k == "MC_CONFIG_DIR") {
            let mode = fs::metadata(dir).map(|m| m.permissions().mode() & 0o777);
            record.borrow_mut().push((dir.clone(), mode.ok()));
        }
        Ok(reply(0, "mc version RELEASE.2025-08-13T08-35-41Z", ""))
    });
    let (code, out, err) = cli(&mut exec, &admin_env, BUCKETS);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert!(
        out.ends_with("dry run: nothing changed. Re-run with --apply.\n"),
        "{out}"
    );
    assert!(exec.log.is_empty());

    let apply: Vec<&str> = BUCKETS.iter().copied().chain(["--apply"]).collect();
    let (code, out, _) = cli(&mut exec, &admin_env, &apply);
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        out.contains("+ run /opt/kbf/bin/mc admin policy create"),
        "{out}"
    );
    assert_eq!(exec.log.len(), 1);

    let seen = seen.borrow();
    assert!(seen.len() >= 4, "{seen:?}");
    for (dir, mode) in seen.iter() {
        assert_eq!(*mode, Some(0o700), "{dir}");
        assert!(!std::path::Path::new(dir).exists(), "{dir} was left behind");
    }
}

/// Catches nodeport running without its scratch `--cache-dir` (kubectl would write a
/// cache under the working directory, since its environment has no HOME), and a
/// failed apply guard read as success: a nodeport plan never applies as root.
#[test]
fn nodeport_runs_kubectl_with_a_scratch_cache_and_never_as_root() {
    let mut exec = FakeExec::new(501);
    exec.answer = Box::new(|_| Ok(reply(0, r#"{"items":[]}"#, "")));
    let (code, out, _) = cli(&mut exec, &admin_env, NODEPORT);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(out, "no Service minio/minio-hdd-kbf-np\nnothing to do\n");
    let cache = exec.queries.borrow()[0].args[3].clone();
    assert!(
        cache.starts_with(&std::env::temp_dir().to_string_lossy().into_owned()),
        "{cache}"
    );

    let mut root = FakeExec::new(0);
    root.answer = Box::new(|_| Ok(reply(0, r#"{"items":[]}"#, "")));
    let apply: Vec<&str> = NODEPORT.iter().copied().chain(["--apply"]).collect();
    let (code, _, err) = cli(&mut root, &admin_env, &apply);
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(
        err,
        "kbf-deploy: this step runs as a user, not as root: run it without sudo\n"
    );
}

/// Catches `main` not reaching the parser and the host: a refused run must exit 1. The
/// endpoint is a closed port, so the run stops at the environment or at the health
/// check, whichever the test process has.
#[test]
fn main_runs_on_the_host() {
    let args: Vec<&str> = BUCKETS
        .iter()
        .map(|a| {
            if *a == "http://127.0.0.1:9000" {
                "http://127.0.0.1:1"
            } else {
                a
            }
        })
        .collect();
    assert_eq!(main(argv(&args)), ExitCode::FAILURE);
}
