//! Fetching a release from the GitHub release of the repository, for a channel whose source
//! is `github`. Everything here runs through the pinned `gh`, as `nobody`, in a directory
//! `nobody` owns; what it leaves there is untrusted bytes until the root process has copied
//! them once and checked the copy ([`super::stage`]).
//!
//! 1. The releases are listed with the system `curl` from the public REST endpoint
//!    `https://api.github.com/repos/<repo>/releases`, a page of 100 at a time until a page
//!    is empty, at most [`MAX_PAGES`] pages. No token is needed (the API allows 60 requests
//!    an hour per address without one). Never only the latest: a rollback
//!    names an old release. Every page is read, because two commits that start with the
//!    version's 12 hex digits are an ambiguous release and only the whole listing shows it.
//! 2. A release asset named `kbf-<40 hex>-darwin-arm64.tar.gz` whose commit starts with the
//!    digits picks the release. None is [`FetchError::Unknown`] (or a failure when the
//!    listing was cut at the page limit: it was not all seen); two commits are
//!    [`FetchError::Ambiguous`].
//! 3. The download is rebuilt from the release's tag and the asset names, which are checked:
//!    `gh release download <tag> --pattern <tarball> --pattern <bundle>`. No URL of the
//!    listing (`browser_download_url`, `url`) is ever read, let alone followed.
//!    The command has a time limit and a limit on what it may write.
//!
//! A channel may name a `token_file`: a token for the public API, no scopes needed, read by
//! root for a higher rate limit. The listing sends it to `curl` as a header in a
//! configuration on its standard input (never in the arguments, which every user can read),
//! and the download gets it as `GH_TOKEN`. Nothing is trusted because the API said it; the
//! attestation is the trust anchor.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::release::{self, REPO};
use crate::runner::{Cmd, Identity, Runner, WriteCap};

/// Releases asked for per page (GitHub's most).
pub const PER_PAGE: usize = 100;

/// The most pages read.
pub const MAX_PAGES: u32 = 30;

/// The most bytes of one page that are read.
pub const MAX_PAGE_BYTES: usize = 16 * 1024 * 1024;

/// Why nothing was fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// No asset starts with the digits.
    Unknown,
    /// The assets of two commits do.
    Ambiguous,
    /// The listing or the download did not work, and why.
    Failed(String),
}

fn failed<T>(why: impl Into<String>) -> Result<T, FetchError> {
    Err(FetchError::Failed(why.into()))
}

/// What a fetch may take.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long one listing page may take.
    pub tool: Duration,
    /// How long the download may take.
    pub download: Duration,
    pub tarball_max: u64,
    pub bundle_max: u64,
}

/// The system `curl` that lists and the pinned `gh` that downloads, run as `who` with `home`
/// as the home of `gh`. `token`, when there is one, is one line of letters, digits, `_` and
/// `-`.
pub struct Fetcher<'a> {
    pub runner: &'a dyn Runner,
    pub curl: &'a Path,
    pub gh: &'a Path,
    pub who: Identity,
    pub home: &'a Path,
    pub token: Option<&'a str>,
    pub limits: Limits,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// The two fields of an asset that are used; its other fields, URLs among them, are not
/// read.
#[derive(Deserialize)]
struct Asset {
    name: String,
    #[serde(default)]
    size: u64,
}

/// Where a commit's tarball was listed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    tag: String,
    tarball_size: u64,
    bundle_size: Option<u64>,
}

/// A tag as `gh release download` takes one as an argument: letters, digits and `. _ + -`,
/// not starting with a sign or a dot.
fn tag_ok(tag: &str) -> bool {
    tag.len() <= 128
        && tag.starts_with(|c: char| c.is_ascii_alphanumeric())
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

impl Fetcher<'_> {
    fn env(&self) -> Vec<(String, String)> {
        let home = self.home.display().to_string();
        let mut env = vec![
            ("HOME".to_owned(), home.clone()),
            ("GH_CONFIG_DIR".to_owned(), home),
            ("GH_NO_UPDATE_NOTIFIER".to_owned(), "1".to_owned()),
            ("GH_PROMPT_DISABLED".to_owned(), "1".to_owned()),
        ];
        if let Some(token) = self.token {
            env.push(("GH_TOKEN".to_owned(), token.to_owned()));
        }
        env
    }

    /// `curl` for page `page` of the repository's releases: no configuration file (`-q`),
    /// HTTPS only, no redirect, a time limit and a size limit, errors of HTTP failing the
    /// command. The command carries no environment of its own (the runner adds only its fixed
    /// `PATH`, `LANG` and `DEBIAN_FRONTEND`). The token, if any, is the only thing on its
    /// standard input.
    pub fn list_cmd(&self, page: u32) -> Cmd {
        let mut args: Vec<String> = [
            "-q",
            "--silent",
            "--show-error",
            "--fail",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--max-redirs",
            "0",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "--user-agent",
            "kbf-host-install",
        ]
        .map(str::to_owned)
        .to_vec();
        args.push("--max-time".to_owned());
        args.push(self.limits.tool.as_secs().max(1).to_string());
        args.push("--max-filesize".to_owned());
        args.push(MAX_PAGE_BYTES.to_string());
        let mut stdin = Vec::new();
        if let Some(token) = self.token {
            args.push("--config".to_owned());
            args.push("-".to_owned());
            stdin = format!("header = \"Authorization: Bearer {token}\"\n").into_bytes();
        }
        args.push("--url".to_owned());
        args.push(format!(
            "https://api.github.com/repos/{REPO}/releases?per_page={PER_PAGE}&page={page}"
        ));
        Cmd {
            program: self.curl.display().to_string(),
            args,
            timeout: Some(self.limits.tool),
            run_as: Some(self.who),
            stdin,
            stdout_max: Some(MAX_PAGE_BYTES),
            ..Cmd::default()
        }
    }

    /// `gh release download` of the tarball and the bundle of `commit` from the release
    /// `tag`, into `scratch`.
    pub fn download_cmd(&self, tag: &str, commit: &str, scratch: &Path) -> Cmd {
        let mut args: Vec<String> = ["release", "download", tag, "--repo", REPO]
            .map(str::to_owned)
            .to_vec();
        for name in [release::tarball_name(commit), release::bundle_name(commit)] {
            args.push("--pattern".to_owned());
            args.push(name);
        }
        args.push("--dir".to_owned());
        args.push(scratch.display().to_string());
        Cmd {
            program: self.gh.display().to_string(),
            args,
            timeout: Some(self.limits.download),
            env: self.env(),
            run_as: Some(self.who),
            write_cap: Some(WriteCap {
                dir: scratch.to_path_buf(),
                max: self
                    .limits
                    .tarball_max
                    .saturating_add(self.limits.bundle_max),
            }),
            ..Cmd::default()
        }
    }

    /// Pick the release of the commit that starts with `prefix` and download its tarball and
    /// bundle into `scratch`; the commit.
    ///
    /// # Errors
    /// [`FetchError`].
    pub fn fetch(&self, prefix: &str, scratch: &Path) -> Result<String, FetchError> {
        let mut found: BTreeMap<String, Found> = BTreeMap::new();
        let mut complete = false;
        for page in 1..=MAX_PAGES {
            let releases = self.list_page(page)?;
            note(&mut found, &releases, prefix);
            // Only an empty page ends the listing: a short one is not taken for the end.
            if releases.is_empty() {
                complete = true;
                break;
            }
        }
        let mut each = found.into_iter();
        let (commit, found) = match (each.next(), each.next()) {
            (None, _) if complete => return Err(FetchError::Unknown),
            (None, _) => {
                return failed(format!("the listing was cut at {MAX_PAGES} pages"));
            }
            (Some(one), None) => one,
            (Some(_), Some(_)) => return Err(FetchError::Ambiguous),
        };
        let Some(bundle_size) = found.bundle_size else {
            return failed("the release lists no attestation bundle for the tarball");
        };
        if found.tarball_size > self.limits.tarball_max || bundle_size > self.limits.bundle_max {
            return failed("a release file is larger than the limit");
        }
        let download = self.download_cmd(&found.tag, &commit, scratch);
        match self.runner.run(&download) {
            Err(e) => failed(format!("gh release download: {e}")),
            Ok(exec) if !exec.success() => failed(format!(
                "gh release download: exit status {:?}: {}",
                exec.code,
                exec.stderr_text()
            )),
            Ok(_) => Ok(commit),
        }
    }

    fn list_page(&self, page: u32) -> Result<Vec<Release>, FetchError> {
        let exec = match self.runner.run(&self.list_cmd(page)) {
            Ok(exec) => exec,
            Err(e) => return failed(format!("curl: {e}")),
        };
        if !exec.success() {
            return failed(format!(
                "curl: exit status {:?}: {}",
                exec.code,
                exec.stderr_text()
            ));
        }
        if exec.stdout.len() > MAX_PAGE_BYTES {
            return failed("a page of the listing is too large");
        }
        serde_json::from_slice(&exec.stdout)
            .or_else(|_| failed("the answer is not a listing of releases"))
    }
}

/// Note the tarballs of the commits that start with `prefix` in `releases`. The first
/// listing of a commit (the newest release) is the one kept.
fn note(found: &mut BTreeMap<String, Found>, releases: &[Release], prefix: &str) {
    for release in releases.iter().filter(|r| !r.draft && tag_ok(&r.tag_name)) {
        for asset in &release.assets {
            let Some(commit) = release::commit_of_name(&asset.name) else {
                continue;
            };
            if !commit.starts_with(prefix) {
                continue;
            }
            let bundle = release::bundle_name(commit);
            found.entry(commit.to_owned()).or_insert_with(|| Found {
                tag: release.tag_name.clone(),
                tarball_size: asset.size,
                bundle_size: release
                    .assets
                    .iter()
                    .find(|a| a.name == bundle)
                    .map(|a| a.size),
            });
        }
    }
}
