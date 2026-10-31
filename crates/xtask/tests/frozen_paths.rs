//! `frozen-paths` (`xtask::frozen_paths`): no change may touch the paths tierstore is moving
//! out with. The decision is tested on change lists; the binary, on throwaway git repositories
//! (a rename is the case only git can produce).

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;
use xtask::frozen_paths::{FROZEN, touched};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

#[test]
fn a_change_list_of_unrelated_paths_passes() {
    // Catches: a check that refuses every change, or that matches on a substring such as
    // `tierstore` anywhere in the path.
    let changes = [
        "README.md",
        "crates/kbf-server/src/lib.rs",
        "docs/tierstore-notes.md",
        ".github/workflows/ci.yml",
        "packaging/install.sh",
    ];
    assert_eq!(touched(changes), Vec::<String>::new());
    assert_eq!(touched([]), Vec::<String>::new());
}

#[test]
fn one_file_under_each_frozen_path_is_refused_and_named() {
    // Catches: a frozen path missing from the list, or one matched in a way that skips a
    // file inside it. Each path is tried alone, so the refusal names exactly that file.
    assert_eq!(
        FROZEN,
        [
            "crates/tierstore",
            "crates/objstore-s3",
            "packaging/tierstore",
            ".github/workflows/tierstore-site.yml",
            ".github/workflows/tierstore-site-publish.yml",
        ]
    );
    for frozen in FROZEN {
        let file = if frozen.ends_with(".yml") {
            (*frozen).to_owned()
        } else {
            format!("{frozen}/src/deep/er/file.rs")
        };
        let refused = touched([file.as_str()]);
        assert_eq!(refused.len(), 1, "{file}: {refused:?}");
        assert!(refused[0].starts_with(&format!("{file}: ")), "{refused:?}");
        assert!(refused[0].contains(frozen), "{refused:?}");
    }
}

#[test]
fn a_path_that_only_shares_a_prefix_passes() {
    // Catches: a match by string prefix, which would freeze `crates/tierstore-bench/` and
    // `crates/objstore-s3x/` (and a workflow named like a frozen one plus a suffix).
    let siblings = [
        "crates/tierstore-bench/src/main.rs",
        "crates/objstore-s3x/src/lib.rs",
        "packaging/tierstore-extra/x",
        "crates/tierstore.rs",
        ".github/workflows/tierstore-site.yml.orig",
        ".github/workflows/tierstore-site-publish.yaml",
        ".github/workflows/tierstore-bench.yml",
    ];
    assert_eq!(touched(siblings), Vec::<String>::new());
}

#[test]
fn every_touched_file_is_named_not_just_the_first() {
    // Catches: a check that stops at the first frozen file.
    let refused = touched([
        "crates/tierstore/a.rs",
        "unrelated.rs",
        "crates/objstore-s3/b.rs",
    ]);
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert!(refused[0].starts_with("crates/tierstore/a.rs: "));
    assert!(refused[1].starts_with("crates/objstore-s3/b.rs: "));
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `git <args>` in `root`, with the identity the commits need.
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
}

/// A repository whose first commit holds a frozen file, `crates/tierstore/lib.rs`, and an
/// unrelated one, `docs/a.md`.
fn repo(name: &str) -> TestDir {
    let root = TestDir::under(Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask"), name);
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("crates/tierstore")).expect("tierstore");
    std::fs::create_dir_all(root.join("docs")).expect("docs");
    let body = "one\ntwo\nthree\nfour\nfive\nsix\n";
    std::fs::write(root.join("crates/tierstore/lib.rs"), body).expect("lib.rs");
    std::fs::write(root.join("docs/a.md"), body).expect("a.md");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    root
}

/// The exit code, stdout and stderr of `frozen-paths --base HEAD~1 --head HEAD` in `root`.
fn frozen_paths(root: &Path) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(root)
        .args(["frozen-paths", "--base", "HEAD~1", "--head", "HEAD"])
        .output()
        .expect("run xtask");
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

#[test]
fn the_verb_passes_an_unrelated_commit_and_refuses_an_edit_of_a_frozen_file() {
    // Catches: a verb that diffs the wrong pair (the working tree, or `--head` ignored) and
    // so reports nothing or everything; one that exits 0 on a frozen change.
    let root = repo("frozen-edit");
    std::fs::write(root.join("docs/a.md"), "changed\n").expect("edit");
    git(&root, &["commit", "-q", "-a", "-m", "unrelated"]);
    let (code, out, err) = frozen_paths(&root);
    assert_eq!((code, err.as_str()), (Some(0), ""), "{out}");

    std::fs::write(root.join("crates/tierstore/lib.rs"), "changed\n").expect("edit");
    git(&root, &["commit", "-q", "-a", "-m", "frozen"]);
    let (code, out, err) = frozen_paths(&root);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("crates/tierstore/lib.rs: "), "{err}");
    assert!(!err.contains("docs/a.md"), "{err}");
}

#[test]
fn a_rename_out_of_a_frozen_path_counts_as_touching_it() {
    // Catches: a check that reads only the new path of a rename, so that moving a file out
    // of the frozen tree (its content unchanged, so git calls it a rename) goes unseen.
    let root = repo("frozen-rename");
    git(&root, &["mv", "crates/tierstore/lib.rs", "docs/lib.rs"]);
    git(&root, &["commit", "-q", "-m", "move out"]);
    let (code, _out, err) = frozen_paths(&root);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("crates/tierstore/lib.rs: "), "{err}");
}

#[test]
fn a_rename_into_a_frozen_path_counts_as_touching_it() {
    // Catches: a check that reads only the old path of a rename.
    let root = repo("frozen-rename-in");
    git(&root, &["mv", "docs/a.md", "crates/tierstore/a.md"]);
    git(&root, &["commit", "-q", "-m", "move in"]);
    let (code, _out, err) = frozen_paths(&root);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("crates/tierstore/a.md: "), "{err}");
}
