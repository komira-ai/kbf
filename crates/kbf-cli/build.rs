//! Embeds what `kbf --version --json` says of this build (`src/version.rs`): the Cargo
//! profile as `KBF_BUILD_PROFILE` (`debug` or `release`, as Cargo's `PROFILE` says), so it
//! can say whether a binary is a release build, and the number of commits behind `HEAD` as
//! `KBF_BUILD_SEQ`, so builds can be put in order (`src/build_seq.rs` has the rule, and says
//! when the number is 0, "not known"). Cargo runs this again when `HEAD` moves, the branch it
//! names gets a new commit, or a shallow clone is deepened.

use std::path::{Path, PathBuf};

#[path = "src/build_seq.rs"]
mod build_seq;

/// A path inside the git directory, as git resolves it (worktrees included).
fn git_path(name: &str) -> Option<PathBuf> {
    build_seq::git(Path::new("."), &["rev-parse", "--git-path", name]).map(PathBuf::from)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build_seq.rs");
    let profile = std::env::var("PROFILE").expect("Cargo sets PROFILE for a build script");
    println!("cargo:rustc-env=KBF_BUILD_PROFILE={profile}");
    let seq = build_seq::from_git(Path::new("."));
    println!("cargo:rustc-env=KBF_BUILD_SEQ={seq}");
    // Watched: HEAD (a checkout), the ref it names (a commit), packed-refs (where that ref
    // lives once packed) and shallow (the file a clone has while it holds part of the history).
    let branch = build_seq::git(Path::new("."), &["symbolic-ref", "-q", "HEAD"]);
    let watched = [
        Some("HEAD"),
        branch.as_deref(),
        Some("packed-refs"),
        Some("shallow"),
    ];
    for path in watched.into_iter().flatten().filter_map(git_path) {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
