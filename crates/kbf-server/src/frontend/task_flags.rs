//! The flags that let the frontend prepare the runs of a host executor: `--task-secret-file`
//! and its companions, and the [`RunPrep`] they open over the frontend's [`Backend`].
//!
//! A [`RunPrep`] (`kbf_front::prepare_run`) hands the executor, for each run, the key a task
//! may write, one presigned write of it to the tier store, and a read grant for the task's
//! inputs. These flags give it the secrets and the places it needs. All are off unless
//! `--task-secret-file` is given, and then a frontend serves exactly as it did: nothing is
//! read from the files until the frontend starts, and nothing is sent anywhere. The switch
//! needs `--task-put-endpoint`, `--task-put-bucket`, `--task-put-credentials-file` and
//! `--task-read-url`, which clap enforces (so does the file of `kbf.toml`, which becomes
//! flags). The companions that have a default (`--task-put-region`, `--task-run-wall-secs`,
//! `--task-url-slack-secs`, `--task-closure-max-digests`, `--task-closure-max-bytes`) need
//! nothing, since the config template writes every flag with a default as a live line.
//!
//! The secrets are files and never flags, as a command line is world-readable. The secret file
//! holds one secret of at least 32 bytes (surrounding white space is dropped), owned by the
//! server's user and readable by it alone: the mint of the pack keys and the key of the read
//! grants are both made from it, each under a domain of its own, and it is the secret the
//! job-facing listener verifies grants with. The credentials file is the JSON of a presign-only
//! key of the tier store, `{"access_key_id": "...", "secret_access_key": "..."}` (a
//! `--presign-key-file` of the tier store works too: its ceiling is read by the store and
//! ignored here), with the same owner and mode rules.

#![allow(dead_code, unused_imports)] // red: the stubs below read no file

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Arg, ArgMatches};
use kbf_front::grants::{GrantKey, KeyError};
use kbf_front::prepare_run::{
    AccessKey, ClosureLimits, ConfigError, PutTarget, RunPrep, RunPrepConfig, check_read_url,
};
use kbf_front::{Backend, MintError, PackKeyMint};
use zeroize::Zeroizing;

use crate::token::{TokenFileError, read_owner_only};

const SECRET: &str = "task_secret_file";
const PUT_ENDPOINT: &str = "task_put_endpoint";
const PUT_BUCKET: &str = "task_put_bucket";
const PUT_REGION: &str = "task_put_region";
const PUT_CREDENTIALS: &str = "task_put_credentials_file";
const READ_URL: &str = "task_read_url";
const RUN_WALL: &str = "task_run_wall_secs";
const URL_SLACK: &str = "task_url_slack_secs";
const MAX_DIGESTS: &str = "task_closure_max_digests";
const MAX_BYTES: &str = "task_closure_max_bytes";

/// The signing region when `--task-put-region` is left out: the one S3-compatible stores accept
/// unless they are configured to another.
const DEFAULT_REGION: &str = "us-east-1";

/// The run wall when `--task-run-wall-secs` is left out: 2 hours.
const DEFAULT_RUN_WALL_SECS: u64 = 7_200;

/// The slack when `--task-url-slack-secs` is left out: 10 minutes.
const DEFAULT_URL_SLACK_SECS: u64 = 600;

/// The longest wall or slack a flag takes, in seconds: the 4 hour ceiling of a presigned write
/// ([`kbf_front::prepare_run::MAX_EXPIRY`]).
const MAX_SECS: u64 = kbf_front::prepare_run::MAX_EXPIRY.as_secs();

/// The most bytes a secret file or a credentials file may hold.
const MAX_FILE_BYTES: usize = 4096;

/// The ten flags.
pub(super) fn args() -> [Arg; 10] {
    [
        Arg::new(SECRET)
            .long("task-secret-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires_all([PUT_ENDPOINT, PUT_BUCKET, PUT_CREDENTIALS, READ_URL])
            .help(
                "Turns on the preparation of the runs of a host executor: for each run the \
                 frontend makes the key the task may write, one presigned write of it to the \
                 tier store, and a read grant for the task's inputs. The file holds one \
                 secret of at least 32 bytes (surrounding white space is dropped), owned by \
                 this user and readable by it alone (mode 0600 or 0400): the pack-key mint and \
                 the read-grant key are made from it, and it is the secret the job-facing \
                 listener verifies grants with. The secret is never a flag: a command line is \
                 world-readable. Needs `--task-put-endpoint`, `--task-put-bucket`, \
                 `--task-put-credentials-file` and `--task-read-url`. Left out, no run is \
                 prepared",
            ),
        Arg::new(PUT_ENDPOINT)
            .long("task-put-endpoint")
            .value_name("URL")
            .value_parser(clap::value_parser!(String))
            .requires(SECRET)
            .help(
                "The tier store's proxy, as `http://host:port` or `https://host:port`: the \
                 host a task's presigned write is signed for, and where it sends the pack. \
                 Used only with `--task-secret-file`",
            ),
        Arg::new(PUT_BUCKET)
            .long("task-put-bucket")
            .value_name("BUCKET")
            .value_parser(clap::value_parser!(String))
            .requires(SECRET)
            .help(
                "The bucket of `--task-put-endpoint` a task's pack is written to, under the \
                 key `cas/b/<lease>/<nonce>-0`",
            ),
        Arg::new(PUT_REGION)
            .long("task-put-region")
            .value_name("REGION")
            .value_parser(clap::value_parser!(String))
            .default_value(DEFAULT_REGION)
            .help("The region the presigned writes to `--task-put-endpoint` are signed for"),
        Arg::new(PUT_CREDENTIALS)
            .long("task-put-credentials-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(SECRET)
            .help(
                "A file with the presign-only key of the tier store, as JSON \
                 `{\"access_key_id\": \"...\", \"secret_access_key\": \"...\"}`, owned by this \
                 user and readable by it alone (mode 0600 or 0400). The key signs a `PUT` of one \
                 object under its prefix and does nothing else; its ceiling (`max_expires_secs` \
                 of the store's `--presign-key-file`) must be at least 14400, and the store's \
                 `--presign-max-expires` too",
            ),
        Arg::new(READ_URL)
            .long("task-read-url")
            .value_name("URL")
            .value_parser(clap::value_parser!(String))
            .requires(SECRET)
            .help(
                "The base URL a task reads its inputs from: the job-facing read listener, as \
                 `https://host:port` with no path (a grant's token is a bearer secret and \
                 crosses only a TLS connection; `http://` is refused at start). A blob is \
                 `GET <url>/<hash>/<size>`",
            ),
        Arg::new(RUN_WALL)
            .long("task-run-wall-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_SECS))
            // DEFAULT_RUN_WALL_SECS, which clap cannot take as a computed string; the
            // default-flags test parses it back and compares it with the constant.
            .default_value("7200")
            .help(
                "The run wall limit a run is prepared for, in seconds, at least 1 and at most \
                 14400: a task's presigned write and read grant are good for this plus \
                 `--task-url-slack-secs`. Used only with `--task-secret-file`",
            ),
        Arg::new(URL_SLACK)
            .long("task-url-slack-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(0..=MAX_SECS))
            // DEFAULT_URL_SLACK_SECS, as above.
            .default_value("600")
            .help(
                "The time past the run wall, in seconds, that a task's presigned write and read \
                 grant stay good: the time a task takes to start and to put its pack. The wall \
                 and the slack together must not pass 14400, the ceiling of a presigned write, \
                 or the frontend does not start. Used only with `--task-secret-file`",
            ),
        Arg::new(MAX_DIGESTS)
            .long("task-closure-max-digests")
            .value_name("DIGESTS")
            .value_parser(clap::value_parser!(u32).range(1..))
            // ClosureLimits::DEFAULT.max_digests, as above.
            .default_value("500000")
            .help(
                "The most distinct digests (directories and files) the input tree of a run may \
                 have: a run whose tree has more is refused, never run on a part of its \
                 inputs. Used only with `--task-secret-file`",
            ),
        Arg::new(MAX_BYTES)
            .long("task-closure-max-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            // ClosureLimits::DEFAULT.max_bytes, as above.
            .default_value("34359738368")
            .help(
                "The most bytes of blobs, by their digests' sizes, the input tree of a run may \
                 have (each distinct blob once): a run whose tree has more is refused. Used only \
                 with `--task-secret-file`",
            ),
    ]
}

/// What the task flags say, when `--task-secret-file` is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFlags {
    /// `--task-secret-file`.
    pub secret_file: PathBuf,
    /// `--task-put-endpoint`.
    pub put_endpoint: String,
    /// `--task-put-bucket`.
    pub put_bucket: String,
    /// `--task-put-region`.
    pub put_region: String,
    /// `--task-put-credentials-file`.
    pub put_credentials_file: PathBuf,
    /// `--task-read-url`.
    pub read_url: String,
    /// `--task-run-wall-secs`.
    pub run_wall: Duration,
    /// `--task-url-slack-secs`.
    pub url_slack: Duration,
    /// `--task-closure-max-digests`.
    pub max_digests: usize,
    /// `--task-closure-max-bytes`.
    pub max_bytes: u64,
}

/// The setting the flags in `m` make: `None` unless `--task-secret-file` is given.
///
/// # Errors
/// `--task-read-url` is not `https://host[:port]`.
pub(super) fn from_matches(m: &ArgMatches) -> Result<Option<TaskFlags>, String> {
    let _ = m;
    Ok(None)
}

/// Why the runs cannot be prepared. No message carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum TaskFlagError {
    /// The secret file cannot be used.
    #[error("--task-secret-file: {0}")]
    Secret(#[from] TokenFileError),
    /// The secret is too short for a grant key.
    #[error("--task-secret-file: {0}")]
    GrantKey(#[from] KeyError),
    /// The secret is too short for the mint of the pack keys.
    #[error("--task-secret-file: {0}")]
    Mint(#[from] MintError),
    /// The credentials file cannot be used.
    #[error("--task-put-credentials-file {path}: {why}")]
    Credentials {
        /// The file.
        path: String,
        /// What is wrong with it.
        why: String,
    },
    /// The preparer cannot be set up from the flags.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

impl TaskFlags {
    /// The preparer these flags describe, reading the store through `backend`: the files are
    /// read, the mint and the keys made, the URLs and the expiry checked. Nothing is sent.
    ///
    /// # Errors
    /// [`TaskFlagError`]: a file is missing, not owned by this user or readable by others, the
    /// secret is under 32 bytes, the credentials are not the JSON of a key, or the preparer
    /// refuses the URLs, the bucket, the region or the expiry.
    pub fn open(&self, backend: Backend) -> Result<RunPrep, TaskFlagError> {
        let _ = backend;
        unimplemented!("TaskFlags::open")
    }
}

/// The preparer the flags ask for, if they ask for one, over `backend`.
///
/// # Errors
/// As [`TaskFlags::open`].
pub(super) fn open(
    flags: Option<&TaskFlags>,
    backend: &Backend,
) -> Result<Option<Arc<RunPrep>>, Box<dyn Error>> {
    let prepared = flags.map(|flags| flags.open(backend.clone()));
    Ok(prepared.transpose()?.map(Arc::new))
}

/// What a credentials file that is not JSON of a key is told.
const NOT_KEY: &str = "not JSON with access_key_id and secret_access_key, both not empty";

/// What the credentials file holds: the key, and whatever else a `--presign-key-file` of the
/// tier store says (its ceiling), which is not read here.
#[derive(serde::Deserialize)]
struct Keys {
    access_key_id: String,
    secret_access_key: String,
}

/// The key in the file at `path`, which only its owner may be able to read.
fn read_credentials(path: &Path) -> Result<AccessKey, TaskFlagError> {
    let bad = |why: String| TaskFlagError::Credentials {
        path: path.display().to_string(),
        why,
    };
    let bytes = read_owner_only(path, MAX_FILE_BYTES).map_err(|error| bad(error.to_string()))?;
    let bytes = Zeroizing::new(bytes);
    let keys = serde_json::from_slice::<Keys>(&bytes).map_err(|_| bad(NOT_KEY.to_owned()))?;
    if keys.access_key_id.is_empty() || keys.secret_access_key.is_empty() {
        return Err(bad(NOT_KEY.to_owned()));
    }
    Ok(AccessKey::new(&keys.access_key_id, &keys.secret_access_key))
}

#[cfg(test)]
mod tests;
