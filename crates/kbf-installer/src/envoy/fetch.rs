//! How the Envoy bytes arrive. A [`Fetcher`] writes the file at a URL to a path in a
//! directory only the fetch user can write; the parent then opens it without following a
//! link, copies it by descriptor and hashes the copy (docs/design/kbf-installer.md,
//! section 5), so what a fetcher left is never believed, whoever ran it.
//!
//! The installer's own fetch child (a separate change) is meant to implement the same
//! trait. [`CurlFetcher`] is the minimal one until then: `curl`, run as the fetch user
//! with an empty environment, https only (also after a redirect, at most 3 of them), no
//! proxy variables read, no `.curlrc`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::run::{RunAs, Runner, Spawn};

/// Why the bytes could not be fetched.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("cannot run {}: {source}", program.display())]
    Start {
        program: PathBuf,
        source: std::io::Error,
    },
    #[error("the fetch ran out of time and was killed")]
    TimedOut,
    #[error("the fetch failed ({status}): {detail}")]
    Failed { status: String, detail: String },
}

pub trait Fetcher {
    /// Write the body at `url` (an `https://` URL) to the new file `dest`, at most `cap`
    /// bytes of it. `dest` is in a directory the fetcher may write.
    fn fetch(&self, url: &str, dest: &Path, cap: u64) -> Result<(), FetchError>;
}

/// Fetch with `curl`, run through a [`Runner`].
pub struct CurlFetcher<'a> {
    pub runner: &'a dyn Runner,
    /// Absolute path of curl; [`CurlFetcher::CURL`] on a host.
    pub curl: PathBuf,
    /// The user the download runs as: the fetch user.
    pub run_as: Option<RunAs>,
    pub timeout: Duration,
}

impl CurlFetcher<'_> {
    pub const CURL: &'static str = "/usr/bin/curl";

    /// The program to run for `url`, written to `dest`.
    pub fn spawn(&self, url: &str, dest: &Path, cap: u64) -> Spawn {
        let args = [
            // No `.curlrc`: it must be the first argument.
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-redirs",
            "3",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            // Not `http_proxy` and the like: the environment is empty, and this says so.
            "--noproxy",
            "*",
            "--max-filesize",
            &cap.to_string(),
            "--output",
        ];
        let mut args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        args.push(dest.into());
        args.push(url.into());
        Spawn {
            program: self.curl.clone(),
            args,
            env: Vec::new(),
            cwd: dest.parent().unwrap_or(Path::new("/")).to_owned(),
            run_as: self.run_as,
            timeout: self.timeout,
        }
    }
}

impl Fetcher for CurlFetcher<'_> {
    fn fetch(&self, url: &str, dest: &Path, cap: u64) -> Result<(), FetchError> {
        let ran = self
            .runner
            .run(&self.spawn(url, dest, cap))
            .map_err(|source| FetchError::Start {
                program: self.curl.clone(),
                source,
            })?;
        if ran.timed_out {
            return Err(FetchError::TimedOut);
        }
        if ran.code == Some(0) {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&ran.stderr);
        Err(FetchError::Failed {
            status: ran
                .code
                .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
            detail: stderr.lines().next().unwrap_or("").to_owned(),
        })
    }
}
