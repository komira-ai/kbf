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
/// takes a non-loopback operator API: a missing inventory is exit 1 with the reason on
/// stderr and nothing on stdout; a non-loopback operator API or proxy address is a usage error before anything opens.
#[test]
fn a_bad_start_exits_nonzero_with_the_reason_on_stderr() {
    let dir = std::env::temp_dir();
    let flags_with = |api: &str, proxy: &str| {
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
            "--proxy-api".to_owned(),
            proxy.to_owned(),
            "--proxy-ca-file".to_owned(),
            "/nonexistent/ca.pem".to_owned(),
        ]
    };
    let flags = |api: &str| flags_with(api, "https://127.0.0.1:8443");
    let (code, out, err) = run(&flags("http://192.0.2.7:8080"));
    assert_eq!((code, out.as_str()), (Some(2), ""));
    assert!(err.contains("loopback"), "{err}");
    let (code, out, err) = run(&flags_with(
        "http://127.0.0.1:8080",
        "https://192.0.2.7:8443",
    ));
    assert_eq!((code, out.as_str()), (Some(2), ""));
    assert!(
        err.contains("--proxy-api must be a loopback address"),
        "{err}"
    );
    let (code, out, err) = run(&flags("http://127.0.0.1:8080"));
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.starts_with("kbf-updater: --token-file: "), "{err}");
}
