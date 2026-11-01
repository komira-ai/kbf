//! `cargo xtask merge-profiles` as `ci.yml` calls it: the real binary, with a fake
//! `llvm-profdata` (`xtask-fake` run under that name) that joins its inputs.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-merge-profiles"),
        name,
    )
}

/// The verb on `dir` with the fake tool, plus `more` flags: exit code, stdout, stderr.
fn run(dir: &TestDir, more: &[&str]) -> (Option<i32>, String, String) {
    let root = dir.to_path_buf();
    let tool = root.join("llvm-profdata");
    if !tool.exists() {
        std::os::unix::fs::symlink(FAKE, &tool).expect("link the fake as llvm-profdata");
    }
    let out = Command::new(XTASK)
        .args(["merge-profiles", "--dir"])
        .arg(root.join("in"))
        .arg("--llvm-profdata")
        .arg(&tool)
        .args(more)
        .output()
        .expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn put(dir: &TestDir, name: &str, bytes: &[u8]) {
    let inputs = dir.to_path_buf().join("in");
    std::fs::create_dir_all(&inputs).expect("in dir");
    std::fs::write(inputs.join(name), bytes).expect("write");
}

/// Catches: the verb not wired to the binary, a shard's merge taking another shard's files, the
/// merged profile not being the join of the shard's own, and the summary line missing.
#[test]
fn a_shard_merges_its_own_profiles_into_one_file() {
    let d = scratch("shard");
    put(&d, "kbf-js-1-1-0.profraw", b"ab");
    put(&d, "kbf-js-1-2-0.profraw", b"cd");
    put(&d, "kbf-legacy-1-1-0.profraw", b"zz");
    let out = d.to_path_buf().join("kbf-js-1-merged.profdata");
    let (code, stdout, stderr) = run(
        &d,
        &[
            "--extension",
            "profraw",
            "--prefix",
            "kbf-js-1-",
            "--out",
            out.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(std::fs::read(&out).expect("merged"), b"abcd");
    assert!(stdout.contains("2 `.profraw` file(s)"), "{stdout}");
    assert!(
        !d.to_path_buf()
            .join("kbf-js-1-merged.profdata.inputs")
            .exists(),
        "the list of inputs is removed"
    );
}

/// Catches: the gate's merge writing a file other than the workspace profile `llvm-cov report`
/// reads (here the name of the root given), and a corrupt profile being skipped: the merge must
/// fail, naming the file.
#[test]
fn the_gate_merges_into_the_workspace_profile_and_a_corrupt_input_fails() {
    let d = scratch("gate");
    put(&d, "kbf-js-1-merged.profdata", b"ab");
    put(&d, "kbf-legacy-1-merged.profdata", b"cd");
    let target = d.to_path_buf().join("target");
    std::fs::create_dir_all(&target).expect("target");
    let root = d.to_path_buf().join("in").join("some-root");
    std::fs::create_dir_all(&root).expect("root");
    let flags = [
        "--extension",
        "profdata",
        "--workspace-profile-in",
        target.to_str().expect("utf-8"),
        "--root",
        root.to_str().expect("utf-8"),
    ];
    let (code, _, stderr) = run(&d, &flags);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        std::fs::read(target.join("some-root.profdata")).expect("workspace profile"),
        b"abcd"
    );
    put(&d, "kbf-js-2-merged.profdata", b"corrupt");
    let (code, _, stderr) = run(&d, &flags);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("failed"), "{stderr}");
}

/// Catches: a tool that writes an empty profile passing, and an empty directory passing.
#[test]
fn an_empty_profile_and_no_input_fail() {
    let d = scratch("empty");
    put(&d, "kbf-js-1-1-0.profraw", b"");
    let out = d.to_path_buf().join("o.profdata");
    let flags = [
        "--extension",
        "profraw",
        "--out",
        out.to_str().expect("utf-8"),
    ];
    let (code, _, stderr) = run(&d, &flags);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("is empty"), "{stderr}");
    let (code, _, stderr) = run(
        &d,
        &[
            "--extension",
            "profdata",
            "--out",
            out.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("nothing to merge"), "{stderr}");
}

/// Catches: the default tool (the running toolchain's) never being looked for: with no
/// `--llvm-profdata` the verb must still try to merge, and fail on a file that is no profile.
#[test]
fn without_a_tool_flag_the_toolchains_llvm_profdata_is_run() {
    let d = scratch("toolchain");
    put(&d, "kbf-js-1-1-0.profraw", b"not a profile");
    let out = d.to_path_buf().join("o.profdata");
    let o = Command::new(XTASK)
        .args(["merge-profiles", "--dir"])
        .arg(d.to_path_buf().join("in"))
        .args(["--extension", "profraw", "--out"])
        .arg(&out)
        .output()
        .expect("run xtask");
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
