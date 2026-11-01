//! What one run is told: the pack key it may write, and the command line of the task tool.
//!
//! The pack url and the read token reach the task one of two ways. [`Delivery::Files`] puts
//! each in a file of the run (the lease server mounts them read-only at `/run/js-files`) and
//! gives the tool the paths: no argument holds a secret. [`Delivery::InsecureArgv`], the
//! default, puts them in the arguments, where the action can read them (`/proc/1/cmdline`)
//! and the engine's inspect shows them, and the executor refuses to run with it unless told to:
//! [`Config::validate`] fails unless `insecure_argv_secrets` is set. There is no fall back from
//! the first to the second. Nothing here prints a secret: [`Secret`] formats as
//! `<redacted>`, and so does a [`TaskArgs`].
//!
//! [`Config::validate`]: crate::executor::Config::validate

use std::fmt;
use std::time::Duration;

use kbf_lease_client::RunFile;
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use ring::rand::{SecureRandom, SystemRandom};

/// A value that must not reach a log or an error: a presigned url, a bearer token.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one place that sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// 128 random bits, drawn for every `POST` of a run and never reused: not per process, not per
/// attempt. It labels the run, so that a run whose `202` was lost can be found again in the
/// lease server's list. (The pack key is not drawn here: the frontend's ingest issues it,
/// see [`PackKey`].)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nonce(u128);

impl Nonce {
    /// A nonce from the operating system's random source, which cannot fail in a way a
    /// caller could repair: a failure stops the process rather than reuse a label.
    #[must_use]
    pub fn random() -> Self {
        let mut bytes = [0_u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .expect("the operating system's random source failed");
        Self(u128::from_be_bytes(bytes))
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// The build id of a farm lease, in a pack key and in the task's `--lease`: `<term>-<seq>`.
/// The farm's own spelling, `<term>.<seq>`, holds a dot, which a build id may not
/// (`kbf_pack::check_build` allows `0-9`, `a-z`, `A-Z`, `-` and `_`): a pack that names it is
/// refused by the ingest, and a key built from it is refused by the mint.
#[must_use]
pub fn build_id(lease: LeaseId) -> String {
    format!("{}-{}", lease.term, lease.seq)
}

/// The one object key a task may write: `cas/b/<build>/<nonce>-<slot>`, issued by the
/// frontend's ingest ([`Ingest::mint`](crate::executor::Ingest::mint)) for one farm lease of
/// one action and verified by the same ingest when the pack is read. The executor does not
/// look inside it.
///
/// A farm lease is run at most once: a worker that cannot tell whether a run was queued is
/// taken down and the farm rules the action again as a new farm lease, so the key of a lease
/// is never handed to a second run, and a zombie of an earlier run, which holds the url of
/// its own key, never meets the create-only refusal of a key it did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackKey(String);

impl PackKey {
    /// The object key `key`.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The object key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the lease server mounts the files of a run, read-only.
const FILES_DIR: &str = "/run/js-files";
const PACK_URL_FILE: &str = "pack-url";
const GRANT_TOKEN_FILE: &str = "grant-token";

/// How the secrets reach the task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// As arguments, behind the tool's `--insecure-argv-secrets`: the default, for a lease
    /// server whose agents do not report `run-files`.
    InsecureArgv,
    /// As files of the run, which the lease server mounts read-only at `/run/js-files`; the
    /// tool is given the paths.
    Files,
}

/// The command line of one task: what the task tool is given.
pub struct TaskArgs<'a> {
    /// The action to run.
    pub action: &'a Digest,
    /// The farm lease the run belongs to.
    pub lease: LeaseId,
    /// The base url of the read endpoint.
    pub grant_url: &'a str,
    /// The read token.
    pub grant_token: &'a Secret,
    /// The presigned `PUT` url of the pack.
    pub pack_url: &'a Secret,
    /// How long the tool lets the action run.
    pub deadline: Duration,
    /// Where the tool works, if not where it would: a writable directory of the kbf image.
    pub work_dir: Option<&'a str>,
    /// Whether the `PUT` goes over plain http (until the store serves TLS).
    pub allow_plain_http_put: bool,
    /// Whether the read grant may be served over plain http: the tool is told with
    /// `--allow-plain-http-grant`.
    pub allow_plain_http_grant: bool,
    /// Whether the task asks the listener where its inputs are kept and reads them from there:
    /// the tool is told with `--locate`.
    pub locate: bool,
}

impl TaskArgs<'_> {
    /// The flags of the task tool, for [`Delivery::InsecureArgv`]. The tool is the kbf image's
    /// `ENTRYPOINT`, which the lease server prepends the arguments to: no path comes first.
    #[must_use]
    pub fn argv(&self, delivery: Delivery) -> Vec<String> {
        let mut argv = vec![
            "--action".to_owned(),
            format!("{}/{}", self.action.hash, self.action.size_bytes),
            "--lease".to_owned(),
            build_id(self.lease),
            "--grant-url".to_owned(),
            self.grant_url.to_owned(),
            "--deadline-ms".to_owned(),
            self.deadline.as_millis().to_string(),
        ];
        match delivery {
            Delivery::InsecureArgv => argv.extend([
                "--insecure-argv-secrets".to_owned(),
                "--pack-url".to_owned(),
                self.pack_url.expose().to_owned(),
                "--grant-token".to_owned(),
                self.grant_token.expose().to_owned(),
            ]),
            Delivery::Files => argv.extend([
                "--pack-url-file".to_owned(),
                format!("{FILES_DIR}/{PACK_URL_FILE}"),
                "--grant-token-file".to_owned(),
                format!("{FILES_DIR}/{GRANT_TOKEN_FILE}"),
            ]),
        }
        if let Some(dir) = self.work_dir {
            argv.extend(["--work-dir".to_owned(), dir.to_owned()]);
        }
        if self.allow_plain_http_put {
            argv.push("--allow-plain-http-put".to_owned());
        }
        if self.allow_plain_http_grant {
            argv.push("--allow-plain-http-grant".to_owned());
        }
        if self.locate {
            argv.push("--locate".to_owned());
        }
        argv
    }
}

impl TaskArgs<'_> {
    /// The files of the run for `delivery`: none for [`Delivery::InsecureArgv`], and for
    /// [`Delivery::Files`] the presigned url and the read token, each alone in a file the
    /// arguments of [`TaskArgs::argv`] name.
    #[must_use]
    pub fn files(&self, delivery: Delivery) -> Vec<RunFile> {
        match delivery {
            Delivery::InsecureArgv => Vec::new(),
            Delivery::Files => vec![
                RunFile {
                    name: PACK_URL_FILE.to_owned(),
                    contents: self.pack_url.expose().to_owned(),
                },
                RunFile {
                    name: GRANT_TOKEN_FILE.to_owned(),
                    contents: self.grant_token.expose().to_owned(),
                },
            ],
        }
    }
}

impl fmt::Debug for TaskArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskArgs")
            .field("action", &format_args!("{}", self.action.hash))
            .field("lease", &self.lease)
            .field("grant_url", &self.grant_url)
            .field("grant_token", &self.grant_token)
            .field("pack_url", &self.pack_url)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
