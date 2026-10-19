//! `cargo xtask release`: a GitHub release of the exact files a push run of the
//! artifacts workflow on `main` built and attested for one commit (docs/artifacts.md).
//! Nothing is rebuilt; `.github/workflows/release.yml` runs these verbs.
//!
//! - `release locate --repo <owner/repo> --commit <sha> [--output <file>] [--gh
//!   <program>]` ([`locate`]): refuses a commit that is not on `main` (GitHub's compare
//!   of `main...<sha>` is neither `identical` nor `behind`), then one with no successful
//!   `push` run of `artifacts.yml` on `main` for that exact commit, then a run that no
//!   longer holds every artifact a release needs ([`ARTIFACTS`]). Prints `run-id=<id>`,
//!   and appends it to `--output` (on Actions, `GITHUB_OUTPUT`).
//! - `release main-control --repo <owner/repo> --on-main <sha> --off-main <sha>`
//!   ([`main_control`]): the on-main check against GitHub, both ways: `--on-main` must
//!   pass and `--off-main` must be refused. CI runs it on pull requests into `main`
//!   with the base commit and the pull request's merge commit, so a check that accepts
//!   every commit (or none) fails there instead of at a release.
//! - `release check <dir> --commit <sha>` ([`check`]): `<dir>` passes `verify-assets
//!   sums`, every file `SHA256SUMS` lists is `<binary>-<sha>-<platform>.tar.gz` or
//!   `.cdx.json`: the assets of that commit and no other, and each tarball has exactly
//!   one provenance bundle, `<tarball>.sigstore.json`, whose one subject is that
//!   tarball's digest ([`bundles`]), and no bundle is there for anything else.
//! - `release verdict <dir> --repo-dir <checkout> --tag <tag> --commit <sha> [--main-ref
//!   <ref>] [--output <file>]` ([`verdict`]): the annotated tag's message is a green
//!   gamma verdict for that tag, that commit (on main) and the set `<dir>`'s `SHA256SUMS`
//!   hashes to, after a bake of 24 hours or more. Prints `set-hash=<hex>`, which
//!   `publish --set-hash` must be given.
//! - `release offer <dir> --repo <owner/repo> --commit <sha> --platform <p>... (--beta-result
//!   <file> | --waive-beta <reason> --operator <name>) [--previous <sha> --version-json <file>
//!   --previous-version-json <file>] [--plan] [--out offer.json]` ([`offer`]): the checks
//!   that make a commit offerable, and `offer.json`, the record a deploy reads. It sets
//!   nothing on any farm.
//! - `release notes <dir> ... --out <file>` ([`notes`]): the same checks, then the
//!   release's notes: commits since the previous `v*` tag, the verdict's summary, what
//!   is not reversible, the gamma escapes and what is withdrawn.
//! - `release publish <dir> --repo <owner/repo> --tag <tag> --commit <sha> --run-id <id>
//!   [--gh <program>]` ([`publish`]): `check`, then creates release `<tag>` with every
//!   file of `<dir>` (the bundles beside their tarballs). When the release exists,
//!   every asset it holds must be one of these files, byte for byte, or nothing is
//!   changed; the files it lacks are uploaded. A re-run therefore creates no second
//!   release and replaces no asset.

mod git;
pub mod notes;
pub mod offer;
pub mod verdict;

use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::Error;
use crate::build_commit::full_commit;
use crate::bundles;
use crate::fetch::Sha256;
use crate::package::{Platform, read};
use crate::sums::{self, SUMS};

/// The branch a released commit must be on.
pub const MAIN: &str = "main";

/// The workflow whose push runs on `main` build and attest the assets.
pub const ARTIFACTS_WORKFLOW: &str = "artifacts.yml";

/// The artifacts such a run leaves, all of which a release needs: the files, the sums,
/// and the provenance bundles of each platform group's tarballs (the `attest-*` jobs').
pub const ARTIFACTS: [&str; 6] = [
    "assets-darwin-arm64",
    "assets-linux-x86_64",
    "assets-linux-arm64",
    "sha256sums",
    "bundles-darwin-arm64",
    "bundles-linux",
];

/// `gh api <path>`: the JSON it printed, or `None` when GitHub answered 404.
fn api(gh: &Path, path: &str) -> Result<Option<Value>, Error> {
    let out = Command::new(gh)
        .args(["api", path])
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if err.contains("(HTTP 404)") {
            return Ok(None);
        }
        return Err(Error::Failed(format!("gh api {path}: {}", err.trim_end())));
    }
    let json = serde_json::from_slice(&out.stdout);
    json.map(Some)
        .map_err(|e| Error::Failed(format!("gh api {path}: not JSON: {e}")))
}

/// Runs `gh <args>`; an error holds what it printed on stderr.
fn gh_run(gh: &Path, args: &[&str]) -> Result<(), Error> {
    let what = format!("gh {}", args[..2].join(" "));
    let out = Command::new(gh)
        .args(args)
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Failed(format!("{what}: {}", err.trim_end())))
}

/// Whether `commit` is on `main`: `main` holds it (compared, `main` is identical to it
/// or ahead of it). An unknown commit is not.
pub fn on_main(gh: &Path, repo: &str, commit: &str) -> Result<bool, Error> {
    let compared = api(gh, &format!("repos/{repo}/compare/{MAIN}...{commit}"))?;
    let status = compared.as_ref().and_then(|v| v["status"].as_str());
    Ok(matches!(status, Some("identical" | "behind")))
}

/// The `release locate` verb: the lines to print.
pub fn locate(
    gh: &Path,
    repo: &str,
    commit: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !on_main(gh, repo, commit)? {
        return Err(Error::Failed(format!(
            "{commit} is not on {MAIN}: a release is cut only from a commit on {MAIN}"
        )));
    }
    let runs = api(
        gh,
        &format!(
            "repos/{repo}/actions/workflows/{ARTIFACTS_WORKFLOW}/runs?head_sha={commit}\
             &event=push&branch={MAIN}&status=success&per_page=100"
        ),
    )?;
    let listed = runs.as_ref().and_then(|v| v["workflow_runs"].as_array());
    let run = listed.into_iter().flatten().find(|r| {
        r["head_sha"] == commit
            && r["head_branch"] == MAIN
            && r["event"] == "push"
            && r["conclusion"] == "success"
    });
    let Some(id) = run.and_then(|r| r["id"].as_u64()) else {
        return Err(Error::Failed(format!(
            "no successful push run of {ARTIFACTS_WORKFLOW} on {MAIN} for {commit}"
        )));
    };
    let artifacts = api(
        gh,
        &format!("repos/{repo}/actions/runs/{id}/artifacts?per_page=100"),
    )?;
    let kept: Vec<&str> = artifacts
        .as_ref()
        .and_then(|v| v["artifacts"].as_array())
        .into_iter()
        .flatten()
        .filter(|a| a["expired"] == false)
        .filter_map(|a| a["name"].as_str())
        .collect();
    let missing: Vec<String> = ARTIFACTS
        .iter()
        .filter(|a| !kept.contains(*a))
        .map(|a| format!("run {id} of {ARTIFACTS_WORKFLOW} holds no unexpired artifact {a}"))
        .collect();
    if !missing.is_empty() {
        return Err(Error::Refused(missing));
    }
    let line = format!("run-id={id}");
    if let Some(output) = output {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output)
            .and_then(|mut f| writeln!(f, "{line}"))
            .map_err(Error::spawn(format!("append to {}", output.display())))?;
    }
    Ok(vec![
        format!("{commit} is on {MAIN}; run {id} of {ARTIFACTS_WORKFLOW} built it"),
        line,
    ])
}

/// The `release main-control` verb: the line to print.
pub fn main_control(gh: &Path, repo: &str, on: &str, off: &str) -> Result<String, Error> {
    full_commit("--on-main", on)?;
    full_commit("--off-main", off)?;
    let mut problems = Vec::new();
    if !on_main(gh, repo, on)? {
        problems.push(format!("control failed: {on}, on {MAIN}, was refused"));
    }
    if on_main(gh, repo, off)? {
        problems.push(format!(
            "control failed: {off}, not on {MAIN}, was accepted"
        ));
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(format!(
        "on-{MAIN} check: {on} is accepted and {off} is refused"
    ))
}

/// Whether `name` is an asset of `commit`: `<binary>-<commit>-<platform>` and
/// `.tar.gz` or `.cdx.json`.
fn is_asset_of(name: &str, commit: &str) -> bool {
    let stem = name
        .strip_suffix(".tar.gz")
        .or_else(|| name.strip_suffix(".cdx.json"));
    let parts = stem.and_then(|s| s.split_once(&format!("-{commit}-")));
    parts
        .is_some_and(|(binary, platform)| !binary.is_empty() && Platform::named(platform).is_some())
}

/// The `release check` verb: the lines to print, then every file name of `dir`
/// (`SHA256SUMS` first, then the listed files, then the bundles).
fn checked(dir: &Path, commit: &str) -> Result<(Vec<String>, Vec<String>), Error> {
    full_commit("--commit", commit)?;
    let sums_line = sums::verify_sums(dir, Sha256::of)?;
    let lines = sums::read_sums(&dir.join(SUMS))?;
    let foreign: Vec<String> = lines
        .iter()
        .filter(|l| !is_asset_of(&l.name, commit))
        .map(|l| format!("{}: not an asset of {commit}", l.name))
        .collect();
    if !foreign.is_empty() {
        return Err(Error::Refused(foreign));
    }
    let unpaired = bundles::pairing(dir, &lines)?;
    if !unpaired.is_empty() {
        return Err(Error::Refused(unpaired));
    }
    let mut names = vec![SUMS.to_owned()];
    names.extend(lines.iter().map(|l| l.name.clone()));
    let tarballs: Vec<&str> = lines
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| n.ends_with(".tar.gz"))
        .collect();
    names.extend(tarballs.iter().map(|t| bundles::name_of(t)));
    let printed = vec![
        sums_line,
        format!("names: every listed file is an asset of {commit}"),
        format!(
            "bundles: each of the {} tarballs has one provenance bundle",
            tarballs.len()
        ),
    ];
    Ok((printed, names))
}

/// The `release check` verb: the lines to print.
pub fn check(dir: &Path, commit: &str) -> Result<Vec<String>, Error> {
    Ok(checked(dir, commit)?.0)
}

/// Which release `release publish` makes or completes.
pub struct Release<'a> {
    /// The repository, `<owner>/<repo>`.
    pub repo: &'a str,
    /// The tag, which must exist (`gh release create --verify-tag`).
    pub tag: &'a str,
    /// The commit the tag names, whose assets these are.
    pub commit: &'a str,
    /// The run of the artifacts workflow that built and attested them.
    pub run_id: u64,
    /// The set hash the gamma verdict names (`release verdict` prints it): the files must
    /// be that set, or nothing is published.
    pub set_hash: &'a kbf_gate::SetHash,
    /// The `gh` program.
    pub gh: &'a Path,
    /// An empty directory, for the assets the release already holds.
    pub scratch: &'a Path,
}

impl Release<'_> {
    /// The release's notes: where the files come from, and how to verify one, online
    /// and (for a tarball) offline with the bundle released beside it.
    fn notes(&self) -> String {
        let repo = self.repo;
        format!(
            "The files run {id} of {ARTIFACTS_WORKFLOW} built and attested for {commit} on \
             {MAIN} (https://github.com/{repo}/actions/runs/{id}), unchanged: a release \
             rebuilds nothing.\n\nVerify an asset before use:\n\n```text\ngh attestation \
             verify <asset> --repo {repo} --signer-workflow \
             {repo}/.github/workflows/{ARTIFACTS_WORKFLOW} --source-ref refs/heads/{MAIN} \
             --deny-self-hosted-runners\n```\n\nEach tarball also has its provenance bundle, \
             `<asset>{SUFFIX}`, which verifies it with no network but a trusted root \
             (`gh attestation trusted-root > trusted_root.jsonl`):\n\n```text\ngh attestation \
             verify <asset> --bundle <asset>{SUFFIX} --custom-trusted-root trusted_root.jsonl \
             --repo {repo} --signer-workflow {repo}/.github/workflows/{ARTIFACTS_WORKFLOW} \
             --cert-oidc-issuer {ISSUER} --source-ref refs/heads/{MAIN} --source-digest \
             {commit} --predicate-type {PROVENANCE} --deny-self-hosted-runners\n```\n",
            id = self.run_id,
            commit = self.commit,
            SUFFIX = bundles::SUFFIX,
            ISSUER = bundles::ISSUER,
            PROVENANCE = bundles::PROVENANCE,
        )
    }
}

/// The `release publish` verb: the lines to print.
pub fn publish(dir: &Path, r: &Release) -> Result<Vec<String>, Error> {
    let (mut printed, names) = checked(dir, r.commit)?;
    let held_set = verdict::set_hash_of(dir)?;
    if &held_set != r.set_hash {
        return Err(Error::Refused(vec![format!(
            "the files in {} are set {}, the gamma verdict names set {}: gamma ran other bytes",
            dir.display(),
            held_set.as_str(),
            r.set_hash.as_str()
        )]));
    }
    let paths: Vec<String> = names
        .iter()
        .map(|n| dir.join(n).display().to_string())
        .collect();
    let path_args = paths.iter().map(String::as_str);
    let held = api(r.gh, &format!("repos/{}/releases/tags/{}", r.repo, r.tag))?;
    let Some(held) = held else {
        let notes = r.notes();
        let mut args = vec!["release", "create", r.tag, "--repo", r.repo, "--verify-tag"];
        args.extend(["--title", r.tag, "--notes", &notes]);
        args.extend(path_args);
        gh_run(r.gh, &args)?;
        printed.push(format!(
            "created release {} with {} files",
            r.tag,
            names.len()
        ));
        return Ok(printed);
    };
    let held: Vec<&str> = held["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str())
        .collect();
    if !held.is_empty() {
        let scratch = r.scratch.display().to_string();
        gh_run(
            r.gh,
            &[
                "release", "download", r.tag, "--repo", r.repo, "--dir", &scratch,
            ],
        )?;
    }
    let mut problems = Vec::new();
    for name in &held {
        if !names.iter().any(|n| n == name) {
            problems.push(format!(
                "release {} holds {name}, not an asset of {}",
                r.tag, r.commit
            ));
        } else if read(&r.scratch.join(name))? != read(&dir.join(name))? {
            problems.push(format!(
                "release {} holds {name} with other bytes than the verified file",
                r.tag
            ));
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let missing: Vec<&str> = names
        .iter()
        .zip(paths.iter())
        .filter(|(n, _)| !held.contains(&n.as_str()))
        .map(|(_, p)| p.as_str())
        .collect();
    if !missing.is_empty() {
        let mut args = vec!["release", "upload", r.tag, "--repo", r.repo];
        args.extend(missing.iter().copied());
        gh_run(r.gh, &args)?;
    }
    printed.push(format!(
        "release {}: {} files already held byte for byte, {} uploaded",
        r.tag,
        held.len(),
        missing.len()
    ));
    Ok(printed)
}
