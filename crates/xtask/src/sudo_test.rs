//! `cargo xtask sudo-test -p <package> --test <name> [-- <test args>]`: one integration
//! test target of one package, built as the runner's user and run as root.
//!
//! Cargo builds the target with `cargo test --locked --no-run --message-format=json -p
//! <package> --test <name>` in the repository root, so nothing is compiled as root and
//! `target/` keeps its owner. [`find`] takes the test binary's path from cargo's JSON
//! messages. The binary then runs through `sudo -n <binary> --ignored <test args>` in
//! its package's directory, where `cargo test` runs it too: only the target's
//! `#[ignore]` tests run, which are the ones that need root. `-n` makes sudo fail
//! instead of prompting, so a runner without passwordless sudo fails the step rather
//! than skipping the tests.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::Error;

/// The test binary cargo built, and the directory `cargo test` would run it in: its
/// package's, the directory of its `Cargo.toml`.
#[derive(Debug, PartialEq, Eq)]
pub struct Built {
    pub executable: PathBuf,
    pub dir: PathBuf,
}

/// The binary of the integration test target `test` in `messages`, cargo's
/// `--message-format=json` output: the one `compiler-artifact` whose target is of
/// kind `test`, is named `test`, and was built with the test profile.
///
/// # Errors
/// A line is not JSON, or there is not exactly one such artifact, or it names no
/// executable or manifest.
pub fn find(messages: &str, test: &str) -> Result<Built, Error> {
    let mut found = Vec::new();
    for line in messages.lines() {
        let m: Value = serde_json::from_str(line).map_err(|e| {
            Error::Failed(format!(
                "cargo printed a line that is not JSON ({e}): {line}"
            ))
        })?;
        // `&`, not `&&`: every message is judged on all four, so each comparison is
        // tested both ways.
        let wanted = (m["reason"] == "compiler-artifact")
            & (m["target"]["kind"] == serde_json::json!(["test"]))
            & (m["target"]["name"] == test)
            & (m["profile"]["test"] == true);
        if wanted {
            found.push(m);
        }
    }
    let [one] = found.as_slice() else {
        return Err(Error::Failed(format!(
            "cargo built {} test binaries for the target `{test}`, not 1",
            found.len()
        )));
    };
    let executable = one["executable"].as_str().map(PathBuf::from);
    let dir = one["manifest_path"]
        .as_str()
        .and_then(|p| Path::new(p).parent())
        .map(Path::to_path_buf);
    match (executable, dir) {
        (Some(executable), Some(dir)) => Ok(Built { executable, dir }),
        _ => Err(Error::Failed(format!(
            "cargo's artifact for the target `{test}` names no executable or no manifest: {one}"
        ))),
    }
}

/// What to build and how to run it as root.
pub struct SudoTest<'a> {
    /// The cargo program (on `cargo xtask`, the `CARGO` cargo sets).
    pub cargo: &'a OsStr,
    /// The sudo program.
    pub sudo: &'a Path,
    /// The repository root, where cargo runs.
    pub root: &'a Path,
    pub package: &'a str,
    pub test: &'a str,
    /// Arguments for the test binary, after `--ignored`.
    pub args: &'a [String],
}

/// Builds the test target as this user, then runs its ignored tests as root: the line
/// to print when they passed. The build's progress and the tests' output go to this
/// process's stderr and stdout.
///
/// # Errors
/// Cargo or sudo could not be started, the build failed, cargo's messages name no
/// single test binary ([`find`]), or the tests failed.
pub fn run(t: &SudoTest<'_>) -> Result<Vec<String>, Error> {
    let what = format!("-p {} --test {}", t.package, t.test);
    let out = Command::new(t.cargo)
        .current_dir(t.root)
        .args(["test", "--locked", "--no-run", "--message-format=json"])
        .args(["-p", t.package, "--test", t.test])
        .stderr(Stdio::inherit())
        .output()
        .map_err(Error::spawn("cargo".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "cargo test --no-run {what} failed: {}",
            out.status
        )));
    }
    let built = find(&String::from_utf8_lossy(&out.stdout), t.test)?;
    let status = Command::new(t.sudo)
        .current_dir(&built.dir)
        .arg("-n")
        .arg(&built.executable)
        .arg("--ignored")
        .args(t.args)
        .status()
        .map_err(Error::spawn(t.sudo.display().to_string()))?;
    if !status.success() {
        return Err(Error::Failed(format!(
            "the ignored tests of {what} failed as root ({}): {status}",
            built.executable.display()
        )));
    }
    Ok(vec![format!(
        "ok: the ignored tests of {what} passed as root"
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What cargo prints for `-p kbf-guest --test as_root --no-run`, cut down: a
    /// dependency's library, the package's own library and bin (built for the test,
    /// but not test-profile), a test-profile artifact of another target, a
    /// non-test-kind artifact named like the target, a compiler message on the target,
    /// the target itself, and the closing message.
    const MESSAGES: &str = r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"libc"},"profile":{"test":false},"executable":null,"manifest_path":"/r/libc/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"kbf_guest"},"profile":{"test":false},"executable":null,"manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"kbf-guest"},"profile":{"test":false},"executable":"/w/target/debug/kbf-guest","manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["test"],"name":"binary"},"profile":{"test":true},"executable":"/w/target/debug/deps/binary-1","manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"as_root"},"profile":{"test":true},"executable":"/w/target/debug/deps/as_root-bin","manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"compiler-message","target":{"kind":["test"],"name":"as_root"},"profile":{"test":true},"manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["test"],"name":"as_root"},"profile":{"test":false},"executable":"/w/target/debug/deps/as_root-notest","manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"compiler-artifact","target":{"kind":["test"],"name":"as_root"},"profile":{"test":true},"executable":"/w/target/debug/deps/as_root-0f1e","manifest_path":"/w/crates/kbf-guest/Cargo.toml"}
{"reason":"build-finished","success":true}
"#;

    /// Catches the wrong artifact taken: any of the decoys above (another target,
    /// another kind, a message rather than an artifact, a build without the test
    /// profile) instead of the target's test binary, or the binary run from somewhere
    /// other than its package's directory.
    #[test]
    fn the_target_s_test_binary_is_found_among_the_decoys() {
        assert_eq!(
            find(MESSAGES, "as_root"),
            Ok(Built {
                executable: "/w/target/debug/deps/as_root-0f1e".into(),
                dir: "/w/crates/kbf-guest".into(),
            })
        );
    }

    /// Catches a guess where there is no single answer: no artifact for the target
    /// (a misspelt `--test`), or two (the first one taken).
    #[test]
    fn none_or_two_test_binaries_are_refused() {
        let none = find(MESSAGES, "as_rot").expect_err("no target as_rot");
        assert_eq!(
            none.to_string(),
            "cargo built 0 test binaries for the target `as_rot`, not 1"
        );
        let last = MESSAGES.lines().nth(7).expect("the target's line");
        let twice = format!("{MESSAGES}{last}\n");
        let two = find(&twice, "as_root").expect_err("two binaries");
        assert_eq!(
            two.to_string(),
            "cargo built 2 test binaries for the target `as_root`, not 1"
        );
    }

    /// Catches cargo output that is not its JSON messages (a `--message-format` that
    /// did not reach cargo) passed over as "no binary".
    #[test]
    fn a_line_that_is_not_json_is_refused() {
        let e = find("   Compiling kbf-guest v0.1.0\n", "as_root").expect_err("not JSON");
        let text = e.to_string();
        assert!(
            text.starts_with("cargo printed a line that is not JSON ("),
            "{text}"
        );
        assert!(text.ends_with("):    Compiling kbf-guest v0.1.0"), "{text}");
    }

    /// Catches an artifact with no executable, or no manifest to take the directory
    /// from, run anyway (as an empty path, or from the wrong directory).
    #[test]
    fn an_artifact_without_executable_or_manifest_is_refused() {
        let no_exe = r#"{"reason":"compiler-artifact","target":{"kind":["test"],"name":"t"},"profile":{"test":true},"executable":null,"manifest_path":"/w/Cargo.toml"}"#;
        let no_manifest = r#"{"reason":"compiler-artifact","target":{"kind":["test"],"name":"t"},"profile":{"test":true},"executable":"/w/t"}"#;
        for line in [no_exe, no_manifest] {
            let e = find(line, "t").expect_err(line);
            assert!(
                e.to_string()
                    .starts_with("cargo's artifact for the target `t` names no executable"),
                "{e}"
            );
        }
    }
}
