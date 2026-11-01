//! The host rules `kbf-deploy linux check-units` had (rules `credential` and `traverse`),
//! in the `kbf` binary: `kbf <mode> --check` refuses what they refused, reading the
//! process's own environment and ids. (Each rule's cases, and the key file and cgroup
//! rules, are the unit tests of `src/preflight`; a process test cannot plant
//! `/etc/kbf/s3.env` or the node's cgroup.) Every run passes `--config` so the host's own
//! `/etc/kbf/kbf.toml` is not read.

use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("host-rules-{name}"))
}

fn kbf(dir: &std::path::Path, mode: &str, args: &[String]) -> Command {
    let mut c = Command::new(BIN);
    c.arg(mode)
        .arg(kbf_testkit::config::flag(dir))
        .arg("--check")
        .args(args)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY");
    c
}

fn daemon(dir: &std::path::Path) -> Command {
    kbf(
        dir,
        "daemon",
        &[
            "--driver=native".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            "--node-id=n".to_owned(),
            format!("--ca-cert={}", dir.join("ca.pem").display()),
            format!("--cert={}", dir.join("node.pem").display()),
            format!("--key={}", dir.join("node.key").display()),
            format!("--data-dir={}", dir.join("data").display()),
        ],
    )
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Catches rule `credential` not wired to a start: a daemon whose environment holds the
/// key pair exits 2 naming both variables, and the same daemon without them says nothing
/// about the environment (its own missing files are the only problems).
#[test]
fn a_daemon_with_a_key_in_its_environment_is_refused() {
    let dir = scratch("aws");
    let with = daemon(&dir)
        .env("AWS_ACCESS_KEY_ID", "a")
        .env("AWS_SECRET_ACCESS_KEY", "b")
        .output()
        .expect("run kbf");
    let err = stderr(&with);
    assert_eq!(with.status.code(), Some(2), "{err}");
    assert!(
        err.contains("the environment sets AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY"),
        "{err}"
    );
    let without = daemon(&dir).output().expect("run kbf");
    let err = stderr(&without);
    assert_eq!(without.status.code(), Some(2), "{err}");
    assert!(!err.contains("the environment sets"), "{err}");
}

/// Catches rule `traverse` not wired to a start: a storage node whose `--hot-store` is
/// below a directory its user cannot enter exits 2 naming it. Skipped as root, which
/// enters every directory.
#[test]
fn a_hot_store_below_a_directory_the_user_cannot_enter_is_refused() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let dir = scratch("traverse");
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).expect("mkdir");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("mode");
    let out = kbf(
        &dir,
        "storage",
        &[
            "--storage-listen=127.0.0.1:0".to_owned(),
            format!("--cold-store=file://{}", dir.join("cold").display()),
            "--cold-store-floor-bytes=1".to_owned(),
            format!("--hot-store=file://{}", locked.join("hot").display()),
            format!("--meta-dir={}", dir.join("meta").display()),
        ],
    )
    .output()
    .expect("run kbf");
    // Let the scratch directory be removed.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!("{} (mode 0000", locked.display())) && err.contains("cannot enter"),
        "{err}"
    );
}

/// Catches the key file looked for only at the default place: a storage node started with
/// `--config` in another directory, beside which `s3.env` is readable by everyone, is
/// refused at start, naming that file.
#[test]
fn a_lax_key_file_beside_the_loaded_config_is_refused() {
    let dir = scratch("keyfile");
    let key = dir.join("s3.env");
    std::fs::write(&key, "AWS_ACCESS_KEY_ID=a\n").expect("write");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let out = kbf(
        &dir,
        "storage",
        &[
            "--storage-listen=127.0.0.1:0".to_owned(),
            format!("--cold-store=file://{}", dir.join("cold").display()),
            "--cold-store-floor-bytes=1".to_owned(),
            format!("--meta-dir={}", dir.join("meta").display()),
        ],
    )
    .output()
    .expect("run kbf");
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains(&format!("{} is mode 0644", key.display())),
        "{err}"
    );
}
