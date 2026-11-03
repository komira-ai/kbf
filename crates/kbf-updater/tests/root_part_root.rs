//! The `kbf-updater root` binary end to end, as root, under the real systemd: a fake service is
//! declared in a directory standing for `/etc/updater.d`, a release of it is staged and sealed,
//! and the agent's order runs its hook through the step runner, each step a transient unit as the
//! declared account.
//!
//! Needs Linux, systemd and root, so it is `#[ignore]`d and runs in the `update-rights-linux`
//! workflow through `cargo xtask sudo-test -p kbf-updater --test root_part_root`. No test here
//! decides anything by how long it takes: it waits for the root part to say it serves the agent,
//! and each wait is ended by nextest's guard if what it waits for never comes.

#![cfg(target_os = "linux")]

mod root_support;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use kbf_updater::root::socket::{self, Request};
use kbf_updater::root::test_hook::RECORD_DIR_FILE;
use root_support::{HOOK, dir, put};
use update_hook::Verdict;
use update_rights::{Accounts, EtcAccounts};

/// The fake service, its name this process's: two runs share no unit, record or directory.
fn service() -> String {
    format!("kbfue2e-{}", std::process::id())
}

/// What the root part writes when it starts answering the agent.
const SERVING: &str = "kbf-updater root: serving the agent at ";

/// The directory of the test under `/var/lib` (the hook's units have a private `/tmp` and no
/// home), removed with everything in it when the test ends.
struct Base(PathBuf);

impl Drop for Base {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The root part, killed when the test ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(base: &Path) -> Running {
    let path = |sub: &str| base.join(sub).display().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kbf-updater"))
        .args([
            "root",
            "--socket",
            &path("run/root.sock"),
            "--socket-user",
            "nobody",
            "--declarations",
            &path("updater.d"),
            "--approvals",
            &path("updater.d/approved"),
            "--state-dir",
            &path("state"),
            "--releases-dir",
            &path("releases"),
        ])
        .stderr(Stdio::piped())
        .spawn()
        .expect("the root part starts");
    // It says when it serves the agent, once the start pass is over and the socket is bound; the
    // rest of what it writes is passed on.
    let mut lines = BufReader::new(child.stderr.take().expect("its standard error")).lines();
    let serving = lines.by_ref().map_while(Result::ok).find(|line| {
        eprintln!("{line}");
        line.starts_with(SERVING)
    });
    if serving.is_none() {
        let status = child.wait().unwrap();
        panic!("the root part exited before it served: {status}");
    }
    std::thread::spawn(move || {
        lines
            .map_while(Result::ok)
            .for_each(|line| eprintln!("{line}"));
    });
    Running(child)
}

fn steps(base: &Path) -> Vec<String> {
    fs::read_to_string(base.join("hooks").join(service()).join("steps.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Catches the root part not running a declared hook as the step runner says, under the rights
/// of its declaration: the binary, given the order to update the fake service, runs `prepare`,
/// `cutover`, `verify` and `cleanup` in order, each as the declared account (`nobody`, who wrote
/// the hook's log), writes the step record, and answers `Succeeded`. Started again, it runs the
/// declaration's boot entry from that release before it answers anyone.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn the_root_part_binary_runs_a_declared_hook_through_systemd_and_writes_the_step_record() {
    let base = Base(PathBuf::from(format!(
        "/var/lib/kbfu-e2e-{}",
        std::process::id()
    )));
    let base = &base.0;
    let _ = fs::remove_dir_all(base);
    for sub in ["", "updater.d", "releases", "run", "hooks"] {
        dir(&base.join(sub), 0o755);
    }
    let service = service();
    let state_dir = base.join("hooks").join(&service);
    let declaration = format!(
        "name = \"{service}\"\n\
         binary = \"hook\"\n\
         signer = {{ repository = \"komira-ai/kbf\", workflow = \"artifacts.yml\" }}\n\
         uid = \"daemon\"\n\
         verify_deadline_secs = 120\n\
         state_dir = \"{state}\"\n\
         boot = \"restore\"\n\
         [hook]\n\
         user = \"nobody\"\n\
         group = \"nogroup\"\n\
         address_families = [\"AF_UNIX\"]\n\
         [hook.timeout_secs]\n\
         prepare = 60\ncutover = 60\nverify = 60\nrollback = 60\ncleanup = 60\n",
        state = state_dir.display(),
    );
    put(
        &base.join("updater.d").join(format!("{service}.toml")),
        declaration.as_bytes(),
        0o644,
    );
    let tree = base.join("releases").join(&service).join("1.0.0");
    dir(&tree, 0o755);
    fs::copy(HOOK, tree.join("hook")).unwrap();
    // The hook's record directory, which it reads beside itself.
    put(
        &tree.join(RECORD_DIR_FILE),
        state_dir.display().to_string().as_bytes(),
        0o644,
    );
    fs::set_permissions(tree.join("hook"), fs::Permissions::from_mode(0o755)).unwrap();
    let digest = kbf_step_runner::tree_digest(&tree).unwrap();
    put(
        &base.join("releases").join(&service).join("1.0.0.seal"),
        format!(
            "{{\"tree_digest\":\"{digest}\",\"artifact_digest\":\"{}\"}}",
            "ab".repeat(32)
        )
        .as_bytes(),
        0o644,
    );

    let root = start(base);
    let socket_path = base.join("run/root.sock");
    let meta = fs::symlink_metadata(&socket_path).unwrap();
    let accounts = EtcAccounts::system().unwrap();
    let nobody = accounts.user("nobody").expect("an account nobody");
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!(
        meta.uid(),
        nobody.uid,
        "the agent's account owns the socket"
    );

    let answer = socket::request(
        &socket_path,
        &Request::Update {
            service: service.clone(),
            version: "1.0.0".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(
        answer.verdict,
        Some(Verdict::Succeeded {
            promotion_pending: false
        }),
        "{answer:?}"
    );
    assert_eq!(steps(base), ["prepare", "cutover", "verify", "cleanup"]);
    let log = fs::metadata(state_dir.join("steps.log")).unwrap();
    assert_eq!(
        log.uid(),
        nobody.uid,
        "the hook ran as the declared account"
    );
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(base.join("state").join(&service).join("step-record.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"]["state"], "finished");
    assert_eq!(record["target_version"], "1.0.0");
    // The hook, as `nobody`, read the params file it was given.
    let seen = fs::read_to_string(state_dir.join("params.seen")).unwrap();
    assert!(seen.contains("updater.target_version"), "{seen}");
    drop(root);
    // The service's group may enter the record directory (its units execute from it); a start
    // leaves that as it is.
    let daemon = accounts.group("daemon").expect("a group daemon");
    std::os::unix::fs::chown(&state_dir, None, Some(daemon)).unwrap();
    fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o750)).unwrap();

    let again = start(base);
    assert_eq!(
        steps(base),
        ["prepare", "cutover", "verify", "cleanup", "restore"],
        "the boot entry ran before the socket answered"
    );
    let record_dir = fs::metadata(&state_dir).unwrap();
    assert_eq!(
        record_dir.gid(),
        daemon,
        "the record directory keeps its group"
    );
    assert_eq!(record_dir.mode() & 0o777, 0o750);
    drop(again);
}
