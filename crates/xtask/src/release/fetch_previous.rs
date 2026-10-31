//! `release fetch-previous`: the release before a commit, fetched, verified and unpacked, for the
//! tests that need an N-1 (the N/N-1 checks of docs/design/kbf-cicd.md, section 9). It
//! sets nothing on any farm and writes nothing but `--out` and `--output`.
//!
//! `--sha` is the commit N. Its previous release is the highest release tag
//! ([`tag_versions`]: `v` and three plain numbers, so no `-rc1` and no suffix, and 10 is
//! above 9) that names a strict ancestor of N: GitHub's compare of `<tag>...<sha>` says
//! `ahead`. The tags are tried from the highest down. `identical` (the tag of N itself),
//! `behind` and `diverged` are no ancestor and are passed over; any other answer, and any
//! call that fails (a 404 included), is an error, never "no tags".
//!
//! With no such tag the verb prints `previous-source=none` and succeeds: whether a missing
//! N-1 is red is for the caller (section 9: "not run" is not "passed"). There is no
//! fallback to an earlier attested build of a branch: that is not a release, and a release
//! is what is verified here.
//!
//! With one, and only for that tag:
//!
//! 1. the tag is resolved: its ref names a tag object (a lightweight tag is refused: a
//!    release tag carries its audit record in its message) and that names a commit. The scan
//!    compared names; this commit is compared again and must itself be a strict ancestor of
//!    N, so a branch of the tag's name, or a tag made since the scan, cannot stand in;
//! 2. the release of that tag must exist. A tag with no release is an error, not a step back
//!    to a lower tag: N-1 is the release before N, not the newest that happens to be whole.
//!    GitHub answers a release that is still a draft with 404 too, and a release is a draft
//!    for some minutes after its tag is pushed, so the release is asked for up to
//!    `--release-attempts` times, `--release-poll-secs` apart (by default 40 times, 30 s
//!    apart), before its absence is that error;
//! 3. its assets are downloaded into a directory of this process alone;
//! 4. they are verified as a release for that commit is: `release
//!    check` (`SHA256SUMS`, names, one bundle per tarball) and `verify-assets attestations`
//!    (provenance, SBOM and bundle of each asset). The bundles are checked offline when
//!    `--trusted-root` is given; the provenance and SBOM attestations are always asked of
//!    GitHub, so the token needs `contents: read` and `attestations: read` either way;
//! 5. only then are the `--binary` names of `--platform` taken out of their tarballs
//!    (`<binary>-<commit>-<platform>.tar.gz`, exactly the one entry `package` writes) into
//!    `--out`, mode 0755. Every tarball is read before the first file is written, and a
//!    file already in `--out` is refused, not replaced; the files this run wrote before the
//!    refusal are removed again.
//!
//! A refusal in steps 1 to 4, or a binary the release lacks, leaves `--out` as it was (it
//! is made once every requested tarball has been read) and `--output` untouched. On
//! success it prints `previous-tag`, `previous-sha` (the commit the tag names) and
//! `previous-source=release` as `key=value` lines, and appends them to `--output` (on
//! Actions, `GITHUB_OUTPUT`). `--output` is written last: if it cannot be written the run
//! fails with the binaries already in `--out`.
//!
//! This is the previous release tag, not the commit the farm runs: it is no input to
//! `reversible` (a rollback goes to what the farm runs).

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::attest_gh::{Attester, Gh};
use super::tags::tag_versions;
use crate::Error;
use crate::build_commit::{full_commit, write_executable};
use crate::darwin::packaged_binary;
use crate::package::{Platform, read};

/// The arguments of `release fetch-previous`.
#[derive(clap::Args)]
pub struct FetchPreviousArgs {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The commit whose previous release is wanted, a full SHA.
    #[arg(long)]
    pub sha: String,
    /// The platform whose binaries are taken out.
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// A binary to take out (repeat): the one entry of `<binary>-<commit>-<platform>.tar.gz`.
    #[arg(long = "binary", required = true)]
    pub binaries: Vec<String>,
    /// The directory the binaries go in; made when there is a previous release to unpack.
    #[arg(long)]
    pub out: PathBuf,
    /// A `trusted_root.jsonl` (`gh attestation trusted-root`) to verify the bundles with.
    #[arg(long)]
    pub trusted_root: Option<PathBuf>,
    /// Append the lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// How many times the previous tag's release is asked for before its absence is an error:
    /// GitHub answers a draft release with 404, and a release is one for some minutes after
    /// its tag is pushed.
    #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(u32).range(1..))]
    pub release_attempts: u32,
    /// Seconds between those asks.
    #[arg(long, default_value_t = 30)]
    pub release_poll_secs: u64,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// `gh api <path>`: the JSON it printed. GitHub's 404 is an error here, not `None`: every
/// path this verb reads is named by an answer it already holds, so that it is missing is
/// no evidence that there is no previous release.
fn must_api(gh: &Path, path: &str) -> Result<Value, Error> {
    let found = super::api(gh, path)?;
    found.ok_or_else(|| Error::Failed(format!("gh api {path}: not found (HTTP 404)")))
}

/// What GitHub's compare of `base` (a tag or a commit) against `sha` says: `ahead` when
/// `base` is a strict ancestor of `sha`.
fn compare_status(gh: &Path, repo: &str, base: &str, sha: &str) -> Result<String, Error> {
    let compared = must_api(gh, &format!("repos/{repo}/compare/{base}...{sha}"))?;
    Ok(compared["status"].as_str().unwrap_or("nothing").to_owned())
}

/// The highest release tag of `repo` that names a strict ancestor of `sha`, if any.
fn previous_tag(gh: &Path, repo: &str, sha: &str) -> Result<Option<String>, Error> {
    let mut versions = tag_versions(gh, repo)?;
    // Numbers compare as numbers (10 is above 9); the highest is tried first.
    versions.sort_unstable();
    for version in versions.into_iter().rev() {
        let tag = version.tag();
        let status = compare_status(gh, repo, &tag, sha)?;
        if status == "ahead" {
            return Ok(Some(tag));
        }
        if !matches!(status.as_str(), "identical" | "behind" | "diverged") {
            return Err(Error::Failed(format!(
                "compare {tag}...{sha} says {status:?}: expected ahead, identical, behind or \
                 diverged"
            )));
        }
    }
    Ok(None)
}

/// The `type` and `sha` of the `object` an API answer names.
fn object_of(answer: &Value) -> (Option<&str>, Option<&str>) {
    let object = &answer["object"];
    (object["type"].as_str(), object["sha"].as_str())
}

/// The commit the annotated release tag `tag` names: its ref names a tag object, and that
/// names a commit.
pub(super) fn tag_commit(gh: &Path, repo: &str, tag: &str) -> Result<String, Error> {
    let found = must_api(gh, &format!("repos/{repo}/git/ref/tags/{tag}"))?;
    let (Some("tag"), Some(object)) = object_of(&found) else {
        return Err(Error::Failed(format!(
            "{tag} is not an annotated tag: a release tag is one, and carries its audit record"
        )));
    };
    let annotation = must_api(gh, &format!("repos/{repo}/git/tags/{object}"))?;
    match object_of(&annotation) {
        (Some("commit"), Some(commit)) => Ok(commit.to_owned()),
        _ => Err(Error::Failed(format!("{tag} does not name a commit"))),
    }
}

/// The release of `tag`, waited for while GitHub answers 404: a release is a draft for some
/// minutes after its tag is pushed, and GitHub answers a draft's tag as it answers a missing
/// release. Asked up to `--release-attempts` times, `--release-poll-secs` apart; still missing
/// after the last, it is the error a missing release always was.
fn published_release(a: &FetchPreviousArgs, tag: &str) -> Result<Value, Error> {
    let path = format!("repos/{}/releases/tags/{tag}", a.repo);
    let attempts = a.release_attempts;
    for attempt in 1..=attempts {
        if let Some(found) = super::api(&a.gh, &path)? {
            return Ok(found);
        }
        if attempt < attempts {
            eprintln!(
                "xtask: gh api {path}: not found yet (a release is a draft until it is \
                 published); asking again in {} s ({attempt} of {attempts})",
                a.release_poll_secs
            );
            std::thread::sleep(std::time::Duration::from_secs(a.release_poll_secs));
        }
    }
    Err(Error::Failed(format!(
        "gh api {path}: not found (HTTP 404), asked {attempts} times {} s apart: the release of \
         {tag} is still a draft, or does not exist",
        a.release_poll_secs
    )))
}

/// Creates `dir` and its parents.
fn make_dir(dir: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))
}

/// Downloads the release `tag` (of `commit`) into `scratch`, verifies it, and writes the
/// binaries `a` asks for.
fn unpack(a: &FetchPreviousArgs, tag: &str, commit: &str, scratch: &Path) -> Result<(), Error> {
    published_release(a, tag)?;
    let assets = scratch.join("assets");
    let flipped = scratch.join("flipped");
    for dir in [&assets, &flipped] {
        make_dir(dir)?;
    }
    let download = assets.display().to_string();
    super::gh_run(
        &a.gh,
        &[
            "release", "download", tag, "--repo", &a.repo, "--dir", &download,
        ],
    )?;
    // The release before holds the version facts of its `kbf` and the facts and SBOM of its
    // task image, as every release since v0.0.41 does; its files are attested below.
    super::check(&assets, commit)?;
    let attester = Gh {
        repo: &a.repo,
        trusted_root: a.trusted_root.as_deref(),
        gh: &a.gh,
        scratch: &flipped,
    };
    attester.artifacts(&assets, commit)?;
    // Everything is read before anything is written: a binary the release lacks leaves no
    // file behind.
    let suffix = format!("-{commit}-{}.tar.gz", a.platform.name());
    let wanted: BTreeSet<&String> = a.binaries.iter().collect();
    let mut binaries = Vec::new();
    for binary in wanted {
        let tarball = assets.join(format!("{binary}{suffix}"));
        let tgz = read(&tarball)?;
        let bytes = packaged_binary(&tarball, &tgz, binary)?;
        binaries.push((binary, bytes));
    }
    make_dir(&a.out)?;
    let mut written: Vec<PathBuf> = Vec::new();
    for (binary, bytes) in binaries {
        let path = a.out.join(binary);
        if let Err(refused) = write_executable(&path, &bytes) {
            // A file already there is not replaced, and what this run wrote stays no longer.
            for done in &written {
                let _ = std::fs::remove_file(done);
            }
            return Err(refused);
        }
        written.push(path);
    }
    Ok(())
}

/// A directory of this process alone, under the temporary directory.
fn scratch_path() -> PathBuf {
    std::env::temp_dir().join(format!("xtask-fetch-previous-{}", std::process::id()))
}

/// The lines to print for the release `tag`, after its binaries are unpacked.
fn previous_release(a: &FetchPreviousArgs, tag: &str) -> Result<Vec<String>, Error> {
    let commit = tag_commit(&a.gh, &a.repo, tag)?;
    let status = compare_status(&a.gh, &a.repo, &commit, &a.sha)?;
    if status != "ahead" {
        return Err(Error::Failed(format!(
            "{tag} names {commit}, which is no ancestor of {}: compare says {status:?}",
            a.sha
        )));
    }
    let scratch = scratch_path();
    let unpacked = unpack(a, tag, &commit, &scratch);
    // Nothing in it is needed after the run, whatever its result.
    let _ = std::fs::remove_dir_all(&scratch);
    unpacked?;
    Ok(vec![
        format!("previous-tag={tag}"),
        format!("previous-sha={commit}"),
        "previous-source=release".to_owned(),
    ])
}

/// The `release fetch-previous` verb: the lines to print.
pub fn run(a: &FetchPreviousArgs) -> Result<Vec<String>, Error> {
    full_commit("--sha", &a.sha)?;
    let lines = match previous_tag(&a.gh, &a.repo, &a.sha)? {
        Some(tag) => previous_release(a, &tag)?,
        None => vec!["previous-source=none".to_owned()],
    };
    if let Some(output) = &a.output {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .and_then(|mut f| writeln!(f, "{}", lines.join("\n")))
            .map_err(Error::spawn(format!("append to {}", output.display())))?;
    }
    Ok(lines)
}
