//! The frontend's local cache of the kbf base tree bundle of its own release
//! (docs/deploy/job-service-cutover.md, "The kbf base tree"): the bundle is fetched by its
//! release's digest the first time a frontend of that release starts on a host, and kept.
//!
//! [`Cache::ensure`] answers the directory [`kbf_front::base_publish::publish`] then reads,
//! one that holds the `SHA256SUMS` and the bundle of the commit this binary was built from:
//!
//! 1. `<cache>/<commit>/` holds both: the cache has it, nothing is fetched;
//! 2. `<cache>` itself holds both: an operator staged them there by hand, as every release
//!    before this one asked, and that is used as it was (nothing is fetched, nothing moved);
//! 3. otherwise, with `--executor-base-tree-url` not empty, the release is looked up in the
//!    releases list the URL gives (GitHub's `GET /repos/<owner>/<repo>/releases` shape: an array
//!    of releases, each with `assets` of `name` and `browser_download_url`) by the bundle's
//!    file name, which holds the commit. The release's `SHA256SUMS` and the bundle are downloaded
//!    to `<cache>/.fetch-<commit>/`, the bundle is hashed against the sums file there
//!    ([`kbf_front::base_publish::verify`]), and only then is the directory renamed to
//!    `<cache>/<commit>/`. A download that stops, or hashes to another digest, is removed with
//!    its directory, so a partial file is never in a place anything reads.
//!
//! After a fetch the cache keeps this release and the `--executor-base-tree-keep` newest others
//! (a rollback to one of them fetches nothing) and removes the rest; a failure to remove is
//! logged, not an error. The digest the bundle is checked against is the one `SHA256SUMS` of
//! the same release lists, over https from the release's own page: the same file an operator
//! used to copy by hand.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kbf_front::base_publish::{self, PublishError, SUMS};
use kbf_image::{Downloader, Timeouts};

/// The most one page of the releases list may be, in bytes: a bound on what a wrong URL can
/// make the frontend read into memory.
const INDEX_PAGE_LIMIT: u64 = 64 << 20;

/// The releases the list gives per page (GitHub's most).
const PER_PAGE: u32 = 100;

/// The prefix of the directory a download is made in.
const STAGING: &str = ".fetch-";

/// Where a bundle came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// `<cache>/<commit>/` held it already.
    Cached,
    /// `<cache>` held it, put there by hand.
    Staged,
    /// Fetched now.
    Fetched,
}

/// Why the bundle is not in the cache.
#[derive(Debug, thiserror::Error)]
pub(super) enum FetchError {
    /// A file or directory of the cache could not be made, written or renamed.
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    /// A download failed.
    #[error("{url}: {source}")]
    Download {
        url: String,
        source: kbf_image::Error,
    },
    /// The releases list is not one.
    #[error("{url}: not a list of releases: {why}")]
    Index { url: String, why: String },
    /// No release of the list holds the bundle and the sums file.
    #[error("{url}: no release lists both {name} and {SUMS}")]
    NotReleased { url: String, name: String },
    /// The downloaded bundle is not the one the release's sums file lists.
    #[error("the downloaded release files are refused: {0}")]
    Refused(PublishError),
    /// A blocking task did not finish.
    #[error("a task of the fetch stopped: {0}")]
    Task(String),
}

/// The cache of `--executor-base-tree` and how it is filled.
#[derive(Clone, Debug)]
pub(super) struct Cache {
    /// `--executor-base-tree`.
    pub(super) dir: PathBuf,
    /// `--executor-base-tree-url`: the releases list; empty fetches nothing.
    pub(super) url: String,
    /// `--executor-base-tree-keep`.
    pub(super) keep: usize,
    /// `--executor-base-tree-timeout-secs`: each connect, response head and body stall.
    pub(super) timeout: Duration,
}

impl Cache {
    /// The directory that holds the `SHA256SUMS` and the bundle of `commit` for `platform`,
    /// fetching them if the cache has neither way of holding them.
    ///
    /// # Errors
    /// A [`FetchError`]: the bundle is not held and could not be fetched.
    pub(super) async fn ensure(
        &self,
        commit: &str,
        platform: &str,
    ) -> Result<(PathBuf, Origin), FetchError> {
        // A commit that cannot name a directory (none, or a build that does not know its own) is
        // not looked up or fetched: the publish names what the directory lacks.
        let named = is_commit(commit);
        let release = self.dir.join(commit);
        if named && base_publish::is_staged(&release, commit, platform) {
            return Ok((release, Origin::Cached));
        }
        if !named || base_publish::is_staged(&self.dir, commit, platform) || self.url.is_empty() {
            return Ok((self.dir.clone(), Origin::Staged));
        }
        let name = base_publish::bundle_name(commit, platform);
        self.fetch(commit, platform, &name).await?;
        self.prune(commit).await;
        Ok((release, Origin::Fetched))
    }

    /// Downloads the release's files into `<cache>/.fetch-<commit>/`, checks them and renames
    /// the directory to `<cache>/<commit>/`.
    async fn fetch(&self, commit: &str, platform: &str, name: &str) -> Result<(), FetchError> {
        let (bundle_url, sums_url) = self.locate(name).await?;
        let staging = self.dir.join(format!("{STAGING}{commit}"));
        let release = self.dir.join(commit);
        let downloaded = self
            .download(&staging, &[(&sums_url, SUMS), (&bundle_url, name)])
            .await;
        let checked = match downloaded {
            Ok(()) => check(&staging, commit, platform).await,
            Err(why) => Err(why),
        };
        if let Err(why) = checked {
            // What was fetched is of no use: the next start fetches again.
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(why);
        }
        // A directory in the way is a release that failed its own check.
        clear(&release).await?;
        tokio::fs::rename(&staging, &release)
            .await
            .map_err(io(&release))?;
        tracing::info!(
            commit,
            platform,
            dir = %release.display(),
            "fetched the kbf base tree bundle into the cache"
        );
        Ok(())
    }

    /// Makes `staging` new and downloads each `(url, file name)` into it.
    async fn download(&self, staging: &Path, files: &[(&str, &str)]) -> Result<(), FetchError> {
        // What an earlier start that stopped left.
        clear(staging).await?;
        tokio::fs::create_dir_all(staging)
            .await
            .map_err(io(staging))?;
        let downloader = self.downloader();
        for (url, file) in files {
            downloader
                .file(url, &staging.join(file))
                .await
                .map_err(|source| FetchError::Download {
                    url: (*url).to_owned(),
                    source,
                })?;
        }
        Ok(())
    }

    fn downloader(&self) -> Downloader {
        Downloader::new(Timeouts {
            connect: self.timeout,
            head: self.timeout,
            idle: self.timeout,
        })
    }

    /// The download URLs of the bundle `name` and the `SHA256SUMS` of the release that has it,
    /// from the releases list, a page at a time, the newest releases first.
    async fn locate(&self, name: &str) -> Result<(String, String), FetchError> {
        let downloader = self.downloader();
        let joiner = if self.url.contains('?') { '&' } else { '?' };
        for page in 1_u32.. {
            let url = format!("{}{joiner}per_page={PER_PAGE}&page={page}", self.url);
            let body = downloader
                .bytes(&url, INDEX_PAGE_LIMIT)
                .await
                .map_err(|source| FetchError::Download {
                    url: url.clone(),
                    source,
                })?;
            let releases: serde_json::Value =
                serde_json::from_slice(&body).map_err(|e| FetchError::Index {
                    url: url.clone(),
                    why: e.to_string(),
                })?;
            let Some(releases) = releases.as_array() else {
                return Err(FetchError::Index {
                    url,
                    why: "not an array".to_owned(),
                });
            };
            if releases.is_empty() {
                break;
            }
            for release in releases {
                let asset = |wanted: &str| asset_url(release, wanted);
                if let (Some(bundle), Some(sums)) = (asset(name), asset(SUMS)) {
                    return Ok((bundle, sums));
                }
            }
        }
        Err(FetchError::NotReleased {
            url: self.url.clone(),
            name: name.to_owned(),
        })
    }

    /// Removes the releases of the cache but `commit` and the `keep` newest others, and any
    /// download a stopped start left. A failure is logged: the cache is only larger.
    async fn prune(&self, commit: &str) {
        let dir = self.dir.clone();
        let (keep, commit) = (self.keep, commit.to_owned());
        let pruned = tokio::task::spawn_blocking(move || prune(&dir, &commit, keep)).await;
        match pruned {
            Ok(Ok(removed)) => {
                for path in removed {
                    tracing::info!(path = %path.display(), "removed an older base tree bundle from the cache");
                }
            }
            Ok(Err(path_error)) => {
                tracing::warn!(%path_error, "the base tree cache was not pruned")
            }
            Err(stopped) => tracing::warn!(%stopped, "the base tree cache was not pruned"),
        }
    }
}

/// The error of a failed file operation on `path`.
fn io(path: &Path) -> impl FnOnce(io::Error) -> FetchError {
    let path = path.to_owned();
    move |source| FetchError::Io { path, source }
}

/// Removes `path` and what is in it; a `path` that is not there is as removed.
async fn clear(path: &Path) -> Result<(), FetchError> {
    match tokio::fs::remove_dir_all(path).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io(path)(e)),
        _ => Ok(()),
    }
}

/// The `browser_download_url` of the asset `wanted` of `release`, if it has one.
fn asset_url(release: &serde_json::Value, wanted: &str) -> Option<String> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(wanted))?
        .get("browser_download_url")?
        .as_str()
        .map(str::to_owned)
}

/// Checks the downloaded release files in `dir`: the bundle hashes to the listed digest.
async fn check(dir: &Path, commit: &str, platform: &str) -> Result<(), FetchError> {
    let (dir, commit, platform) = (dir.to_owned(), commit.to_owned(), platform.to_owned());
    match tokio::task::spawn_blocking(move || base_publish::verify(&dir, &commit, &platform)).await
    {
        Ok(checked) => checked.map_err(FetchError::Refused),
        Err(stopped) => Err(FetchError::Task(stopped.to_string())),
    }
}

/// Whether `text` is a full commit: 40 lowercase hex digits.
fn is_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Removes from `dir` the commit directories but `commit` and the `keep` newest others (by the
/// time each was last changed), and the downloads of [`STAGING`]. Gives what it removed.
///
/// # Errors
/// The first path that could not be read or removed, with what failed.
fn prune(dir: &Path, commit: &str, keep: usize) -> Result<Vec<PathBuf>, String> {
    let failed = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    let mut others: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut removable: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| failed(dir, e))? {
        let entry = entry.map_err(|e| failed(dir, e))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(STAGING) {
            removable.push(path);
        } else if name != commit && is_commit(&name) && path.is_dir() {
            let changed = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map_err(|e| failed(&path, e))?;
            others.push((changed, path));
        }
    }
    others.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    removable.extend(others.into_iter().skip(keep).map(|(_, path)| path));
    for path in &removable {
        std::fs::remove_dir_all(path).map_err(|e| failed(path, e))?;
    }
    Ok(removable)
}

#[cfg(test)]
#[path = "base_fetch_tests.rs"]
pub(in crate::frontend) mod tests;
