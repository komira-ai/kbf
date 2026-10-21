//! Which commit count `build.rs` embeds as `KBF_BUILD_SEQ`, the `seq` of `kbf --version
//! --json` and [`crate::BUILD_SEQ`]. The build script includes this file by path, and the
//! library compiles it too (`kbf-cli` tests the live checkout against [`choose`]), so the
//! rule the build applies is the rule the tests check.
//!
//! `seq` is the number of commits behind `HEAD` (`git rev-list --count HEAD`). It grows with
//! every commit on a branch, so two builds of one branch can be put in order by it. It is right
//! only in a checkout that holds the whole history: a shallow clone counts the commits it has
//! (one, with `fetch-depth: 1`), a plausible number and the wrong one. So a shallow clone, a
//! directory that is no repository, a repository with no commit and a git that cannot say are
//! all [`UNKNOWN`], 0, and nothing is newer than 0: a build that could not count is never put
//! ahead of another by its `seq`.

use std::path::Path;
use std::process::Command;

/// The `seq` of a build that could not count its commits.
pub const UNKNOWN: u64 = 0;

/// The count to embed. `shallow` is what `git rev-parse --is-shallow-repository` printed
/// (`None` when it failed, as it does in a git older than 2.15) and `count` what `git rev-list
/// --count HEAD` printed (`None` when it failed: not a repository, or no commit yet).
pub fn choose(shallow: Option<&str>, count: Option<&str>) -> u64 {
    match (shallow, count) {
        (Some("false"), Some(count)) => count.parse().unwrap_or(UNKNOWN),
        _ => UNKNOWN,
    }
}

/// `git -C dir args`: its trimmed stdout, if it ran and succeeded.
pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
}

/// The `seq` of the checkout at `dir`.
pub fn from_git(dir: &Path) -> u64 {
    let shallow = git(dir, &["rev-parse", "--is-shallow-repository"]);
    let count = git(dir, &["rev-list", "--count", "HEAD"]);
    choose(shallow.as_deref(), count.as_deref())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use kbf_testkit::dir::TestDir;

    use super::{UNKNOWN, choose, from_git};

    /// `git args` in `dir`, run as a person with no git configuration of their own would, and
    /// with no repository chosen by the caller's environment (a hook exports `GIT_DIR` and
    /// `GIT_INDEX_FILE`, and a commit made under them would land in the caller's repository).
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "git {args:?}: {stderr}");
    }

    /// A repository of `commits` empty commits, in a directory of its own.
    fn repo(name: &str, commits: usize) -> TestDir {
        let dir = TestDir::under(std::env::temp_dir(), name);
        git(&dir, &["init", "-q"]);
        for n in 0..commits {
            let message = format!("commit {n}");
            git(&dir, &["commit", "-q", "--allow-empty", "-m", &message]);
        }
        dir
    }

    /// Catches: the count of a shallow clone used (a plausible number and the wrong one), the
    /// count used when git cannot say whether the clone is shallow (a git before 2.15), and a
    /// count that is not a number taken for one.
    #[test]
    fn only_a_complete_history_with_a_count_gives_one() {
        assert_eq!(choose(Some("false"), Some("812")), 812);
        for (shallow, count) in [
            (Some("true"), Some("812")),
            (None, Some("812")),
            (Some("maybe"), Some("812")),
            (Some("false"), None),
            (Some("false"), Some("")),
            (Some("false"), Some("many")),
            (Some("false"), Some("-3")),
        ] {
            assert_eq!(choose(shallow, count), UNKNOWN, "{shallow:?} {count:?}");
        }
    }

    /// Catches: a count that is off by one, taken from the wrong directory, or of every branch
    /// instead of the commits behind `HEAD`.
    #[test]
    fn a_repository_counts_the_commits_behind_head() {
        let three = repo("build-seq-three", 3);
        assert_eq!(from_git(&three), 3);
        let one = repo("build-seq-one", 1);
        assert_eq!(from_git(&one), 1);
        // A commit on another branch is not behind HEAD.
        git(&three, &["checkout", "-q", "-b", "side"]);
        git(&three, &["commit", "-q", "--allow-empty", "-m", "side"]);
        git(&three, &["checkout", "-q", "-"]);
        assert_eq!(from_git(&three), 3);
    }

    /// Catches: a shallow clone counted. The clone holds one of the three commits, so a count
    /// that ignores `--is-shallow-repository` answers 1 where the rule answers `UNKNOWN`; the
    /// repository it was cut from still answers 3.
    #[test]
    fn a_shallow_clone_does_not_know_its_count() {
        let full = repo("build-seq-full", 3);
        let at = TestDir::under(std::env::temp_dir(), "build-seq-shallow");
        let url = format!("file://{}", full.display());
        git(&at, &["clone", "-q", "--depth", "1", url.as_str(), "cut"]);
        assert_eq!(from_git(&at.join("cut")), UNKNOWN);
        assert_eq!(from_git(&full), 3);
    }

    /// Catches: a directory with no repository, and a repository with no commit, counted as
    /// something. The first half assumes the system temp directory is not inside a git work
    /// tree, as it is on every runner CI uses; `from_git` runs the production `git`, whose
    /// environment a test cannot set.
    #[test]
    fn no_history_is_not_known() {
        let none = TestDir::under(std::env::temp_dir(), "build-seq-none");
        assert_eq!(from_git(&none), UNKNOWN);
        let empty = repo("build-seq-empty", 0);
        assert_eq!(from_git(&empty), UNKNOWN);
    }
}
