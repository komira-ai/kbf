use kbf_testkit::dir::TestDir;

use super::*;

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-merge-profiles-unit"), name)
}

fn put(dir: &TestDir, name: &str, bytes: &[u8]) {
    std::fs::write(dir.to_path_buf().join(name), bytes).expect("write");
}

fn args(dir: &TestDir, program: &str) -> Args {
    Args {
        dir: dir.to_path_buf(),
        extension: "profraw".to_owned(),
        prefix: "kbf-js-1-".to_owned(),
        out: Some(dir.to_path_buf().join("merged.profdata")),
        workspace_profile_in: None,
        llvm_profdata: Some(PathBuf::from(program)),
    }
}

/// Catches: another shard's profiles, other extensions, and subdirectories merged into a
/// shard's profile (its counts would then be counted twice in the report).
#[test]
fn only_the_prefix_and_extension_named_files_are_inputs() {
    let d = scratch("inputs");
    put(&d, "kbf-js-1-1-0.profraw", b"abc");
    put(&d, "kbf-js-1-2-0.profraw", b"de");
    put(&d, "kbf-legacy-1-1-0.profraw", b"other shard");
    put(&d, "kbf-js-1-merged.profdata", b"merged");
    std::fs::create_dir(d.to_path_buf().join("kbf-js-1-dir.profraw")).expect("dir");
    let got = inputs(&d, "profraw", "kbf-js-1-").expect("inputs");
    let names: Vec<_> = got
        .iter()
        .map(|(p, n)| (p.file_name().unwrap().to_string_lossy().into_owned(), *n))
        .collect();
    assert_eq!(
        names,
        vec![
            ("kbf-js-1-1-0.profraw".to_owned(), 3),
            ("kbf-js-1-2-0.profraw".to_owned(), 2)
        ]
    );
}

/// Catches: a shard with no profile merging nothing and passing.
#[test]
fn no_input_is_refused() {
    let d = scratch("none");
    put(&d, "kbf-legacy-1-1-0.profraw", b"abc");
    let Err(Error::Refused(p)) = run(&args(&d, "true"), &d) else {
        panic!("not refused");
    };
    assert!(p[0].contains("nothing to merge"), "{p:?}");
}

/// Catches: a failing `llvm-profdata` (a corrupt profile) passing, and a tool that exits 0
/// without writing the profile passing.
#[test]
fn a_failed_merge_and_a_missing_output_are_errors() {
    let d = scratch("fails");
    put(&d, "kbf-js-1-1-0.profraw", b"abc");
    let e = run(&args(&d, "false"), &d).expect_err("merge failed");
    assert!(e.to_string().contains("failed"), "{e}");
    let e = run(&args(&d, "true"), &d).expect_err("no output");
    assert!(e.to_string().starts_with("read "), "{e}");
    assert!(
        !d.to_path_buf().join("merged.profdata.inputs").exists(),
        "the list of inputs is removed"
    );
}

/// Catches: a run that names no output passing, and a root with no directory name (`/`) naming
/// the profile `.profdata`.
#[test]
fn an_output_that_cannot_be_named_is_an_error() {
    let d = scratch("unnamed");
    let mut a = args(&d, "true");
    a.out = None;
    let e = out_path(&a, &d).expect_err("no output named");
    assert!(e.to_string().contains("needs --out"), "{e}");
    a.workspace_profile_in = Some(PathBuf::from("p"));
    let e = out_path(&a, Path::new("/")).expect_err("no directory name");
    assert!(e.to_string().contains("no directory name"), "{e}");
    let e = out_path(&a, &d.to_path_buf().join("missing")).expect_err("no such root");
    assert!(e.to_string().starts_with("resolve "), "{e}");
}

/// Catches: the toolchain's tool not being found where `rustc` says its sysroot is.
#[test]
fn the_toolchain_names_its_llvm_profdata_beside_its_host() {
    let p = toolchain_llvm_profdata().expect("rustc runs");
    assert!(p.ends_with("bin/llvm-profdata"), "{p:?}");
    assert!(p.to_string_lossy().contains("lib/rustlib/"), "{p:?}");
}

/// Catches: the merged profile written under a name `cargo llvm-cov report` does not look for
/// (the report would then fail for want of profiles, or read another file), and an `--out` that
/// is not honoured.
#[test]
fn the_workspace_profile_is_named_for_the_workspace_directory() {
    let d = scratch("named");
    let ws = d.to_path_buf().join("some-checkout");
    std::fs::create_dir(&ws).expect("workspace");
    let mut a = args(&d, "true");
    a.out = None;
    a.workspace_profile_in = Some(PathBuf::from("target/llvm-cov-target"));
    assert_eq!(
        out_path(&a, &ws).expect("path"),
        PathBuf::from("target/llvm-cov-target/some-checkout.profdata")
    );
    let b = args(&d, "true");
    assert_eq!(
        out_path(&b, &ws).expect("path"),
        d.to_path_buf().join("merged.profdata")
    );
}
