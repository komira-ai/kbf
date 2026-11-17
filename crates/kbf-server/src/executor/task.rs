//! What one job is told: the pack key it may write, and the command line of the task tool.
//!
//! The pack url and the read token reach the task one of two ways. [`Delivery::Files`] puts
//! each in a file of the job (its host writes them before the job starts, read-only at
//! `/run/js-files`) and gives the tool the paths: no argument holds a secret.
//! [`Delivery::InsecureArgv`], the default, puts them in the arguments, where the action can
//! read them (`/proc/1/cmdline`)
//! and the engine's inspect shows them, and the executor refuses to run with it unless told to:
//! [`Config::validate`] fails unless `insecure_argv_secrets` is set. There is no fall back from
//! the first to the second. Nothing here prints a secret: [`Secret`] formats as
//! `<redacted>`, and so does a [`TaskArgs`].
//!
//! [`Config::validate`]: crate::executor::Config::validate

use std::fmt;
use std::time::Duration;

use js_proto::v1::JobFile;
use kbf_proto::reapi::Digest;
use kbf_types::OperationId;

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

/// One run of an operation: the operation's id and the attempt its job is, counted from 0. An
/// operation is run again only as its next attempt, so a run is one job ([`job_id`]) and one
/// pack key, and a zombie of an earlier attempt never writes under a later one's key.
///
/// [`job_id`]: crate::executor::job_id
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId {
    /// The operation (`operations/<id>`).
    pub operation: OperationId,
    /// Which run of it, from 0.
    pub attempt: u32,
}

impl RunId {
    /// The run `attempt` of `operation`.
    #[must_use]
    pub const fn new(operation: OperationId, attempt: u32) -> Self {
        Self { operation, attempt }
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.operation.0, self.attempt)
    }
}

/// The build id of a run, in a pack key and in the task's `--lease`: `<operation>-<attempt>`,
/// which `kbf_pack::check_build` takes (`0-9`, `a-z`, `A-Z`, `-` and `_`).
#[must_use]
pub fn build_id(run: RunId) -> String {
    run.to_string()
}

/// The one object key a task may write, `cas/b/<build>/<nonce>-<slot>`, or the put directory it
/// commits its objects into, `cas/b/<build>/<nonce>`: issued by the frontend's ingest
/// ([`Ingest::mint`](crate::executor::Ingest::mint)) for one run of one action and verified by
/// the same ingest when the run is read. The executor does not look inside it.
///
/// A run is one job: its id is made from the run ([`job_id`](crate::executor::job_id)), a
/// submit whose answer was lost is sent again with the same id and makes no second job, and an
/// operation is run again only as its next attempt. So the key of a run is never handed to a
/// second job, and a zombie of an earlier job, which holds the url of its own key, never meets
/// the create-only refusal of a key it did not write.
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

/// Where the host of a job writes its files, read-only.
const FILES_DIR: &str = "/run/js-files";
const PACK_URL_FILE: &str = "pack-url";
const GRANT_TOKEN_FILE: &str = "grant-token";

/// How the secrets reach the task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// As arguments, behind the tool's `--insecure-argv-secrets`: the default, for hosts
    /// whose agents cannot write the files of a job.
    InsecureArgv,
    /// As files of the job, which its host writes read-only at `/run/js-files`; the tool is
    /// given the paths.
    Files,
}

/// The command line of one task: what the task tool is given.
pub struct TaskArgs<'a> {
    /// The action to run.
    pub action: &'a Digest,
    /// The run the job is.
    pub run: RunId,
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
    /// The flags of the task tool, the arguments of the job. The tool is the kbf image's
    /// `ENTRYPOINT`, which the job's command is left empty to keep: no path comes first.
    #[must_use]
    pub fn argv(&self, delivery: Delivery) -> Vec<String> {
        let mut argv = vec![
            "--action".to_owned(),
            format!("{}/{}", self.action.hash, self.action.size_bytes),
            "--lease".to_owned(),
            build_id(self.run),
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

/// The flags of the task tool for `run` of `action` that commits into a put
/// directory: the inputs are the tree at `/` (`--tree`), the outputs are committed into
/// [`OUT_TARGET`](crate::executor::OUT_TARGET) (`--stage-and-commit`, `--out-dir`), and the action
/// may run for `deadline`. The task is also told to write one object (`--object-format one-pass`),
/// the only format it writes: a task of a release that still has two formats needs the words, and
/// one that has only this one accepts them. No url, no grant and no secret: the job has no
/// network.
#[must_use]
pub fn put_dir_argv(action: &Digest, run: RunId, deadline: Duration) -> Vec<String> {
    vec![
        "--tree".to_owned(),
        "--stage-and-commit".to_owned(),
        "--out-dir".to_owned(),
        crate::executor::OUT_TARGET.to_owned(),
        "--action".to_owned(),
        format!("{}/{}", action.hash, action.size_bytes),
        "--lease".to_owned(),
        build_id(run),
        "--deadline-ms".to_owned(),
        deadline.as_millis().to_string(),
        "--object-format".to_owned(),
        "one-pass".to_owned(),
    ]
}

impl TaskArgs<'_> {
    /// The files of the job for `delivery`: none for [`Delivery::InsecureArgv`], and for
    /// [`Delivery::Files`] the presigned url and the read token, each alone in a file the
    /// arguments of [`TaskArgs::argv`] name.
    #[must_use]
    pub fn files(&self, delivery: Delivery) -> Vec<JobFile> {
        match delivery {
            Delivery::InsecureArgv => Vec::new(),
            Delivery::Files => vec![
                JobFile {
                    name: PACK_URL_FILE.to_owned(),
                    contents: self.pack_url.expose().to_owned(),
                },
                JobFile {
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
            .field("run", &self.run)
            .field("grant_url", &self.grant_url)
            .field("grant_token", &self.grant_token)
            .field("pack_url", &self.pack_url)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
