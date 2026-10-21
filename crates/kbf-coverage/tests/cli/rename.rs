//! `no-lowering --base-ref`: a crate git sees renamed keeps its floor, on a real
//! throwaway git repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A repository removed when dropped.
struct Repo(PathBuf);

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository whose first commit holds the crate `old` (package `old`) and the floors
/// `base_floors`, and whose second moves it to `crates/new` (package `new`) with
/// `head_floors`. `base` is the first commit's floors file, beside the repository.
fn renamed(name: &str, base_floors: &str, head_floors: &str) -> Repo {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("crates/old")).expect("mkdir");
    git(&dir, &["init", "-q"]);
    let manifest = "[package]\nname = \"old\"\nversion = \"1\"\n\n[dependencies]\nx = \"1\"\n";
    std::fs::write(dir.join("crates/old/Cargo.toml"), manifest).expect("manifest");
    std::fs::write(dir.join("coverage-floors"), base_floors).expect("floors");
    std::fs::write(dir.join("base"), base_floors).expect("base");
    git(&dir, &["add", "crates", "coverage-floors"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["mv", "crates/old", "crates/new"]);
    let moved = manifest.replace("name = \"old\"", "name = \"new\"");
    std::fs::write(dir.join("crates/new/Cargo.toml"), moved).expect("manifest");
    std::fs::write(dir.join("coverage-floors"), head_floors).expect("floors");
    git(&dir, &["add", "crates", "coverage-floors"]);
    git(&dir, &["commit", "-q", "-m", "rename"]);
    Repo(dir)
}

fn no_lowering(repo: &Repo, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .current_dir(&repo.0)
        .args([
            "no-lowering",
            "--base",
            "base",
            "--floors",
            "coverage-floors",
        ])
        .args(extra)
        .output()
        .expect("run kbf-coverage")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Catches (kbf#1401): a pure crate rename refused as a floor added; the rename
/// excusing a lowered floor; `--base-ref` ignored (the control run without it must
/// still refuse); a git failure passed or reported as a gate failure.
#[test]
fn a_renamed_crate_keeps_its_floor_and_a_lowered_one_is_refused() {
    let repo = renamed("rename-pure", "old - 97.43\n", "new - 97.43\n");
    let o = no_lowering(&repo, &[]);
    assert_eq!(o.status.code(), Some(1), "without --base-ref it is new");
    assert!(
        text(&o.stderr).contains("new branches: threshold 99.00 -> 97.43"),
        "{}",
        text(&o.stderr)
    );
    let o = no_lowering(&repo, &["--base-ref", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("lowers no threshold"));

    let repo = renamed("rename-lowers", "old - 97.43\n", "new - 90.00\n");
    let o = no_lowering(&repo, &["--base-ref", "HEAD~1"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("new branches: threshold 97.43 -> 90.00 (a floor may only rise)"),
        "{}",
        text(&o.stderr)
    );

    let repo = renamed("rename-unknown", "old - 97.43\n", "new - 97.43\n");
    let o = no_lowering(&repo, &["--base-ref", "no-such-rev"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).contains("--base-ref no-such-rev: git diff --find-renames"),
        "{}",
        text(&o.stderr)
    );
}
