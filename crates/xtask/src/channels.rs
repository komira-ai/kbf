//! `cargo xtask channels`: the kbf native release reaching the apt repository at
//! `https://packages.komira.ai/apt`, and the check that it installs. `.github/workflows/release.yml`
//! runs these verbs in its `testing-apt-*`, `check-apt` and `stable-apt-*` jobs.
//!
//! - `channels gate --label <job> [--setting NAME=VALUE]... [--secret NAME]... [--require
//!   NAME=VALUE]... [--output <file>]` ([`gate`]): whether a job has the settings it needs; a
//!   setting's value is on the command line, a secret's is read from the environment by the verb
//!   itself (only names are ever printed). None set: `enabled=false` and a
//!   notice, the job ends green and writes nothing. All set: `enabled=true`. Some set: red,
//!   naming the missing ones.
//! - `channels assume --role-arn <arn> --session-name <name> --region <r> --env-file <file>
//!   --token-url <url>` ([`assume`]): the job's AWS credentials from its OIDC token (the bearer
//!   from `ACTIONS_ID_TOKEN_REQUEST_TOKEN`), into the env file; the step `release.yml` runs
//!   where an action would otherwise do it.
//! - `channels apt version|read|index|promote|sign|write ...` ([`apt`]): the release's native
//!   Debian packages into the apt repository the organisation's products share, in the jobs of
//!   `release.yml`.
//! - `channels check-apt --arch <arch> --version <X.Y.Z> --commit <sha> ...` ([`check`]): the
//!   release installed from the `testing` suite in a clean Ubuntu system, as a host does.
//!
//! The programs that talk to the outside (`aws`, `gpg`, `apt-get`, `dpkg`, `curl`) are flags, so a
//! test hands a stand-in ([`aws::Aws`], [`apt::sign::Gpg`], [`check::Host`]).
//!
//! The verbs return the lines to print; [`crate::run`] joins them. An [`Error::Usage`] is a usage
//! error of the command line (exit status 2), any other [`Error`] a failure (exit status 1).

pub mod apt;
pub mod assume;
pub mod aws;
pub mod check;
pub mod gate;
pub mod sums;

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Subcommand;

/// Why a channel verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A flag or file name that is not of its form, or flags that do not fit together (exit
    /// status 2).
    #[error("{0}")]
    Usage(String),
    /// The verb could not do what it was asked.
    #[error("{0}")]
    Failed(String),
    /// Every problem a check found, one line each.
    #[error("{}", .0.join("\n"))]
    Refused(Vec<String>),
    /// A program could not be started, or a file not read or written.
    #[error("{what}: {why}")]
    Spawn {
        /// What was being done.
        what: String,
        /// The operating system's reason.
        why: String,
    },
}

impl Error {
    /// The error for a program that could not be started, or a file not read: `what` and the
    /// system's reason.
    pub(crate) fn spawn(what: String) -> impl FnOnce(std::io::Error) -> Error {
        move |e| Error::Spawn {
            what,
            why: e.to_string(),
        }
    }
}

impl From<Error> for crate::Error {
    fn from(e: Error) -> crate::Error {
        match e {
            Error::Usage(why) => crate::Error::Usage(why),
            other => crate::Error::Channels(other),
        }
    }
}

/// Whether `text` is `<major>.<minor>.<patch>`: three whole numbers and nothing else.
pub(crate) fn is_plain_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (*p == "0" || !p.starts_with('0'))
        })
}

/// `<major>.<minor>.<patch>` as numbers; none for anything else, or a number too large.
pub(crate) fn numbers(version: &str) -> Option<(u64, u64, u64)> {
    if !is_plain_version(version) {
        return None;
    }
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// Is `name` a bucket name (lower case letters, digits, `-` and `.`)?
pub(crate) fn is_bucket(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Is `id` a CloudFront distribution id (upper case letters and digits)?
pub(crate) fn is_distribution_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Appends `lines` to `output` (on Actions, `GITHUB_OUTPUT` or `GITHUB_ENV`), one per line.
pub(crate) fn append_output(output: &Path, lines: &[String]) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .and_then(|mut f| writeln!(f, "{}", lines.join("\n")))
        .map_err(Error::spawn(format!("append to {}", output.display())))
}

/// The `channels` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// Whether a channel job has its settings: none set is a green no-op, some set is red.
    Gate {
        /// The job, as the notice names it.
        #[arg(long)]
        label: String,
        /// One setting the job needs, as `NAME=VALUE`; an empty value is unset. No value is
        /// printed.
        #[arg(long = "setting")]
        settings: Vec<String>,
        /// The name of an environment variable holding secret material the job needs; only
        /// whether it is empty is read, and no value is printed.
        #[arg(long = "secret")]
        secrets: Vec<String>,
        /// An earlier job's output, as `NAME=VALUE`: unless every `VALUE` is `true` the job is
        /// off and green, and its settings are not judged.
        #[arg(long = "require")]
        requires: Vec<String>,
        /// Append `enabled=true` or `enabled=false` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// The job's AWS credentials from its OIDC token, appended to the env file (`GITHUB_ENV`).
    Assume {
        /// The role to assume.
        #[arg(long)]
        role_arn: String,
        /// The session name AWS records.
        #[arg(long)]
        session_name: String,
        /// The region the later steps use.
        #[arg(long)]
        region: String,
        /// Where the `AWS_*` lines are appended.
        #[arg(long)]
        env_file: PathBuf,
        /// The runner's token request URL (`ACTIONS_ID_TOKEN_REQUEST_URL`); the bearer is read
        /// from `ACTIONS_ID_TOKEN_REQUEST_TOKEN`.
        #[arg(long)]
        token_url: String,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// The release installed from the suite `testing` in a clean Ubuntu system.
    CheckApt {
        /// The Debian architecture of this machine, `amd64` or `arm64`.
        #[arg(long)]
        arch: String,
        /// `<major>.<minor>.<patch>`, the version of the package installed.
        #[arg(long)]
        version: String,
        /// The release's full commit, which `kbf --version --json` must name.
        #[arg(long)]
        commit: String,
        /// The apt repository.
        #[arg(long, default_value = "https://packages.komira.ai/apt")]
        apt_url: String,
        /// The system to check; `/` in the container.
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// The `apt-get` program.
        #[arg(long, default_value = "apt-get")]
        apt_get: PathBuf,
        /// The `dpkg-query` program.
        #[arg(long, default_value = "dpkg-query")]
        dpkg_query: PathBuf,
        /// The `dpkg` program.
        #[arg(long, default_value = "dpkg")]
        dpkg: PathBuf,
        /// The `curl` program.
        #[arg(long, default_value = "curl")]
        curl: PathBuf,
    },
    /// The apt repository: read the index, merge the packages, sign it, write it.
    Apt {
        /// Which step.
        #[command(subcommand)]
        verb: apt::Verb,
    },
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Gate {
            label,
            settings,
            secrets,
            requires,
            output,
        } => gate::gate(
            &gate::process_env,
            &label,
            &settings,
            &secrets,
            &requires,
            output.as_deref(),
        ),
        Verb::Assume {
            role_arn,
            session_name,
            region,
            env_file,
            token_url,
            aws,
        } => assume::assume(
            &aws::RealAws(aws),
            &mut std::io::stdout().lock(),
            &|| assume::oidc_from_env(&token_url),
            &assume::Request {
                role_arn: &role_arn,
                session_name: &session_name,
                region: &region,
                env_file: &env_file,
                token_dir: &std::env::temp_dir(),
                https_only: true,
            },
        ),
        Verb::CheckApt {
            arch,
            version,
            commit,
            apt_url,
            root,
            apt_get,
            dpkg_query,
            dpkg,
            curl,
        } => check::check_apt(
            &check::RealHost,
            &check::Apt {
                root: &root,
                arch: &arch,
                version: &version,
                commit: &commit,
                apt_url: &apt_url,
                apt_get: &apt_get,
                dpkg_query: &dpkg_query,
                dpkg: &dpkg,
                curl: &curl,
            },
        ),
        Verb::Apt { verb } => apt::run(verb),
    }
}

#[cfg(test)]
mod tests;
