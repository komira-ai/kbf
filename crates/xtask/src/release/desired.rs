//! `release desired --repo <owner/repo> --version <X.Y.Z or ""> --after-run <run id or ""> --dir
//! <dir> [--output <file>] [--apt-url <url>] [--gh <program>] [--curl <program>]`: what
//! `.github/workflows/hostkeeper-desired.yml` sets as the desired version of the hostkeeper
//! service `kbf-storage-and-daemon`, or why it sets nothing. It runs before any step of that
//! workflow can hold a token, so a refusal here means the coordinator is never called.
//!
//! Which release:
//!
//! - `--version X.Y.Z` (a run by hand): the release `vX.Y.Z`, whichever it is, an older one
//!   included (a rollback).
//! - `--after-run <id>` (the completion of a run of `release.yml`): when the job `publish` of
//!   that run succeeded, the newest published release of `<owner/repo>` (the highest `vX.Y.Z`,
//!   not a draft, not a pre-release). Release runs are one at a time, so that is the release the
//!   run published, or a newer one whose run then comes next. When `publish` did not succeed
//!   (most runs plan no release, and their `publish` is skipped) nothing is set: the output is
//!   `set=false` and the verb succeeds.
//!
//! Exactly one of the two is given; an empty one counts as not given, so the workflow passes
//! both from its two events and one is always empty.
//!
//! The release must be a published GitHub release (not a draft, not a pre-release). The commit
//! it releases is the one its tag names. Its `SHA256SUMS` is downloaded into `<dir>`, and then:
//!
//! - `SHA256SUMS` lists `kbf-native-<commit>-linux-x86_64.deb` (architecture `amd64`) and
//!   `kbf-native-<commit>-linux-arm64.deb` (`arm64`);
//! - the `Packages` index of the suite `stable` for each architecture, read through the CDN as
//!   a host reads it (`--apt-url`), has a stanza of the package `kbf` whose `Filename` is
//!   `pool/kbf/kbf_<Version>_<arch>.deb` with the SHA-256 `SHA256SUMS` gives. The stable suite
//!   is what hosts install from: the release promoted the package there after its install
//!   checks;
//! - the two stanzas have the same `Version`, three decimal numbers: the package version.
//!
//! Every problem found is reported, one line each. When there is none, `--output` (on Actions,
//! `GITHUB_OUTPUT`) gets `set=true`, `package-version=<X.Y.Z>` and `targets=<one-line JSON>`:
//! the proto3 JSON of hostkeeper's `TargetList` per platform ([`manifest`]; no `darwin-arm64`,
//! which has no package).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::tags::Version;
use super::{append_output, gh_run};
use crate::Error;
use crate::build_commit::full_commit;
use crate::package::Platform;
use crate::sums::SUMS;

pub mod manifest;
pub mod repo;

#[cfg(test)]
mod tests;

/// The job of `release.yml` that publishes a release.
pub const PUBLISH_JOB: &str = "publish";

/// The package the `.deb`s are of.
pub const PACKAGE: &str = manifest::PACKAGE;

/// The directory of the repository's pool that holds the package, as a `Packages` stanza's
/// `Filename` names it: the publisher's own.
pub const POOL_DIR: &str = "pool/kbf";

/// The platforms with a package and the Debian architecture of each.
const PLATFORMS: [(Platform, &str); 2] = [
    (Platform::LinuxX86_64, "amd64"),
    (Platform::LinuxArm64, "arm64"),
];

/// The arguments of `release desired`.
#[derive(clap::Args)]
pub struct DesiredArgs {
    /// `owner/name` of the repository whose release is set.
    #[arg(long)]
    pub repo: String,
    /// A released version, `X.Y.Z`; empty when `--after-run` is given.
    #[arg(long)]
    pub version: String,
    /// The id of a run of `release.yml`; empty when `--version` is given.
    #[arg(long)]
    pub after_run: String,
    /// Where `SHA256SUMS` and the `Packages` files are downloaded.
    #[arg(long)]
    pub dir: PathBuf,
    /// The file the outputs are appended to (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The apt repository's URL, as a host reads it.
    #[arg(long, default_value = "https://packages.komira.ai/apt")]
    pub apt_url: String,
    /// The GitHub CLI.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
    /// The program that downloads a file.
    #[arg(long, default_value = "curl")]
    pub curl: PathBuf,
}

/// What the verb asks of GitHub and of the CDN, behind a seam so tests run without either.
pub trait Source {
    /// What `gh api <path>` answered, `None` for a 404.
    fn api(&self, path: &str) -> Result<Option<Value>, Error>;
    /// Downloads the asset `SHA256SUMS` of the release `tag` of `repo` into `dir`.
    fn download_sums(&self, repo: &str, tag: &str, dir: &Path) -> Result<(), Error>;
    /// Reads `url` into `file`: `Ok(Some(why))` when the server did not give it.
    fn fetch(&self, url: &str, file: &Path) -> Result<Option<String>, Error>;
}

/// `gh` and `curl` as programs.
struct Programs<'a> {
    gh: &'a Path,
    curl: &'a Path,
}

impl Source for Programs<'_> {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        super::api(self.gh, path)
    }

    fn download_sums(&self, repo: &str, tag: &str, dir: &Path) -> Result<(), Error> {
        let dir = dir.display().to_string();
        let args = [
            "release",
            "download",
            tag,
            "--repo",
            repo,
            "--dir",
            &dir,
            "--pattern",
            SUMS,
        ];
        gh_run(self.gh, &args)
    }

    fn fetch(&self, url: &str, file: &Path) -> Result<Option<String>, Error> {
        let out = Command::new(self.curl)
            .args(curl_args(file, url))
            .output()
            .map_err(Error::spawn(self.curl.display().to_string()))?;
        if out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
        ))
    }
}

/// The arguments of the `curl` that reads `url` into `file`: HTTPS only, TLS 1.2 or later, a
/// failure on an HTTP error status and five retries, as a host's tooling reads a repository.
fn curl_args(file: &Path, url: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--proto",
        "=https",
        "--tlsv1.2",
        "--fail",
        "--silent",
        "--show-error",
        "--retry",
        "5",
        "--retry-delay",
        "20",
        "--retry-all-errors",
        "--max-time",
        "60",
        "--output",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    args.push(file.display().to_string());
    args.push(url.to_owned());
    args
}

/// The elements of the array `value`, none for anything else.
fn array(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

/// `gh api <path>` that must answer.
fn must_api(source: &dyn Source, path: &str) -> Result<Value, Error> {
    source
        .api(path)?
        .ok_or_else(|| Error::Failed(format!("gh api {path}: not found")))
}

/// Whether the job [`PUBLISH_JOB`] of the run `run` of `repo` succeeded.
fn published(source: &dyn Source, repo: &str, run: &str) -> Result<bool, Error> {
    let path = format!("repos/{repo}/actions/runs/{run}/jobs?per_page=100&filter=latest");
    let found = must_api(source, &path)?;
    let jobs = array(&found["jobs"]);
    // One page holds every job of release.yml; a run with more is refused, not half read.
    if found["total_count"].as_u64() != Some(jobs.len() as u64) {
        return Err(Error::Failed(format!(
            "run {run} of {repo} has {} jobs; one page of {} was read",
            found["total_count"],
            jobs.len()
        )));
    }
    Ok(jobs
        .iter()
        .any(|j| j["name"] == PUBLISH_JOB && j["conclusion"] == "success"))
}

/// The highest version among the published releases of `repo` (their tags `vX.Y.Z`).
fn newest(source: &dyn Source, repo: &str) -> Result<Version, Error> {
    let found = must_api(source, &format!("repos/{repo}/releases?per_page=100"))?;
    array(&found)
        .iter()
        .filter(|r| r["draft"] == false && r["prerelease"] == false)
        .filter_map(|r| Version::parse_tag(r["tag_name"].as_str()?))
        .max()
        .ok_or_else(|| Error::Failed(format!("{repo} has no published release vX.Y.Z")))
}

/// The version the request names: `--version`, or the newest release when the run of
/// `--after-run` published one; `None` when it published none.
fn chosen(source: &dyn Source, args: &DesiredArgs) -> Result<Option<Version>, Error> {
    match (args.version.as_str(), args.after_run.as_str()) {
        (version, "") if !version.is_empty() => Version::parse_tag(&format!("v{version}"))
            .map(Some)
            .ok_or_else(|| {
                Error::Usage(format!(
                    "--version {version:?} is not <major>.<minor>.<patch>, a release tag without its v"
                ))
            }),
        ("", run) if !run.is_empty() => {
            if !run.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Usage(format!(
                    "--after-run {run:?} is not the id of a workflow run"
                )));
            }
            if published(source, &args.repo, run)? {
                newest(source, &args.repo).map(Some)
            } else {
                Ok(None)
            }
        }
        _ => Err(Error::Usage(
            "give one of --version and --after-run, not both and not neither".to_owned(),
        )),
    }
}

/// The names of the assets of the published release `tag` of `repo`.
fn assets(source: &dyn Source, repo: &str, tag: &str) -> Result<Vec<String>, Error> {
    let found = must_api(source, &format!("repos/{repo}/releases/tags/{tag}"))?;
    if found["draft"] == true || found["prerelease"] == true {
        return Err(Error::Failed(format!(
            "release {tag} of {repo} is a draft or a pre-release, not a released version"
        )));
    }
    Ok(array(&found["assets"])
        .iter()
        .filter_map(|a| a["name"].as_str().map(str::to_owned))
        .collect())
}

/// The commit the tag `tag` of `repo` names.
fn tag_commit(source: &dyn Source, repo: &str, tag: &str) -> Result<String, Error> {
    // `refs/tags/`: a bare name would resolve a branch of that name as well.
    let path = format!("repos/{repo}/commits/refs/tags/{tag}");
    let Some(found) = source.api(&path)? else {
        return Err(Error::Failed(format!(
            "the tag {tag} does not exist in {repo}"
        )));
    };
    let commit = found["sha"].as_str().unwrap_or_default();
    full_commit("the commit of the tag", commit).map_err(|e| Error::Failed(e.to_string()))?;
    Ok(commit.to_owned())
}

/// The release asset name of the native `.deb` of `commit` for `platform`.
fn deb_name(commit: &str, platform: Platform) -> String {
    format!("kbf-native-{commit}-{}.deb", platform.name())
}

/// Whether `text` is a package version of the form `X.Y.Z`: three decimal numbers.
fn is_package_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// What the sums and the stable suite say of the `.deb`s of a release.
struct Found {
    /// The digest of each platform's `.deb`, as `SHA256SUMS` gives it.
    digests: Vec<(Platform, String)>,
    /// The `Version` the stable suite lists for each of them.
    versions: Vec<String>,
    /// What is wrong.
    problems: Vec<String>,
}

/// Holds the `.deb`s of `commit` the sums list to the stable suite.
fn check_suite(
    source: &dyn Source,
    args: &DesiredArgs,
    sums: &str,
    commit: &str,
    tag: &str,
) -> Result<Found, Error> {
    let mut found = Found {
        digests: Vec::new(),
        versions: Vec::new(),
        problems: Vec::new(),
    };
    for (platform, arch) in PLATFORMS {
        let name = deb_name(commit, platform);
        let Some(digest) = repo::sums_digest(sums, &name)? else {
            found
                .problems
                .push(format!("{SUMS} of release {tag} lists no {name}"));
            continue;
        };
        let file = args.dir.join(format!("Packages-{arch}"));
        let url = format!("{}/dists/stable/main/binary-{arch}/Packages", args.apt_url);
        if let Some(why) = source.fetch(&url, &file)? {
            found
                .problems
                .push(format!("{url} could not be read: {why}"));
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(Error::spawn(format!("read {}", file.display())))?;
        match repo::listed(&text, arch, &digest) {
            repo::Listed::No(listed) if listed.is_empty() => found.problems.push(format!(
                "the stable suite lists no {PACKAGE} package for {arch}: the release is not promoted there"
            )),
            repo::Listed::No(listed) => found.problems.push(format!(
                "the stable suite lists no {PACKAGE} {arch} package with sha256 {digest} (the digest {SUMS} lists for {name}); it lists versions {}",
                listed.join(", ")
            )),
            repo::Listed::Yes(version) => {
                found.digests.push((platform, digest));
                found.versions.push(version);
            }
        }
    }
    Ok(found)
}

/// The one package version the stanzas found state, or the problems with them.
fn agreed(found: &mut Found) -> Option<String> {
    let first = found.versions.first()?.clone();
    if found.versions.iter().any(|v| *v != first) {
        found.problems.push(format!(
            "the stable suite lists the two architectures with different versions: {}",
            found.versions.join(", ")
        ));
        return None;
    }
    if !is_package_version(&first) {
        found.problems.push(format!(
            "the stable suite lists version {first:?}, which is not <major>.<minor>.<patch>"
        ));
        return None;
    }
    Some(first)
}

/// Runs `release desired` through `source`: the lines to print.
///
/// # Errors
/// [`Error::Usage`] for neither or both of `--version` and `--after-run`, a `--version` that is
/// not `<major>.<minor>.<patch>`, an `--after-run` that is not digits or an `--apt-url` that is
/// not an https URL without a trailing `/`; [`Error::Failed`] for a run GitHub lists in more
/// than one page, a repository with no published release, a release that is a draft or a
/// pre-release, a tag that names no commit or a `SHA256SUMS` line that is malformed;
/// [`Error::Refused`], with a line per problem, for a release without `SHA256SUMS`, one whose
/// sums do not list a `.deb`, a stable suite that does not list a `.deb` with the digest of the
/// sums, or one that lists versions that differ or are not `X.Y.Z`.
pub fn run_with(source: &dyn Source, args: &DesiredArgs) -> Result<Vec<String>, Error> {
    if !args.apt_url.starts_with("https://") || args.apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {:?} is not an https URL without a trailing /",
            args.apt_url
        )));
    }
    let Some(version) = chosen(source, args)? else {
        let lines = [
            format!(
                "run {} of {} did not publish a release ({PUBLISH_JOB} did not succeed): nothing is set",
                args.after_run, args.repo
            ),
            "set=false".to_owned(),
        ];
        if let Some(output) = &args.output {
            append_output(output, &lines[1..])?;
        }
        return Ok(lines.to_vec());
    };
    let tag = version.tag();
    if !assets(source, &args.repo, &tag)?.iter().any(|n| n == SUMS) {
        return Err(Error::Refused(vec![format!(
            "release {tag} has no {SUMS}: there are no digests to hold the stable suite to"
        )]));
    }
    let commit = tag_commit(source, &args.repo, &tag)?;
    std::fs::create_dir_all(&args.dir)
        .map_err(Error::spawn(format!("create {}", args.dir.display())))?;
    source.download_sums(&args.repo, &tag, &args.dir)?;
    let sums_path = args.dir.join(SUMS);
    let sums = std::fs::read_to_string(&sums_path)
        .map_err(Error::spawn(format!("read {}", sums_path.display())))?;
    let mut found = check_suite(source, args, &sums, &commit, &tag)?;
    let package_version = agreed(&mut found);
    let Some(package_version) = package_version.filter(|_| found.problems.is_empty()) else {
        return Err(Error::Refused(found.problems));
    };
    let lines = vec![
        "set=true".to_owned(),
        format!("package-version={package_version}"),
        format!(
            "targets={}",
            manifest::targets(&package_version, &found.digests)
        ),
    ];
    if let Some(output) = &args.output {
        append_output(output, &lines)?;
    }
    let mut printed = vec![format!(
        "release {tag}: the stable suite lists {PACKAGE} {package_version} for each of {} architectures, each with the digest {SUMS} lists",
        found.digests.len()
    )];
    printed.extend(lines);
    Ok(printed)
}

/// Runs `release desired` with `gh` and `curl` as programs: the lines to print.
///
/// # Errors
/// As [`run_with`], and [`Error::Spawn`] for a program that does not run or fails.
pub fn run(args: &DesiredArgs) -> Result<Vec<String>, Error> {
    run_with(
        &Programs {
            gh: &args.gh,
            curl: &args.curl,
        },
        args,
    )
}
