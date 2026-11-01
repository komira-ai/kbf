//! The `kbf-updater root` binary end to end, as root, under the real systemd: a fake service is
//! declared in a directory standing for `/etc/updater.d`, a release of it is staged and sealed,
//! and the agent's order runs its hook through the step runner, each step a transient unit as the
//! declared account.
//!
//! Needs Linux, systemd and root, so it is `#[ignore]`d and runs in the `update-rights-linux`
//! workflow through `cargo xtask sudo-test -p kbf-updater --test root_part_root`. No test here
//! decides anything by how long it takes: it waits for the socket to answer and for the hook's
//! log, and each wait is ended by nextest's guard if what it waits for never comes.

#![cfg(target_os = "linux")]

mod root_support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use kbf_updater::root::socket::{self, Request};
use root_support::{HOOK, dir, put};
use update_hook::Verdict;
use update_rights::{Accounts, EtcAccounts};

const SERVICE: &str = "kbfue2e";

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
        .spawn()
        .expect("the root part starts");
    // The socket is bound once the start pass is over.
    let socket = base.join("run/root.sock");
    while UnixStream::connect(&socket).is_err() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the root part exited before it served: {status}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Running(child)
}

fn steps(base: &Path) -> Vec<String> {
    fs::read_to_string(base.join("hooks").join(SERVICE).join("steps.log"))
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
    let state_dir = base.join("hooks").join(SERVICE);
    let declaration = format!(
        "name = \"{SERVICE}\"\n\
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
         environment = {{ HOOK_RECORD_DIR = \"{state}\" }}\n\
         [hook.timeout_secs]\n\
         prepare = 60\ncutover = 60\nverify = 60\nrollback = 60\ncleanup = 60\n",
        state = state_dir.display(),
    );
    put(
        &base.join("updater.d").join(format!("{SERVICE}.toml")),
        declaration.as_bytes(),
        0o644,
    );
    let tree = base.join("releases").join(SERVICE).join("1.0.0");
    dir(&tree, 0o755);
    fs::copy(HOOK, tree.join("hook")).unwrap();
    fs::set_permissions(tree.join("hook"), fs::Permissions::from_mode(0o755)).unwrap();
    let digest = kbf_step_runner::tree_digest(&tree).unwrap();
    put(
        &base.join("releases").join(SERVICE).join("1.0.0.seal"),
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
            service: SERVICE.to_owned(),
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
        &fs::read(base.join("state").join(SERVICE).join("step-record.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["state"]["state"], "finished");
    assert_eq!(record["target_version"], "1.0.0");
    drop(root);

    let again = start(base);
    assert_eq!(
        steps(base),
        ["prepare", "cutover", "verify", "cleanup", "restore"],
        "the boot entry ran before the socket answered"
    );
    drop(again);
}
