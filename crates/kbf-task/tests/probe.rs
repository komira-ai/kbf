//! `kbf-task --probe-confinement` run as a process, the way the job scheduler runs it
//! (`src/probe.rs` lists the lines). Nothing here needs a privilege, a network or a server.
//!
//! Which tests run where:
//! - every test but one runs in the pull-request gate, on a host that may or may not let an
//!   unprivileged user namespace keep its capabilities: each asserts what must hold on either
//!   path (the lines, the verdict agreeing with the exit code, the netns path implying PASS);
//! - `the_probe_passes_where_the_namespace_layer_applies` is `#[ignore]`d, because a runner whose
//!   AppArmor restricts unprivileged user namespaces cannot pass it. Run it where the namespace
//!   is expected (a job container on the farm, or a host that allows the namespace):
//!   `cargo test -p kbf-task --test probe -- --ignored`.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;

const SENTINEL: &str = "sentinel-bytes-that-must-never-be-printed";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-probe-{name}"))
}

/// Runs the probe with `work` made first.
fn probe(work: &Path, extra: &[&str]) -> Output {
    std::fs::create_dir_all(work).expect("work base");
    probe_raw(work, extra)
}

fn probe_raw(work: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .arg("--probe-confinement")
        .arg("--probe-work-dir")
        .arg(work)
        .args(extra)
        .output()
        .expect("kbf-task starts")
}

/// The `probe <name>: <rest>` lines in order, as (name, rest); the last line aside.
fn checks(out: &Output) -> (Vec<(String, String)>, String) {
    let text = String::from_utf8(out.stdout.clone()).expect("utf-8");
    let mut lines: Vec<&str> = text.lines().collect();
    let last = lines.pop().expect("a last line").to_owned();
    let parsed = lines
        .iter()
        .map(|l| {
            let rest = l.strip_prefix("probe ").expect(l);
            let (name, rest) = rest.split_once(": ").expect(l);
            (name.to_owned(), rest.to_owned())
        })
        .collect();
    (parsed, last)
}

fn rest<'a>(checks: &'a [(String, String)], name: &str) -> &'a str {
    &checks.iter().find(|(n, _)| n == name).expect(name).1
}

fn status(rest: &str) -> &str {
    rest.split(' ').next().expect("a status")
}

const NAMES: [&str; 19] = [
    "layers",
    "child",
    "netns_distinct",
    "loopback",
    "inet_unreachable",
    "netlink",
    "packet",
    "io_uring",
    "secret_path",
    "apparmor_restrict_unprivileged_userns",
    "max_user_namespaces",
    "io_uring_disabled",
    "euid",
    "uid_map",
    "gid_map",
    "cap_eff",
    "child_uid_map",
    "child_gid_map",
    "child_cap_eff",
];

/// Catches: a line missing, renamed or reordered; a verdict that disagrees with the exit code;
/// the namespace path running without loopback working; the fallback being called a PASS; the
/// contents of the secret path reaching the output.
#[test]
fn the_probe_prints_the_documented_lines_and_its_exit_code_is_the_verdict() {
    let dir = scratch("lines");
    let secret = dir.join("secret");
    std::fs::write(&secret, SENTINEL).expect("secret");
    let out = probe(
        &dir.join("work-base"),
        &["--probe-secret-path", secret.to_str().expect("utf-8")],
    );
    let (checks, last) = checks(&out);
    let names: Vec<&str> = checks.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, NAMES);
    assert!(
        last == "probe result: PASS" || last == "probe result: FAIL",
        "{last}"
    );
    assert_eq!(
        out.status.code(),
        Some(i32::from(last.ends_with("FAIL"))),
        "{last}"
    );
    assert_eq!(rest(&checks, "secret_path"), "ok readable");
    assert!(!String::from_utf8_lossy(&out.stdout).contains(SENTINEL));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(SENTINEL));
    let layers = rest(&checks, "layers");
    if status(layers) == "ok" {
        assert_eq!(layers, "ok netns (loopback only) + seccomp");
        assert_eq!(rest(&checks, "child"), "ok exit 0");
        assert!(
            rest(&checks, "netns_distinct").starts_with("ok net:["),
            "{checks:?}"
        );
        assert!(rest(&checks, "loopback").starts_with("ok "), "{checks:?}");
        assert_eq!(
            rest(&checks, "inet_unreachable"),
            "ok ENETUNREACH (101) (netns)"
        );
        for name in ["netlink", "packet", "io_uring"] {
            assert!(rest(&checks, name).starts_with("ok "), "{name}: {checks:?}");
        }
        assert_eq!(last, "probe result: PASS");
        // The Command's own maps and capabilities, from inside the namespace.
        assert_ne!(status(rest(&checks, "child_uid_map")), "n/a");
    } else {
        assert!(
            layers.starts_with("FAIL seccomp only, no inet (netns: "),
            "{layers}"
        );
        assert!(rest(&checks, "loopback").starts_with("n/a "));
        assert!(rest(&checks, "netns_distinct").starts_with("n/a "));
        assert_eq!(last, "probe result: FAIL");
    }
}

/// Catches: a path that does not exist being reported as an answer, and a PASS with a question
/// left open.
#[test]
fn a_secret_path_that_is_not_there_fails_the_probe() {
    let dir = scratch("missing");
    let missing = dir.join("nothing-here");
    let out = probe(
        &dir.join("w"),
        &["--probe-secret-path", missing.to_str().expect("utf-8")],
    );
    let (checks, last) = checks(&out);
    assert!(
        rest(&checks, "secret_path").starts_with("FAIL ENOENT (2)"),
        "{checks:?}"
    );
    assert_eq!(last, "probe result: FAIL");
    assert_eq!(out.status.code(), Some(1));
}

/// Catches: a file the Command cannot read being reported readable (root reads any file, so the
/// case is skipped there).
#[test]
fn a_secret_path_the_command_cannot_read_is_reported_not_readable() {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = scratch("unreadable");
    let secret = dir.join("secret");
    std::fs::write(&secret, SENTINEL).expect("secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let out = probe(
        &dir.join("w"),
        &["--probe-secret-path", secret.to_str().expect("utf-8")],
    );
    let (checks, _) = checks(&out);
    assert_eq!(
        rest(&checks, "secret_path"),
        "ok not readable (EACCES (13))"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(SENTINEL));
}

/// Catches: no secret path asked being treated as an answer, and a leftover scratch directory.
#[test]
fn without_a_secret_path_the_check_is_not_applicable_and_the_scratch_is_removed() {
    let dir = scratch("none");
    let work = dir.join("w");
    std::fs::create_dir(&work).expect("work");
    let out = probe(&work, &[]);
    let (checks, _) = checks(&out);
    assert_eq!(rest(&checks, "secret_path"), "n/a no --probe-secret-path");
    assert_eq!(
        std::fs::read_dir(&work).expect("work").count(),
        0,
        "left over"
    );
}

/// Catches: an unusable work directory reading as a pass.
#[test]
fn a_work_directory_that_cannot_be_made_fails_the_probe() {
    let dir = scratch("nowork");
    let out = probe_raw(&dir.join("not/there"), &[]);
    let (checks, last) = checks(&out);
    assert!(
        rest(&checks, "child").starts_with("FAIL cannot make "),
        "{checks:?}"
    );
    assert_eq!(last, "probe result: FAIL");
    assert_eq!(out.status.code(), Some(1));
}

/// Catches: the probe flag accepted next to the flags of a real task, which would run neither.
#[test]
fn a_flag_of_the_task_beside_the_probe_flag_is_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .args(["--probe-confinement", "--grant-url", "http://127.0.0.1:1"])
        .output()
        .expect("kbf-task starts");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

/// Catches: the Command side printing anything but `observe` lines, or leaving a call out.
/// It runs here without the confinement, so only the shape is asserted, not the values.
#[test]
fn the_command_side_prints_one_observation_per_call() {
    let dir = scratch("child");
    let secret = dir.join("secret");
    std::fs::write(&secret, SENTINEL).expect("secret");
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .args(["--probe-confinement-child", "--probe-secret-path"])
        .arg(&secret)
        .output()
        .expect("kbf-task starts");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(text.lines().all(|l| l.starts_with("observe ")), "{text}");
    for name in [
        "loopback", "inet", "netlink", "packet", "io_uring", "secret",
    ] {
        assert!(
            text.lines()
                .any(|l| l.starts_with(&format!("observe {name} "))),
            "{name}: {text}"
        );
    }
    assert!(text.contains("observe secret readable"));
    assert!(!text.contains(SENTINEL));
    // Without a path there is no `secret` line.
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .arg("--probe-confinement-child")
        .output()
        .expect("kbf-task starts");
    let bare = String::from_utf8(out.stdout).expect("utf-8");
    assert!(bare.contains("observe io_uring "), "{bare}");
    assert!(!bare.contains("observe secret"), "{bare}");
    // A directory is listed, and a path that is not there names its errno.
    for (path, want) in [
        (dir.path().to_owned(), "readable"),
        (dir.join("nothing"), "errno 2"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
            .args(["--probe-confinement-child", "--probe-secret-path"])
            .arg(&path)
            .output()
            .expect("kbf-task starts");
        let text = String::from_utf8(out.stdout).expect("utf-8");
        assert!(
            text.contains(&format!("observe secret {want}\n")),
            "{path:?}: {text}"
        );
    }
}

/// Catches: the default scratch directory (the system temporary directory) not being used or
/// not being cleaned up, and a probe that needs the work-dir flag.
#[test]
fn without_a_work_directory_the_probe_uses_the_temporary_directory() {
    let dir = scratch("tmpdir");
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .arg("--probe-confinement")
        .env("TMPDIR", dir.path())
        .output()
        .expect("kbf-task starts");
    let (checks, last) = checks(&out);
    assert_eq!(checks.len(), NAMES.len(), "{checks:?}");
    assert!(last.starts_with("probe result: "));
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("tmp").count(),
        0,
        "left over"
    );
}

/// The pass, where the namespace is expected.
///
/// `cargo test -p kbf-task --test probe -- --ignored`
#[test]
#[ignore = "needs a host that allows unprivileged user namespaces; run with --ignored"]
fn the_probe_passes_where_the_namespace_layer_applies() {
    let dir = scratch("pass");
    let out = probe(&dir.join("w"), &[]);
    let (checks, last) = checks(&out);
    assert_eq!(last, "probe result: PASS", "{checks:?}");
    assert_eq!(out.status.code(), Some(0));
}
