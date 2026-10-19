//! `kbf-installer` as the unit runs it, on a scratch root. Nothing here restarts a service or
//! runs a fetched binary: the passes see an empty order spool, an order record the binary
//! refuses (it has no release fetcher yet), and a release on trial.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_installer::layout::{Layout, Platform};
use kbf_installer::state::{Locked, PendingConfirm, State};
use kbf_installer::testkit::{A, B, c};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

fn root(name: &str) -> PathBuf {
    // The platform the binary was built for, as the binary lays the host out.
    let (_, dir) =
        kbf_installer::testkit::host_root_on(&format!("helper-bin-{name}"), Platform::current());
    fs::create_dir_all(dir.join("var/lib/kbf-installer/orders")).expect("order spool");
    dir
}

fn layout(root: &Path) -> Layout {
    Layout::new(
        kbf_host::Root::new(root).expect("absolute"),
        Platform::current(),
    )
}

/// A file in the state directory of this platform (`var/lib/kbf-installer` on Linux,
/// `var/db/kbf-installer` on macOS), under `root`.
fn in_state_dir(root: &Path, name: &str) -> PathBuf {
    let layout = layout(root);
    layout.join(&layout.state_dir()).join(name)
}

fn helper(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("runs")
}

fn status(root: &Path) -> serde_json::Value {
    let bytes = fs::read(root.join("var/lib/kbf-installer/status.json")).expect("status.json");
    serde_json::from_slice(&bytes).expect("json")
}

/// Catches a helper that cannot do one pass of its own: it writes a world-readable
/// status.json and exits 0 (here as a non-root user, so with the installer's refusal); with an order it cannot
/// carry out (the binary has no fetcher yet), the pass ends and the refusal is the node's to
/// read, not a crash.
#[test]
fn one_pass_reports_the_host_and_a_refused_order() {
    let root = root("pass");
    let host = layout(&root);
    Locked::acquire(&host)
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    let release = host.join(&host.release_dir(&c(A)));
    fs::create_dir_all(&release).expect("release");
    fs::write(release.join("kbf"), b"x").expect("binary");

    let run = ["run", "--passes", "1", "--unit", "kbf.service"];
    let out = helper(&root, &run);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Not root: even a pass that moves no link refuses (the installer's rule), and the
    // node is told why in status.json rather than the helper dying silently.
    assert_eq!(status(&root)["state"], "refused");
    assert!(
        status(&root)["refusal"]
            .as_str()
            .expect("text")
            .contains("as root")
    );
    assert_eq!(status(&root)["installed_commit"], A);
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(root.join("var/lib/kbf-installer/status.json"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644, "the node reads it as the service user");

    // An activation: the binary has no lease source and no fetcher yet, so it is refused
    // with that reason (fail closed), and the pass ends.
    let record = kbf_update::OrderRecord {
        order_id: 2,
        epoch: 1,
        generation: 1,
        verb: kbf_update::Verb::Activate,
        commit: B.to_owned(),
        window_s: 600,
        components: vec![kbf_update::Locator {
            name: "kbf".to_owned(),
            sha256: "ab".repeat(32),
            size: 1000,
            attestation: "{}".to_owned(),
        }],
        expected: Some(kbf_update::Facts {
            // The platform of the host the binary was built for.
            platform: if cfg!(target_os = "macos") {
                "darwin-arm64"
            } else {
                "linux-x86_64"
            }
            .to_owned(),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        }),
    };
    fs::write(
        root.join("var/lib/kbf-installer/orders")
            .join(record.file_name()),
        record.to_bytes().expect("bytes"),
    )
    .expect("record");
    let out = helper(&root, &run);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(status(&root)["state"], "refused");
    let why = status(&root)["refusal"].as_str().expect("text").to_owned();
    assert!(
        why.contains("lease source"),
        "the order's own refusal now: {why}"
    );
}

/// Catches the start check that does not count: with a release on trial, the unit's
/// ExecStartPre uses one of its starts and starts the window. The revert with none left is
/// the installer's (tested on a host that plays root); here the helper must only refuse to
/// change the links when it is not root.
///
/// A state file that is not readable stops the start (exit 1) instead of guessing.
#[test]
fn the_boot_check_counts_the_starts_and_reverts_the_last() {
    let root = root("boot");
    let host = layout(&root);
    for commit in [A, B] {
        let dir = host.join(&host.release_dir(&c(commit)));
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"x").expect("binary");
    }
    let lock = Locked::acquire(&host).expect("lock");
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
    let out = helper(&root, &["boot-check", "--window-secs", "600"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let left = |root: &Path| {
        kbf_installer::state::read_unlocked(&layout(root))
            .expect("reads")
            .expect("there")
    };
    assert_eq!(
        left(&root).pending_confirm.expect("on trial").starts_left,
        Some(0)
    );
    // The last start's turn: reverting changes the links, which only root may do. Run as
    // the test's own user it refuses, and leaves the swap as it was.
    let out = helper(&root, &["boot-check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("as root"));
    // The state is written first and the links follow (reconcile, as root): the swap is
    // already back on A in the state.
    assert_eq!(left(&root).current, Some(c(A)));

    // A damaged state.json is read from the backup: the pass goes on (it is not exit 5).
    fs::write(in_state_dir(&root, "state.json"), b"not json").expect("ruin");
    let out = helper(&root, &["boot-check"]);
    assert_ne!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // With the backup damaged too, nobody can say what the host runs: exit 5, needs an
    // operator, and nothing is written.
    for name in ["state.json", "state.json.bak"] {
        fs::write(in_state_dir(&root, name), b"not json").expect("ruin");
    }
    let out = helper(&root, &["boot-check"]);
    assert_eq!(out.status.code(), Some(5));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("kbf-installer:") && err.contains("unusable"),
        "{err}"
    );
    assert_eq!(
        fs::read(in_state_dir(&root, "state.json")).expect("kept"),
        b"not json"
    );
}

/// Catches a helper that starts without being told what to restart: neither flag is a
/// usage error (exit 2), and both together are too; a relative `--root` is refused.
#[test]
fn the_service_must_be_named_exactly_once() {
    let root = root("flags");
    assert_eq!(
        helper(&root, &["run", "--passes", "1"]).status.code(),
        Some(2)
    );
    let both = [
        "run",
        "--passes",
        "1",
        "--unit",
        "a",
        "--launchd-label",
        "b",
    ];
    assert_eq!(helper(&root, &both).status.code(), Some(2));
    // The same for `run`: a relative root is refused before anything is read.
    let out = Command::new(BIN)
        .args(["--root", "relative", "run", "--passes", "1", "--unit", "a"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
    let out = Command::new(BIN)
        .args(["--root", "relative", "boot-check"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
}

/// Catches a loop that does not loop or does not stop: two passes with a launchd label
/// (the label is taken, and no unit is needed) end on their own; a helper.json the helper
/// cannot read ends the run with an exit of 1 and a message, not a guess.
///
/// Mutant seen red: `passes` ignored (the run never ends; the test would hang), the
/// decrement dropped.
#[test]
fn passes_end_the_run_and_an_unreadable_memory_stops_it() {
    let root = root("passes");
    Locked::acquire(&layout(&root))
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    let two = ["run", "--passes", "2", "--launchd-label", "com.example.kbf"];
    let out = helper(&root, &two);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(in_state_dir(&root, "helper.json"), b"not json").expect("ruin");
    let out = helper(&root, &two);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("helper.json"));
    // A directory the helper cannot write status.json into ends the pass with an error.
    fs::remove_file(in_state_dir(&root, "helper.json")).expect("remove");
    let mut elsewhere = two.to_vec();
    elsewhere.extend(["--update-dir", "/no/such/dir"]);
    let out = helper(&root, &elsewhere);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("kbf-installer:"));
}

/// Catches the operator verbs' exit codes (contract section 2) going wrong or a verb run
/// without `--apply` changing the host: with nothing on trial, `confirm`, `revert` and
/// `rollback` need the operator (exit 5), `quiesce` is at rest (0); with a swap on trial
/// `quiesce` is exit 5 and a dry-run `revert` leaves the state alone; `gc` without
/// `--apply` removes nothing.
///
/// Mutants planted: `Failure::exit_code` returning 1 for every case; `quiesce` not reading
/// the pending swap.
#[test]
fn the_operator_verbs_exit_by_the_contract() {
    let root = root("verbs");
    let host = layout(&root);
    let service = ["--unit", "kbf.service"];
    let state = |pending| State {
        current: Some(c(B)),
        previous: Some(c(A)),
        pending_confirm: pending,
        ..State::default()
    };
    Locked::acquire(&host)
        .expect("lock")
        .write(&state(None))
        .expect("state");
    for dir in [A, B] {
        let d = root.join("var/lib/kbf-installer/releases").join(dir);
        fs::create_dir_all(&d).expect("release");
        fs::write(d.join("kbf"), b"x").expect("binary");
    }
    let code = |verb: &[&str]| helper(&root, verb).status.code();
    let with = |verb: &str, extra: &[&str]| {
        let mut v = vec![verb];
        v.extend(extra);
        v.extend(service);
        v.iter().map(ToString::to_string).collect::<Vec<_>>()
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        code(&refs)
    };
    assert_eq!(run(with("confirm", &["--id", "1-5"])), Some(5));
    assert_eq!(run(with("revert", &["--id", "1-5"])), Some(5));
    assert_eq!(code(&["quiesce"]), Some(0));

    let pending = PendingConfirm {
        commit: c(B),
        previous: Some(c(A)),
        activation: "1-5".to_owned(),
        deadline_wall: Some(2_000_000_000),
        deadline_mono_s: None,
        starts_left: Some(3),
    };
    Locked::acquire(&host)
        .expect("lock")
        .write(&state(Some(pending)))
        .expect("state");
    assert_eq!(code(&["quiesce"]), Some(5));
    assert_eq!(run(with("rollback", &[])), Some(5), "a swap is on trial");
    assert_eq!(run(with("revert", &["--id", "1-9"])), Some(3));
    assert_eq!(run(with("revert", &["--id", "1-5"])), Some(0));
    let kept = kbf_installer::state::read_unlocked(&host)
        .expect("reads")
        .expect("there");
    assert_eq!(kept.current, Some(c(B)), "a dry run moves nothing");
    assert_eq!(code(&["gc"]), Some(0));
    assert!(root.join("var/lib/kbf-installer/releases").join(A).exists());
}

/// Catches the gate in front of the verbs being absent from the shipped binary (the unit tests
/// run the same code, but the binary is what a host runs): an ordinary user is refused every
/// verb that needs root with exit 2 and the reason, `--json` puts one object on standard
/// output, and a verb that needs no root and is not built yet says which pull request builds it.
///
/// Mutant planted: the root check skipped (`setup` then reaches "not implemented", exit 1).
#[test]
fn the_shipped_binary_refuses_a_verb_that_needs_root_and_names_the_unbuilt_ones() {
    let root = root("gate");
    let out = helper(&root, &["setup", "--mode=daemon"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("setup needs root"));
    let out = helper(&root, &["--json", "setup", "--mode=daemon"]);
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one object");
    assert_eq!(v["error"], "needs_root");
    let out = helper(&root, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("status: not implemented (P4)"));
}

/// Catches a service that goes on running a file that was replaced under it, or that hides
/// it: once the program's own file is another file, the loop says so and starts the new
/// one in its place; here the new file is no program, so the start fails and the service
/// ends with the reason (exit 1) instead of running on or hanging.
///
/// Mutants planted: the watch never consulted (the loop runs all its passes and exits 0);
/// the failed start not reported.
#[test]
fn a_service_whose_file_was_replaced_says_so_and_ends_when_the_new_one_cannot_start() {
    use std::process::Stdio;
    let root = root("replaced");
    Locked::acquire(&layout(&root))
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    let dir = root.join("copy");
    fs::create_dir_all(&dir).expect("dir");
    let me = dir.join("kbf-installer");
    fs::copy(BIN, &me).expect("a copy of the program");
    let child = Command::new(&me)
        .arg("--root")
        .arg(&root)
        .args(["run", "--passes", "4", "--unit", "kbf.service"])
        .stderr(Stdio::piped())
        .spawn()
        .expect("starts");
    let status_file = root.join("var/lib/kbf-installer/status.json");
    for _ in 0..400 {
        if status_file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(status_file.exists(), "the first pass was made");
    let other = dir.join("other");
    fs::write(&other, b"not a program").expect("a file that is no program");
    fs::rename(&other, &me).expect("replaced");
    let out = child.wait_with_output().expect("ends");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("my file was replaced"), "{stderr}");
    assert!(stderr.contains("cannot start"), "{stderr}");
}

/// Catches a verb that queues behind another run for ever, or hides that it is waiting: with
/// the lock held by another process, `quiesce` waits its time and then ends busy (exit 4),
/// which tells a maintainer script to ask again.
///
/// Mutant planted: the busy error mapped to a plain failure (exit 1).
#[test]
fn a_verb_that_finds_the_lock_held_ends_busy() {
    let root = root("busy");
    Locked::acquire(&layout(&root))
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    let held = Locked::acquire(&layout(&root)).expect("held by this test");
    let out = helper(&root, &["quiesce"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    drop(held);
    assert_eq!(helper(&root, &["quiesce"]).status.code(), Some(0));
}

/// Catches the state errors of an operator's verb landing on the wrong exit in the shipped
/// binary: both state files damaged is an operator's job (exit 5), and a lock file that cannot
/// even be opened is a plain failure (exit 1).
///
/// Mutants planted: the unusable state mapped to a plain failure; the lock error to busy.
#[test]
fn the_operators_verbs_end_by_the_state_error_they_met() {
    let root = root("state-errors");
    let host = layout(&root);
    Locked::acquire(&host)
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    for name in ["state.json", "state.json.bak"] {
        fs::write(in_state_dir(&root, name), b"not json").expect("ruin");
    }
    let out = helper(&root, &["confirm", "--id", "1-5", "--unit", "kbf.service"]);
    assert_eq!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lock = host.join(&host.lock());
    fs::remove_dir_all(lock.parent().expect("a directory")).expect("no lock directory");
    let out = helper(&root, &["quiesce"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
