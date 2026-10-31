//! Embeds the commit the binary is built from as `TIERSTORE_BUILD_COMMIT`, which `--version`, the
//! start line and `tierstore_build_info` name (`src/server/build_info.rs`).
//!
//! The commit is `TIERSTORE_BUILD_COMMIT_OVERRIDE` when set (12 lowercase hex digits, for a build
//! without a `.git` that knows its commit; anything else, an empty value included, fails the
//! build), else `git rev-parse --short=12 HEAD` in the checkout being built, or `unknown` where
//! that fails (a source archive, no `git`): a build never fails for want of a commit. The rule
//! is in `src/build_commit.rs`. Uncommitted changes are not marked. Cargo runs this again when
//! the override changes, `HEAD` moves or the branch it names gets a new commit.

use std::path::PathBuf;
use std::process::Command;

#[path = "src/build_commit.rs"]
mod build_commit;

/// `git` with `args` in this crate's directory: its trimmed stdout, if it succeeded.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    out.status.success().then(|| text.trim().to_owned())
}

/// A path inside the git directory, as git resolves it (worktrees included).
fn git_path(name: &str) -> Option<PathBuf> {
    git(&["rev-parse", "--git-path", name]).map(PathBuf::from)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={}", build_commit::OVERRIDE);
    let given = std::env::var_os(build_commit::OVERRIDE);
    let head = git(&["rev-parse", "--short=12", "HEAD"]);
    let commit = match build_commit::choose(given.as_deref(), head) {
        Ok(commit) => commit,
        Err(e) => panic!("{e}"),
    };
    println!("cargo:rustc-env=TIERSTORE_BUILD_COMMIT={commit}");
    // Watched: HEAD (a checkout), the ref it names (a commit), and packed-refs (where that ref
    // lives once packed).
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    let watched = [Some("HEAD"), branch.as_deref(), Some("packed-refs")];
    for path in watched.into_iter().flatten().filter_map(git_path) {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
