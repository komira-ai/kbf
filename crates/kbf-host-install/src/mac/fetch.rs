//! Fetching a release from the GitHub release of the repository, for a channel whose source
//! is `github`. Everything here runs through the pinned `gh`, as `nobody`, in a directory
//! `nobody` owns; what it leaves there is untrusted bytes until the root process has copied
//! them once and checked the copy ([`super::stage`]).
//!
//! 1. The releases are listed with the system `curl` from the public REST endpoint
//!    `https://api.github.com/repos/<repo>/releases` (the channel's repository), a page of 100 at a time until a page
//!    is empty, at most [`MAX_PAGES`] pages. No token is needed (the API allows 60 requests
//!    an hour per address without one; a runner or a NAT shared with others can have used
//!    them up, which is [`FetchError::RateLimited`]). Never only the latest: a rollback
//!    names an old release. Every page is read, because two commits that start with the
//!    version's 12 hex digits are an ambiguous release and only the whole listing shows it.
//! 2. A release asset named `kbf-<40 hex>-darwin-arm64.tar.gz` (`kbf` is the channel's
//!    artifact prefix) whose commit starts with the
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
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::release::{self, Origin};
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
    /// GitHub refused the listing for its rate limit; the text says when to retry and
    /// that a `token_file` raises the limit.
    RateLimited(String),
}

fn failed<T>(why: impl Into<String>) -> Result<T, FetchError> {
    Err(FetchError::Failed(why.into()))
}

/// What a fetch takes from a release: the file of the platform and the attestation bundle
/// that names it, and for a package or an image also the release's `SHA256SUMS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// `kbf-<commit>-<platform>.tar.gz`.
    Tarball,
    /// `kbf-<commit>-<platform>.deb`.
    Deb,
    /// `kbf-<commit>-<platform>.rpm`.
    Rpm,
    /// `kbf-frontend-image-<commit>-<platform>.oci.tar`, the frontend as a container image.
    FrontendImage,
}

impl AssetKind {
    /// The extension of the file's name. The image's name also has `frontend-image-` before
    /// the prefix and the commit, so take its name from [`Self::name_on`].
    pub fn extension(self) -> &'static str {
        match self {
            Self::Tarball => release::TARBALL_EXTENSION,
            Self::Deb => "deb",
            Self::Rpm => "rpm",
            Self::FrontendImage => "oci.tar",
        }
    }

    /// The name of the file of `commit` for `platform`, in a release whose files' names
    /// start with `prefix` (`kbf`).
    pub fn name_on(self, prefix: &str, platform: &str, commit: &str) -> String {
        match self {
            Self::FrontendImage => {
                format!("{prefix}-frontend-image-{commit}-{platform}.oci.tar")
            }
            _ => release::named_asset_on(prefix, platform, self.extension(), commit),
        }
    }

    /// The commit in the file name `name` of this kind for `platform`.
    pub fn commit_of_name_on<'a>(
        self,
        prefix: &str,
        platform: &str,
        name: &'a str,
    ) -> Option<&'a str> {
        match self {
            Self::FrontendImage => name
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix("-frontend-image-"))
                .and_then(|rest| rest.strip_suffix(&format!("-{platform}.oci.tar")))
                .filter(|commit| release::is_commit(commit)),
            _ => release::commit_of_named_asset_on(prefix, platform, self.extension(), name),
        }
    }

    /// Whether the release's checksum file is taken beside it.
    fn with_sums(self) -> bool {
        self != Self::Tarball
    }
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
/// as the home of `gh`. `token`, when there is one, is one line of letters, digits, `_`, `-`
/// and `.`, at most 1024 bytes.
pub struct Fetcher<'a> {
    /// The platform of the release files to take: `darwin-arm64`, `linux-x86_64`, ...
    pub platform: &'a str,
    /// Which of the platform's files to take.
    pub asset: AssetKind,
    /// The repository the releases are listed from and the start of their files' names.
    pub origin: &'a Origin,
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
    sums_size: Option<u64>,
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
    /// HTTPS only, no redirect, a time limit and a size limit, the status line and headers
    /// printed before the body (`--include`; an HTTP error does not fail the command, so
    /// that [`page_body`] can tell a rate limit from other errors). The command carries no environment of its own (the runner adds only its fixed
    /// `PATH`, `LANG` and `DEBIAN_FRONTEND`). The token, if any, is the only thing on its
    /// standard input.
    pub fn list_cmd(&self, page: u32) -> Cmd {
        let mut args: Vec<String> = [
            "-q",
            "--silent",
            "--show-error",
            "--include",
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
            "https://api.github.com/repos/{}/releases?per_page={PER_PAGE}&page={page}",
            self.origin.repository()
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

    /// `gh release download` of the file and the bundle of `commit` (and, for a package, the
    /// release's `SHA256SUMS`) from the release `tag`, into `scratch`.
    pub fn download_cmd(&self, tag: &str, commit: &str, scratch: &Path) -> Cmd {
        let mut args: Vec<String> = [
            "release",
            "download",
            tag,
            "--repo",
            self.origin.repository(),
        ]
        .map(str::to_owned)
        .to_vec();
        let file = self
            .asset
            .name_on(self.origin.prefix(), self.platform, commit);
        let mut names = vec![file.clone(), release::bundle_of(&file)];
        if self.asset.with_sums() {
            names.push(release::SUMS_NAME.to_owned());
        }
        for name in names {
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
                    .saturating_add(self.limits.bundle_max)
                    .saturating_add(if self.asset.with_sums() {
                        self.limits.bundle_max
                    } else {
                        0
                    }),
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
            note(
                &mut found,
                &releases,
                (self.origin.prefix(), self.platform, self.asset),
                prefix,
            );
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
            return failed("the release lists no attestation bundle for the file");
        };
        let sums_size = if self.asset.with_sums() {
            let Some(size) = found.sums_size else {
                return failed("the release lists no SHA256SUMS");
            };
            size
        } else {
            0
        };
        if found.tarball_size > self.limits.tarball_max
            || bundle_size > self.limits.bundle_max
            || sums_size > self.limits.bundle_max
        {
            return failed("a release file is larger than the limit");
        }
        let download = self.download_cmd(&found.tag, &commit, scratch);
        match self.runner.run(&download) {
            Err(e) => failed(format!("gh release download: {e}")),
            Ok(exec) if !exec.success() => failed(format!(
                "gh release download: {}: {}",
                exec.ended(),
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
            return failed(format!("curl: {}: {}", exec.ended(), exec.stderr_text()));
        }
        serde_json::from_slice(page_body(&exec.stdout)?)
            .or_else(|_| failed("the answer is not a listing of releases"))
    }
}

/// The most characters of GitHub's `message` that go into a reason.
const MESSAGE_MAX: usize = 200;

/// A response as `curl --include` prints it: the status, the headers (names in lower case)
/// and the body.
struct Response<'a> {
    status: u16,
    headers: Vec<(String, &'a str)>,
    body: &'a [u8],
}

impl Response<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }

    /// GitHub's `message` for an error, shortened.
    fn message(&self) -> Option<String> {
        let json: serde_json::Value = serde_json::from_slice(self.body).ok()?;
        let text = json.get("message")?.as_str()?;
        Some(text.chars().take(MESSAGE_MAX).collect())
    }
}

/// Split what `curl --include` printed. Interim `1xx` responses before the final one are
/// skipped. None when there is no complete status line and header block.
fn split_response(mut raw: &[u8]) -> Option<Response<'_>> {
    loop {
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = std::str::from_utf8(&raw[..end]).ok()?;
        let body = &raw[end + 4..];
        let mut lines = head.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let status = match (first.next(), first.next()) {
            (Some(version), Some(code)) if version.starts_with("HTTP/") => code.parse().ok()?,
            _ => return None,
        };
        if (100..200).contains(&status) {
            raw = body;
            continue;
        }
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim()))
            .collect();
        return Some(Response {
            status,
            headers,
            body,
        });
    }
}

/// The body of a page of the listing, from what `curl --include` printed for it.
///
/// # Errors
/// [`FetchError::RateLimited`] for GitHub's rate limit (a `403` or `429` that says so by
/// `x-ratelimit-remaining: 0`, a `retry-after` or its message), [`FetchError::Failed`] for
/// any other answer than `200`, and for output that is too large or not a response.
pub fn page_body(stdout: &[u8]) -> Result<&[u8], FetchError> {
    if stdout.len() > MAX_PAGE_BYTES {
        return failed("a page of the listing is too large");
    }
    let Some(response) = split_response(stdout) else {
        return failed("the answer is not an HTTP response");
    };
    if response.status == 200 {
        return Ok(response.body);
    }
    let message = response.message();
    let limited = matches!(response.status, 403 | 429)
        && (response.header("x-ratelimit-remaining") == Some("0")
            || response.header("retry-after").is_some()
            || message
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains("rate limit")));
    if limited {
        return Err(FetchError::RateLimited(format!(
            "rate limited by GitHub, retry after {} or set token_file",
            retry_after(&response)
        )));
    }
    match message {
        Some(m) => failed(format!("GitHub answered HTTP {}: {m}", response.status)),
        None => failed(format!("GitHub answered HTTP {}", response.status)),
    }
}

/// When a rate-limited listing may be tried again: the reset time the API names (RFC 3339,
/// UTC), else the wait it asks for, else a later time.
fn retry_after(response: &Response<'_>) -> String {
    let reset = response
        .header("x-ratelimit-reset")
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok())
        .and_then(|t| t.format(&Rfc3339).ok());
    if let Some(reset) = reset {
        return reset;
    }
    match response
        .header("retry-after")
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(seconds) => format!("{seconds} seconds"),
        None => "a while".to_owned(),
    }
}

/// Note the files of the commits that start with `prefix` in `releases`. The first
/// listing of a commit (the newest release) is the one kept.
fn note(
    found: &mut BTreeMap<String, Found>,
    releases: &[Release],
    (name_prefix, platform, asset_kind): (&str, &str, AssetKind),
    prefix: &str,
) {
    for release in releases.iter().filter(|r| !r.draft && tag_ok(&r.tag_name)) {
        for asset in &release.assets {
            let Some(commit) = asset_kind.commit_of_name_on(name_prefix, platform, &asset.name)
            else {
                continue;
            };
            if !commit.starts_with(prefix) {
                continue;
            }
            let bundle = release::bundle_of(&asset.name);
            let size_of = |name: &str| {
                release
                    .assets
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| a.size)
            };
            found.entry(commit.to_owned()).or_insert_with(|| Found {
                tag: release.tag_name.clone(),
                tarball_size: asset.size,
                bundle_size: size_of(&bundle),
                sums_size: size_of(release::SUMS_NAME),
            });
        }
    }
}
