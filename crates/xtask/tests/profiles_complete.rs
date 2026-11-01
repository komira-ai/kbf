//! `cargo xtask profiles-complete` as the `test` job of `ci.yml` calls it: the real binary on
//! a directory of raw profiles, as the shards' artifacts leave it.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-profiles-complete"),
        name,
    )
}

fn run(dir: &TestDir, prefixes: &[&str]) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(XTASK);
    cmd.args(["profiles-complete", "--dir"])
        .arg(dir.to_path_buf());
    for p in prefixes {
        cmd.args(["--prefix", p]);
    }
    let out = cmd.output().expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Catches: the verb not wired to the binary, a whole directory refused, and a shard whose
/// profiles are missing passing (exit 1 naming the shard's prefix).
#[test]
fn a_directory_with_every_shard_passes_and_one_without_a_shard_fails() {
    let d = scratch("both");
    let root = d.to_path_buf();
    std::fs::write(root.join("kbf-legacy-1-9-0.profraw"), b"abc").expect("write");
    std::fs::write(root.join("kbf-js-1-9-0.profraw"), b"de").expect("write");
    let (code, out, err) = run(&d, &["kbf-legacy-1-", "kbf-js-1-"]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "profiles-complete: `kbf-legacy-1-` 1 file(s), 3 byte(s)\nprofiles-complete: `kbf-js-1-` 1 file(s), 2 byte(s)\n"
    );

    let (code, _, err) = run(&d, &["kbf-legacy-1-", "kbf-legacy-2-", "kbf-js-1-"]);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("begins `kbf-legacy-2-`: that shard's profiles did not arrive"),
        "{err}"
    );
}

/// Catches: the verb run with no `--prefix`, which would check nothing and pass.
#[test]
fn it_needs_a_prefix() {
    let d = scratch("noprefix");
    let (code, _, err) = run(&d, &[]);
    assert_eq!(code, Some(2));
    assert!(err.contains("--prefix"), "{err}");
}
