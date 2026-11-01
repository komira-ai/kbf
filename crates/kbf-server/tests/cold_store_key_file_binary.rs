//! The `kbf-server` binary and `--cold-store-credentials-file`: a key file it must refuse
//! stops the start with exit 2 and a message that names the flag and the path and holds no
//! byte of the file; the key pair in the environment still works and is warned of once; and
//! no secret reaches the log either way.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");
const ANY_PORT: [&str; 4] = ["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"];
const IDLE_S3: &str = "s3://kbf-test?endpoint=http://127.0.0.1:9";
const FILE_ID: &str = "FILEKEYID0001";
const FILE_SECRET: &str = "file-secret-5d2e9";
const ENV_ID: &str = "ENVKEYID0002";
const ENV_SECRET: &str = "env-secret-81c7f";

/// What a run left: the first line of stdout (the start line; empty if the server exited
/// without one), the exit status code, and stderr.
struct Run {
    start: String,
    code: Option<i32>,
    stderr: String,
}

/// Kills the child when the test ends, however it ends.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs the server over the S3 cold store with `extra` flags and `env` variables: until its
/// start line, which is then stopped with SIGINT, or until it exits without one. A server
/// that starts is stopped; one that refuses has already exited, so nothing waits on a timer.
fn run(name: &str, extra: &[&str], env: &[(&str, &str)]) -> Run {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let mut command = Command::new(BIN);
    command
        .args(["--cold-store", IDLE_S3])
        .args(ANY_PORT)
        .arg("--meta-dir")
        .arg(dir.join("meta"))
        .args(extra)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Reaped(command.spawn().expect("spawn kbf-server"));
    let mut start = String::new();
    let stdout = child.0.stdout.take().expect("stdout");
    BufReader::new(stdout)
        .read_line(&mut start)
        .expect("stdout is UTF-8");
    if !start.is_empty() {
        let sent = Command::new("kill")
            .args(["-INT", &child.0.id().to_string()])
            .status()
            .expect("run kill");
        assert!(sent.success());
    }
    let status = child.0.wait().expect("exit");
    let mut stderr = String::new();
    let mut pipe = child.0.stderr.take().expect("stderr");
    pipe.read_to_string(&mut stderr).expect("stderr is UTF-8");
    Run {
        start,
        code: status.code(),
        stderr,
    }
}

/// A key file `name` in a directory of its own, holding the file's pair at `mode`.
fn key_file(name: &str, mode: u32) -> (kbf_testkit::dir::TestDir, std::path::PathBuf) {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let path = dir.join("cold.json");
    let text = format!(r#"{{"access_key_id": "{FILE_ID}", "secret_access_key": "{FILE_SECRET}"}}"#);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    (dir, path)
}

fn flag(path: &Path) -> String {
    format!("--cold-store-credentials-file={}", path.display())
}

/// Catches: a key file others can read that starts the server, an exit other than 2 (the
/// unit restarts anything else), a message without the flag or the path, and a message with
/// the secret in it.
#[test]
fn a_bad_key_file_exits_2_naming_flag_and_path_and_never_the_secret() {
    let (_dir, wide) = key_file("cold-key-bin-wide", 0o644);
    let ran = run("cold-key-bin-wide-run", &[&flag(&wide)], &[]);
    assert_eq!(ran.start, "", "it started: {}", ran.stderr);
    assert_eq!(ran.code, Some(2), "{}", ran.stderr);
    assert!(
        ran.stderr.contains("--cold-store-credentials-file"),
        "{}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains(&wide.display().to_string()),
        "{}",
        ran.stderr
    );
    assert!(!ran.stderr.contains(FILE_SECRET), "{}", ran.stderr);
    assert!(!ran.stderr.contains(FILE_ID), "{}", ran.stderr);
}

/// Catches: a key file with a `file://` cold store accepted, and the flag taken without a
/// cold store at all.
#[test]
fn the_key_file_needs_an_s3_cold_store() {
    let (_dir, path) = key_file("cold-key-bin-file-store", 0o600);
    let base = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cold-key-bin-fs");
    let cold = format!("file://{}", base.join("cold").display());
    let mut command = Command::new(BIN);
    command
        .args(["--cold-store", &cold, "--cold-store-floor-bytes=1"])
        .args(ANY_PORT)
        .arg("--meta-dir")
        .arg(base.join("meta"))
        .arg(flag(&path))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Reaped(command.spawn().expect("spawn kbf-server"));
    let mut start = String::new();
    let stdout = child.0.stdout.take().expect("stdout");
    BufReader::new(stdout)
        .read_line(&mut start)
        .expect("stdout");
    assert_eq!(start, "", "it started");
    let status = child.0.wait().expect("exit");
    assert_eq!(status.code(), Some(2));
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("stderr");
    assert!(stderr.contains("--cold-store-credentials-file"), "{stderr}");

    let alone = Command::new(BIN)
        .arg(flag(&path))
        .args(ANY_PORT)
        .output()
        .expect("run kbf-server");
    assert_eq!(alone.status.code(), Some(2));
    let said = String::from_utf8_lossy(&alone.stderr);
    assert!(said.contains("--cold-store"), "{said}");
}

/// Catches: a start on the key file that warns of the environment it did not read, and a
/// secret or key id in the start line or the log.
#[test]
fn no_secret_in_start_log_or_error() {
    let (_dir, path) = key_file("cold-key-bin-good", 0o600);
    let ran = run("cold-key-bin-good-run", &[&flag(&path)], &[]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(ran.start.starts_with("kbf-server "), "{:?}", ran.start);
    for text in [&ran.start, &ran.stderr] {
        assert!(!text.contains(FILE_SECRET), "{text}");
        assert!(!text.contains(FILE_ID), "{text}");
    }
    assert!(!ran.stderr.contains("deprecated"), "{}", ran.stderr);
}

/// Catches: the environment's pair refused already (pilot units rely on it for one release),
/// no warning, more than one, and a warning that names the value of either variable.
#[test]
fn environment_keys_start_the_server_with_one_deprecation_warning() {
    let env = [
        ("AWS_ACCESS_KEY_ID", ENV_ID),
        ("AWS_SECRET_ACCESS_KEY", ENV_SECRET),
    ];
    let ran = run("cold-key-bin-env-run", &[], &env);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(ran.start.starts_with("kbf-server "), "{:?}", ran.start);
    let warned: Vec<&str> = ran
        .stderr
        .lines()
        .filter(|l| l.contains("deprecated"))
        .collect();
    assert_eq!(warned.len(), 1, "{}", ran.stderr);
    assert!(
        warned[0].contains("--cold-store-credentials-file"),
        "{}",
        warned[0]
    );
    for text in [&ran.start, &ran.stderr] {
        assert!(!text.contains(ENV_SECRET), "{text}");
        assert!(!text.contains(ENV_ID), "{text}");
    }
}
