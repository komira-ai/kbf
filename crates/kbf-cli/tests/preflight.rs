//! The startup self-checks in the `kbf` binary: a mode on a host that lacks what it needs
//! exits 2 with every missing piece on stderr, one line each, before it opens a store or
//! a socket. (Each check, and the exit classes, are the unit tests of `src/preflight`.)

use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches a start that stops at the first problem, or that goes on to start on a host it
/// should refuse: a daemon whose CA file is missing, whose certificate file holds no
/// certificate and whose key is readable by its group exits 2 naming all three, and
/// prints nothing on stdout.
#[test]
fn a_daemon_on_a_wrong_host_names_every_problem_and_exits_2() {
    let dir = scratch("preflight-daemon");
    let cert = dir.join("node.pem");
    let key = dir.join("node.key");
    std::fs::write(&cert, "not a certificate").expect("write");
    std::fs::write(&key, "key").expect("write");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).expect("mode");
    let out = Command::new(BIN)
        .args([
            "daemon",
            "--driver=native",
            "--server=https://127.0.0.1:1",
            "--node-id=n",
        ])
        .arg(format!("--ca-cert={}", dir.join("ca.pem").display()))
        .arg(format!("--cert={}", cert.display()))
        .arg(format!("--key={}", key.display()))
        .arg(format!("--data-dir={}", dir.join("data").display()))
        .output()
        .expect("run kbf");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(out.stdout.is_empty());
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 3, "{err}");
    assert!(lines.iter().all(|l| l.starts_with("kbf daemon: ")), "{err}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("--ca-cert") && l.contains("does not exist")),
        "{err}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("--cert") && l.contains("holds no certificate")),
        "{err}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("--key") && l.contains("chmod 600")),
        "{err}"
    );
    assert!(
        !dir.join("data").exists(),
        "a refused start made its data directory"
    );
}

/// Catches a storage node with an `s3://` cold store and no key pair in its environment
/// that starts (and fails at the first blob), or that names one variable of two: it
/// exits 2 naming both before it opens the metadata directory.
#[test]
fn a_storage_node_without_its_s3_key_pair_names_both_variables_and_exits_2() {
    let dir = scratch("preflight-storage");
    let out = Command::new(BIN)
        .args([
            "storage",
            "--storage-listen=127.0.0.1:0",
            "--cold-store=s3://bucket/kbf/?endpoint=http://127.0.0.1:1",
        ])
        .arg(format!("--hot-store=file://{}", dir.join("hot").display()))
        .arg(format!("--meta-dir={}", dir.join("meta").display()))
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .output()
        .expect("run kbf");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert_eq!(err.lines().count(), 2, "{err}");
    assert!(err.contains("AWS_ACCESS_KEY_ID is not set"), "{err}");
    assert!(err.contains("AWS_SECRET_ACCESS_KEY is not set"), "{err}");
    assert!(
        !dir.join("meta").exists(),
        "a refused start opened its store"
    );
}
