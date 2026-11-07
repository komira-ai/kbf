//! `cargo xtask pr-paths` as macos-pr.yml runs it: the real binary in throwaway git
//! repositories, a merge commit diffed against its first parent as a pull request's is.

use kbf_testkit::dir::TestDir;
use std::path::Path;
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// The pattern file every repository here holds.
const PATTERNS: &str = "# the surface\ncrates/mac/**\nci.yml\n";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("pr-paths"),
        name,
    )
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The exit code, stdout and stderr of a finished run.
fn outcome(out: Output) -> (Option<i32>, String, String) {
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// `git <args>` in `root`, with the identity the commits need and no configuration of the host's.
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

/// Writes `body` to `path` under `root`, making its directories.
fn put(root: &Path, path: &str, body: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().expect("a parent")).expect("dirs");
    std::fs::write(file, body).expect("write");
}

/// A repository on branch `main` whose one commit holds the pattern file, a file of the surface
/// (`crates/mac/src/lib.rs`) and one outside it (`docs/a.md`).
fn repo(name: &str) -> TestDir {
    let root = scratch(name);
    git(&root, &["init", "-q", "-b", "main"]);
    put(&root, "tools/paths.txt", PATTERNS);
    put(&root, "crates/mac/src/lib.rs", "mac\n");
    put(&root, "docs/a.md", "doc\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    root
}

/// Commits everything in the working tree as `message`.
fn commit(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

/// `pr-paths --base <base>` in `root` with the repository's pattern file and an output file.
fn pr_paths(root: &Path, base: &str) -> (Option<i32>, String, String) {
    let out = root.join("github-output");
    outcome(
        Command::new(XTASK)
            .arg("--root")
            .arg(root)
            .args(["pr-paths", "--base", base])
            .args([
                "--patterns",
                "tools/paths.txt",
                "--key",
                "macos",
                "--output",
            ])
            .arg(&out)
            .output()
            .expect("run xtask"),
    )
}

/// What `pr-paths` appended to the output file, or `None` when it wrote none.
fn output(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join("github-output")).ok()
}

#[test]
fn a_change_under_the_surface_says_true_and_names_the_path() {
    // Catches: a match that is not written to the output the jobs' `if:` reads, or a report
    // that does not say which path matched which pattern.
    let root = repo("surface-change");
    put(&root, "crates/mac/src/lib.rs", "mac, changed\n");
    put(&root, "docs/a.md", "doc, changed\n");
    commit(&root, "change");
    assert_eq!(
        pr_paths(&root, "HEAD^1"),
        (
            Some(0),
            "macos=true\nchanged: crates/mac/src/lib.rs (matches crates/mac/**)\n".to_owned(),
            String::new()
        )
    );
    assert_eq!(output(&root).as_deref(), Some("macos=true\n"));
}

#[test]
fn a_change_outside_the_surface_says_false() {
    // Catches: `true` for every pull request (a macOS runner for each), or no output line at all
    // (the jobs' `if:` then reads an empty string; false, but by accident).
    let root = repo("outside-change");
    put(&root, "docs/a.md", "doc, changed\n");
    put(
        &root,
        "crates/mac2/src/lib.rs",
        "a sibling with a longer name\n",
    );
    commit(&root, "docs");
    assert_eq!(
        pr_paths(&root, "HEAD^1"),
        (Some(0), "macos=false\n".to_owned(), String::new())
    );
    assert_eq!(output(&root).as_deref(), Some("macos=false\n"));
}

#[test]
fn a_file_moved_out_of_the_surface_or_deleted_from_it_says_true() {
    // Catches: a diff with rename detection, which lists only the new path of a moved file, so
    // moving the Mac code elsewhere would skip the macOS jobs that test it; and a deletion missed.
    let root = repo("moved-out");
    git(&root, &["mv", "crates/mac/src/lib.rs", "docs/lib.rs"]);
    commit(&root, "move");
    let (code, out, err) = pr_paths(&root, "HEAD^1");
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            Some(0),
            "macos=true\nchanged: crates/mac/src/lib.rs (matches crates/mac/**)\n",
            ""
        )
    );
    let root = repo("deleted");
    git(&root, &["rm", "-q", "crates/mac/src/lib.rs"]);
    commit(&root, "delete");
    assert_eq!(
        pr_paths(&root, "HEAD^1").1,
        "macos=true\nchanged: crates/mac/src/lib.rs (matches crates/mac/**)\n"
    );
}

#[test]
fn a_merge_commit_is_diffed_against_its_first_parent_only() {
    // Catches: a diff against the pull request's old base (or the branch point), which would
    // count a change main made after the pull request branched as the pull request's own; and,
    // the other way, a pull request's own change lost in the merge.
    let root = repo("merge");
    git(&root, &["checkout", "-q", "-b", "pr"]);
    put(&root, "docs/a.md", "the pull request's own change\n");
    commit(&root, "pr");
    git(&root, &["checkout", "-q", "main"]);
    put(&root, "crates/mac/src/lib.rs", "main moved on\n");
    commit(&root, "main");
    git(&root, &["merge", "-q", "--no-ff", "-m", "merge", "pr"]);
    assert_eq!(
        pr_paths(&root, "HEAD^1"),
        (Some(0), "macos=false\n".to_owned(), String::new())
    );
    // The other way round: the pull request changes the surface, main does not.
    let root = repo("merge-surface");
    git(&root, &["checkout", "-q", "-b", "pr"]);
    put(&root, "ci.yml", "jobs\n");
    commit(&root, "pr");
    git(&root, &["checkout", "-q", "main"]);
    put(&root, "docs/a.md", "main moved on\n");
    commit(&root, "main");
    git(&root, &["merge", "-q", "--no-ff", "-m", "merge", "pr"]);
    assert_eq!(
        pr_paths(&root, "HEAD^1"),
        (
            Some(0),
            "macos=true\nchanged: ci.yml (matches ci.yml)\n".to_owned(),
            String::new()
        )
    );
}

#[test]
fn the_output_line_is_appended_after_what_the_file_holds() {
    // Catches: an output file truncated, which would drop the outputs earlier steps wrote.
    let root = repo("append");
    put(&root, "crates/mac/src/lib.rs", "changed\n");
    commit(&root, "change");
    put(&root, "github-output", "earlier=1\n");
    assert_eq!(pr_paths(&root, "HEAD^1").0, Some(0));
    assert_eq!(output(&root).as_deref(), Some("earlier=1\nmacos=true\n"));
}

#[test]
fn a_base_git_cannot_read_fails_and_writes_no_output() {
    // Catches: a git error read as "nothing changed", which would skip the macOS jobs of the
    // pull request whose check broke.
    let root = repo("bad-base");
    let (code, out, err) = pr_paths(&root, "no-such-rev");
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(
        err.starts_with("xtask: git diff no-such-rev HEAD: "),
        "stderr: {err}"
    );
    assert_eq!(output(&root), None);
}

#[test]
fn a_pattern_file_that_does_not_parse_or_is_missing_fails_and_writes_no_output() {
    // Catches: a bad pattern skipped (the paths it was meant to name would never run the macOS
    // jobs), and a missing file read as an empty list.
    let root = repo("bad-patterns");
    put(&root, "tools/paths.txt", "crates/mac/**\ncrates/*.rs\n");
    put(&root, "crates/mac/src/lib.rs", "changed\n");
    commit(&root, "change");
    assert_eq!(
        pr_paths(&root, "HEAD^1"),
        (
            Some(1),
            String::new(),
            "xtask: tools/paths.txt: line 2: `crates/*.rs` holds a `*` that is not the final `/**`\n"
                .to_owned()
        )
    );
    assert_eq!(output(&root), None);
    std::fs::remove_file(root.join("tools/paths.txt")).expect("remove");
    let (code, out, err) = pr_paths(&root, "HEAD^1");
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("tools/paths.txt: "), "stderr: {err}");
    assert_eq!(output(&root), None);
}

#[test]
fn without_an_output_file_it_only_prints() {
    // Catches: a verb that needs `--output` (a local run would fail), or writes somewhere else.
    let root = repo("no-output");
    put(&root, "ci.yml", "jobs\n");
    commit(&root, "ci");
    let (code, out, err) = outcome(
        Command::new(XTASK)
            .arg("--root")
            .arg(&*root)
            .args([
                "pr-paths",
                "--base",
                "HEAD^1",
                "--patterns",
                "tools/paths.txt",
                "--key",
                "k",
            ])
            .output()
            .expect("run xtask"),
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (Some(0), "k=true\nchanged: ci.yml (matches ci.yml)\n", "")
    );
    assert_eq!(output(&root), None);
}

#[test]
fn a_base_that_looks_like_an_option_is_read_as_a_revision() {
    // Catches: a `--base` git takes as one of its own options (`--output=<file>` writes a file),
    // where it can only be a revision.
    let root = repo("option-base");
    let (code, out, err) = outcome(
        Command::new(XTASK)
            .arg("--root")
            .arg(&*root)
            .args([
                "pr-paths",
                "--base=--output=written",
                "--patterns",
                "tools/paths.txt",
            ])
            .args(["--key", "macos"])
            .output()
            .expect("run xtask"),
    );
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(
        err.starts_with("xtask: git diff --output=written HEAD: "),
        "stderr: {err}"
    );
    assert!(!root.join("written").exists());
}
