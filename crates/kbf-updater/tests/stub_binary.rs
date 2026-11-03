//! The `kbf-updater` binary as a process: `--version`, the usage errors, and a start that
//! fails (the serving path is covered in-process, where a test can stop it).

use std::process::Command;

fn run<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-updater"))
        .args(args)
        .output()
        .expect("run kbf-updater");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("stdout"),
        String::from_utf8(out.stderr).expect("stderr"),
    )
}

/// Catches a binary that prints its answer on the wrong stream or exits zero on a usage error
/// (a unit file would then start a "service" that serves nothing and call it healthy).
#[test]
fn version_goes_to_stdout_with_zero_and_usage_to_stderr_with_two() {
    let (code, out, err) = run(&["--version"]);
    assert_eq!(code, Some(0));
    assert!(out.starts_with("kbf-updater 0."), "{out}");
    assert_eq!(err, "");

    let (code, out, err) = run::<&str>(&[]);
    assert_eq!(code, Some(2));
    assert_eq!(out, "");
    assert!(err.contains("usage: kbf-updater --version"), "{err}");
    assert!(err.contains("--state-dir is required"), "{err}");
}

/// Catches a binary that starts without a root-owned inventory it can read, or one that
/// takes a non-loopback operator API or an ssh key that is no file: a missing inventory is exit 1 with the reason on
/// stderr and nothing on stdout; a non-loopback API is a usage error before anything opens.
#[test]
fn a_bad_start_exits_nonzero_with_the_reason_on_stderr() {
    let dir = std::env::temp_dir();
    // Any file will do as the key: nothing connects.
    let key = env!("CARGO_BIN_EXE_kbf-updater");
    let flags = |api: &str, key: &str| {
        vec![
            "--state-dir".to_owned(),
            dir.display().to_string(),
            "--inventory".to_owned(),
            "/nonexistent/inventory".to_owned(),
            "--socket".to_owned(),
            "/nonexistent/api.sock".to_owned(),
            "--proxy-uid".to_owned(),
            "1".to_owned(),
            "--operator-api".to_owned(),
            api.to_owned(),
            "--token-file".to_owned(),
            "/nonexistent/token".to_owned(),
            "--ssh-key-file".to_owned(),
            key.to_owned(),
            "--ssh-user".to_owned(),
            "kbf".to_owned(),
            "--channel".to_owned(),
            "stable".to_owned(),
        ]
    };
    let (code, out, err) = run(&flags("http://192.0.2.7:8080", key));
    assert_eq!((code, out.as_str()), (Some(2), ""));
    assert!(err.contains("loopback"), "{err}");
    let (code, out, err) = run(&flags("http://127.0.0.1:8080", key));
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.starts_with("kbf-updater: --token-file: "), "{err}");
    let (code, out, err) = run(&flags("http://127.0.0.1:8080", "/nonexistent/key"));
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(
        err.starts_with("kbf-updater: --ssh-key-file /nonexistent/key is not a file"),
        "{err}"
    );
}

/// The binary as the packaged unit starts it on a host not enrolled yet: `kbf-updater agent` with
/// an empty state directory writes exactly one line on stderr, the prefixed "not enrolled" line
/// that names the directory, nothing on stdout, and is still running, waiting, once that line is
/// out (it is stopped by the test then: the binary has no stop of its own but its unit's).
///
/// Catches: a binary that says something else on the error stream (the operator's journal), or
/// that exits once it found no enrollment, which its unit would restart in a loop.
#[test]
fn the_agent_binary_says_once_that_it_is_not_enrolled_and_keeps_waiting() {
    use std::io::{BufRead as _, Read as _};
    use std::process::Stdio;

    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-updater-binary-waits");
    let state = dir.join("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kbf-updater"))
        .args(["agent", "--coordinator", "coordinator.farm.example:8443"])
        .args(["--enroll-frontend", "frontend.farm.example:8444"])
        .arg("--state-dir")
        .arg(&state)
        .args(["--channel", "stable"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run kbf-updater");
    let mut stderr = std::io::BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    stderr.read_line(&mut line).expect("the first line");
    // The line is out, so the wait has begun: the agent is still running.
    assert_eq!(child.try_wait().expect("status"), None, "{line}");
    child.kill().expect("stopped");
    child.wait().expect("reaped");
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).expect("the rest");
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("stdout");
    assert_eq!(
        line,
        format!(
            "kbf-updater: not enrolled: {0} holds no enrollment yet; waiting for `kbf-updater \
             enroll --state-dir {0}`, and starting once it has run\n",
            state.display()
        )
    );
    assert_eq!((rest.as_str(), out.as_str()), ("", ""));
}
