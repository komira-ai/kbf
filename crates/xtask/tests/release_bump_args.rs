//! The request of `cargo xtask release bump-internal`: the flags it accepts, and the branch,
//! title and body it builds. The verb against a recorded world is tested in
//! `release_bump.rs`, the real runner and the binary in `release_bump_runner.rs`.

use std::path::PathBuf;

use xtask::Error;
use xtask::release::bump::{Bump, BumpArgs};

const REPO: &str = "komira-ai/kbf";
const INTERNAL: &str = "example-org/internal";
/// The released commit.
const NEW: &str = "2222222222222222222222222222222222222222";

fn bump_args(version: &str) -> BumpArgs {
    BumpArgs {
        repo: REPO.to_owned(),
        version: version.to_owned(),
        sha: NEW.to_owned(),
        internal_repo: INTERNAL.to_owned(),
        submodule: "kbf".to_owned(),
        workdir: Some(PathBuf::from("/work/clone")),
        gh: PathBuf::from("gh"),
        git: PathBuf::from("git"),
    }
}

fn request() -> Bump {
    Bump::from_args(&bump_args("1.2.3")).expect("a good request")
}

#[test]
fn the_commit_and_the_pull_request_name_the_release() {
    // Catches: a pull request that does not say which release it moves the submodule to.
    let body = request().body();
    let first = format!("Moves the `kbf` submodule to the commit of the release v1.2.3 ({NEW}).");
    let link = "https://github.com/komira-ai/kbf/releases/tag/v1.2.3";
    assert!(body.starts_with(&first), "{body}");
    assert!(body.contains(link), "{body}");
    assert!(body.contains("bump guard"), "{body}");
}

fn args_with(flag: &str, value: &str) -> BumpArgs {
    let mut a = bump_args("1.2.3");
    match flag {
        "--version" => a.version = value.to_owned(),
        "--sha" => a.sha = value.to_owned(),
        "--repo" => a.repo = value.to_owned(),
        "--internal-repo" => a.internal_repo = value.to_owned(),
        "--submodule" => a.submodule = value.to_owned(),
        _ => panic!("no such flag {flag}"),
    }
    a
}

#[test]
fn values_the_guard_would_refuse_are_usage_errors() {
    // Catches: a version, commit, repository or submodule name that is not what the guard
    // and the GitHub paths built from it assume, accepted and sent on.
    let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
    let bad = [
        ("--version", "v1.2.3"),
        ("--version", "1.2"),
        ("--version", "1.2.3.4.5"),
        ("--version", "01.2.3"),
        ("--version", "1.2.3-rc1"),
        ("--version", ""),
        ("--sha", "abc"),
        ("--sha", &NEW[..39]),
        ("--sha", upper),
        ("--repo", "kbf"),
        ("--repo", "a/b/c"),
        ("--repo", "/kbf"),
        ("--repo", "x/"),
        ("--repo", ".x/y"),
        ("--repo", "x/.y"),
        ("--repo", "x y/z"),
        ("--internal-repo", "internal"),
        ("--internal-repo", "a/b/c"),
        ("--submodule", ""),
        ("--submodule", ".kbf"),
        ("--submodule", "a/b"),
        ("--submodule", "a b"),
    ];
    for (flag, value) in bad {
        let got = Bump::from_args(&args_with(flag, value));
        let Err(Error::Usage(why)) = got else {
            panic!("{flag} {value:?} was accepted");
        };
        assert!(why.starts_with(flag), "{flag} {value:?}: {why}");
    }
}

#[test]
fn a_run_with_no_workdir_gets_one_of_its_own() {
    // Catches: a default clone directory shared by two runs, or none.
    let mut a = bump_args("10.20.30");
    a.workdir = None;
    let b = Bump::from_args(&a).expect("a good request");
    let dir = b.workdir.to_string_lossy().into_owned();
    let own = format!("xtask-bump-{}", std::process::id());
    assert!(dir.contains(&own), "{dir}");
    assert_eq!(request().workdir, PathBuf::from("/work/clone"));
}

/// The bump guard's rule for a branch: `bump/<submodule>-v<semver>`, plain numbers.
fn guard_accepts(branch: &str, submodule: &str) -> bool {
    let Some(semver) = branch.strip_prefix(&format!("bump/{submodule}-v")) else {
        return false;
    };
    let parts: Vec<&str> = semver.split('.').collect();
    let plain = |p: &&str| p.parse::<u64>().is_ok_and(|n| n.to_string() == **p);
    parts.len() == 3 && parts.iter().all(plain)
}

#[test]
fn the_branch_and_the_title_are_the_ones_the_bump_guard_accepts() {
    // Catches: a branch or title that drifts from `bump/<submodule>-v<semver>` and
    // `chore(<submodule>): bump to v<semver>`: the guard reads the branch, and the pull
    // requests already merged read the title.
    for version in ["0.0.1", "1.2.3", "10.20.30"] {
        let b = Bump::from_args(&bump_args(version)).expect("a good request");
        assert_eq!(b.branch(), format!("bump/kbf-v{version}"));
        assert_eq!(b.title(), format!("chore(kbf): bump to v{version}"));
        assert_eq!(b.tag(), format!("v{version}"));
        assert!(guard_accepts(&b.branch(), "kbf"), "{}", b.branch());
    }
    assert!(!guard_accepts("bump/kbf-v1.2", "kbf"));
    assert!(!guard_accepts("bump/mopm-v1.2.3", "kbf"));
    assert!(!guard_accepts("bump/kbf-v01.2.3", "kbf"));
}
