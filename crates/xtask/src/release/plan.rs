//! `release plan`: what the push of one commit would release, and whether it may. It
//! writes nothing but its `--output`.
//!
//! Checks, in this order; the first that fails refuses:
//!
//! 1. the freeze: `--freeze` is not empty (on Actions, the `RELEASE_FREEZE` repository
//!    variable): an admin stops every later release without a code change;
//! 2. `VERSION` at `--sha` is `MAJOR.MINOR` and one line end ([`Base::parse_file`]);
//! 3. the base never goes backwards: no release tag has a higher base;
//! 4. Rule S ([`superseded`]): the highest release tag names `--sha` or an ancestor of it.
//!
//! Then the version: one above the highest `v<MAJOR>.<MINOR>.N` of the `VERSION` base
//! among the tags and the releases (drafts too), or `1`; or, on a re-run of a commit the
//! highest tag already names, that tag's version again (`reused=true`). Then the push is
//! classified from the files `--before` to `--sha` changed:
//!
//! - `code`: some file ships in a release. Not code: a root `*.md`, `docs/**`,
//!   `.github/**`, `site/**`, `infra/**` (a `.md` under `crates/` is code: it may be
//!   `include_str!`ed). `VERSION` is code (a bump releases). A mixed push is code.
//! - `site`: some file is under `site/` or `docs/`, which kbf.dev renders.
//! - `pipeline`: some file is the release machinery or what it trusts (`.github/`,
//!   `infra/`, `crates/xtask/`, `crates/kbf-gate/`, `.config/`, `.cargo/`,
//!   `rust-toolchain*`, `deny.toml`, `Cargo.lock`): the pipeline does not release a
//!   commit that changes it; the next push that does not releases both. Classes are not
//!   exclusive: such a file under `crates/` is code too.
//!
//! A `--before` of all zeros (a branch's first push) names no earlier commit, and one
//! this checkout does not hold (a forced push) cannot be diffed: both read as `code=true`
//! and `site=true`, the checks that are safe to repeat. A first push is no change to the
//! pipeline (`pipeline=false`); a before that is lost might hide one, so it reads
//! `pipeline=true`. A push that changed no file is not code, and neither is it a site.
//!
//! Prints `version`, `tag`, `reused`, `code`, `site` and `pipeline` as `key=value` lines,
//! and appends them to `--output` (on Actions, `GITHUB_OUTPUT`).

use std::path::{Path, PathBuf};

use super::append_output;
use super::git::{git, git_output};
use super::tags::{self, Base, Standing, Version};
use crate::Error;
use crate::build_commit::{full_commit, is_commit};

/// The arguments of `release plan`.
#[derive(clap::Args)]
pub struct PlanArgs {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The commit pushed, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub sha: String,
    /// The commit the branch was at before the push (`github.event.before`): a SHA, or
    /// all zeros for the first push.
    #[arg(long)]
    pub before: String,
    /// A checkout holding both commits (`fetch-depth: 0`).
    #[arg(long, default_value = ".")]
    pub repo_dir: PathBuf,
    /// The freeze (on Actions, `vars.RELEASE_FREEZE`): when it is not empty, nothing is
    /// planned.
    #[arg(long, default_value = "")]
    pub freeze: String,
    /// Append the lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// What a push changed, by what it is to release.
#[derive(Debug, PartialEq, Eq)]
pub struct Class {
    pub code: bool,
    pub site: bool,
    pub pipeline: bool,
}

/// A path that ships in no release. A markdown file under `crates/` may be
/// `include_str!`ed, so only a root `*.md` and the directories below ship nothing.
fn ships_nothing(path: &str) -> bool {
    (path.ends_with(".md") && !path.contains('/'))
        || ["docs/", ".github/", "site/", "infra/"]
            .iter()
            .any(|dir| path.starts_with(dir))
}

/// The release machinery and what it trusts: the workflows and every other file under
/// `.github/` and `infra/`, all of `crates/xtask/` and `crates/kbf-gate/` (the record
/// and the notes use them), and the toolchain, dependency and policy files. Fail closed:
/// a push that touches one never releases itself.
fn is_pipeline(path: &str) -> bool {
    [
        ".github/",
        "infra/",
        "crates/xtask/",
        "crates/kbf-gate/",
        ".config/",
        ".cargo/",
        "rust-toolchain",
        "deny.toml",
        "Cargo.lock",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

/// The class of a push that changed exactly `paths`.
pub fn classify(paths: &[String]) -> Class {
    Class {
        code: paths.iter().any(|p| !ships_nothing(p)),
        site: paths
            .iter()
            .any(|p| p.starts_with("site/") || p.starts_with("docs/")),
        pipeline: paths.iter().any(|p| is_pipeline(p)),
    }
}

/// The files `before..sha` changed, renames as the two paths they touch, or none when
/// `before` is not a commit this checkout holds.
fn changed(repo_dir: &Path, before: &str, sha: &str) -> Result<Option<Vec<String>>, Error> {
    let probe = format!("{before}^{{commit}}");
    let known = git_output(repo_dir, &["rev-parse", "--verify", "--quiet", &probe])?;
    if !known.status.success() {
        return Ok(None);
    }
    let listed = git(
        repo_dir,
        &["diff", "--name-only", "-z", "--no-renames", before, sha],
    )?;
    Ok(Some(
        listed
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

/// `--before` as a commit to diff from: none for the first push (all zeros).
fn before_of(before: &str) -> Result<Option<&str>, Error> {
    if !before.is_empty() && before.bytes().all(|b| b == b'0') {
        return Ok(None);
    }
    if is_commit(before) {
        return Ok(Some(before));
    }
    Err(Error::Usage(format!(
        "--before {before:?} is not a commit (12 to 40 lowercase hex digits) or all zeros"
    )))
}

/// The `release plan` verb: the lines to print.
pub fn run(a: &PlanArgs) -> Result<Vec<String>, Error> {
    full_commit("--sha", &a.sha)?;
    let before = before_of(&a.before)?;
    if !a.freeze.is_empty() {
        return Err(Error::Refused(vec![format!(
            "frozen: RELEASE_FREEZE is {:?}: nothing is released until an admin clears it",
            a.freeze
        )]));
    }
    let file = git(&a.repo_dir, &["show", &format!("{}:VERSION", a.sha)])?;
    let Some(base) = Base::parse_file(&file) else {
        return Err(Error::Refused(vec![format!(
            "VERSION at {} is {file:?}: expected MAJOR.MINOR and one line end, e.g. \"0.1\\n\"",
            a.sha
        )]));
    };
    let mut taken = tags::tag_versions(&a.gh, &a.repo)?;
    taken.extend(tags::release_versions(&a.gh, &a.repo)?);
    if let Some(above) = taken.iter().map(|v| v.base).filter(|b| *b > base).max() {
        return Err(Error::Refused(vec![format!(
            "VERSION is {}.{} but a release of {}.{} exists: the base never goes backwards",
            base.major, base.minor, above.major, above.minor
        )]));
    }
    let standing = tags::superseded(&a.gh, &a.repo, &a.sha)?;
    let top = taken
        .iter()
        .filter(|v| v.base == base)
        .map(|v| v.patch)
        .max();
    let (version, reused) = match (standing, top) {
        (Standing::Is(v), Some(patch)) if v.base == base && v.patch == patch => (v, true),
        _ => (
            Version {
                base,
                patch: top.map_or(1, |p| p + 1),
            },
            false,
        ),
    };
    let class = match before {
        None => Class {
            code: true,
            site: true,
            pipeline: false,
        },
        Some(before) => match changed(&a.repo_dir, before, &a.sha)? {
            Some(paths) => classify(&paths),
            None => Class {
                code: true,
                site: true,
                pipeline: true,
            },
        },
    };
    let lines = vec![
        format!("version={version}"),
        format!("tag={}", version.tag()),
        format!("reused={reused}"),
        format!("code={}", class.code),
        format!("site={}", class.site),
        format!("pipeline={}", class.pipeline),
    ];
    if let Some(output) = &a.output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}
