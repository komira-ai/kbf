//! The apt channel: the release's native packages (`kbf-native-<commit>-linux-x86_64.deb` and
//! `...-linux-arm64.deb`, pool files `kbf_<version>_{amd64,arm64}.deb`) into the apt repository
//! the organisation's products share, `packages.komira.ai`. `.github/workflows/release.yml` runs these
//! verbs in three jobs, split so that no job holds both the signing key and the cloud role. The
//! repository's other publisher writes into it too, and the text of the index these verbs write
//! is the text it writes for the same entries.
//!
//! `read`, `index` and `write` take `--suite stable|testing` (default `stable`): a release is
//! published into `testing` first, and `promote` lists the same packages in `stable` once the
//! install check passed.
//!
//! - `channels apt version --debs <dir> --commit <sha> --output <file>` ([`version`]): the
//!   version the two packages carry, as `package-version=<v>`. Job `verify`.
//! - `channels apt read --bucket <b> --out <dir>` ([`read`]): the current index and the ETag of
//!   each of its objects, from S3 (never through the CDN, which caches the index and would hand
//!   back a stale ETag). Jobs `testing-apt-read` and
//!   `stable-apt-read`: the cloud role, no key.
//! - `channels apt index --current <dir> --out <dir> --debs <dir> --commit <sha> --version <V> ...`
//!   ([`build`]): the packages merged into that index, every other product's entries kept as they
//!   are. Reads no cloud and no key.
//! - `channels apt promote --testing <dir> --stable <dir> --out <dir> --version <V> --sums <file>
//!   --commit <sha> ...` ([`promote`]): the entries of a release `testing` lists, merged into the index of
//!   `stable`; reads no cloud and no key. The write that follows is `apt write --suite stable
//!   --no-pool`.
//! - `channels apt sign --dir <dir> --work <dir> --fingerprint <fpr> (--key-file <f> |
//!   --key-env <name>) (--passphrase-file <f> | --passphrase-env <name>)` ([`sign`]):
//!   `InRelease` and `Release.gpg`, verified. Jobs `testing-apt-index` and `stable-apt-index`: the
//!   key (by the two `--...-env` flags), no cloud role.
//! - `channels apt write --bucket <b> --distribution-id <id> --hostname <h> --next <dir> --debs
//!   <dir> --commit <sha> --version <V>` ([`write`]): the packages into the pool with `If-None-Match`, the index
//!   with `If-Match` on the ETags that were read (`--no-pool` instead of `--debs`, `--commit` and `--version`:
//!   the index only). Jobs `testing-apt-write` and `stable-apt-write`: the cloud role, no key.
//!
//! The programs that talk to the outside (`aws`, `gpg`) are flags, so a test hands a stand-in
//! ([`super::aws::Aws`], [`sign::Gpg`]).

pub mod build;
pub mod control;
pub mod deb;
pub mod index;
pub mod promote;
pub mod read;
pub mod sign;
pub mod state;
pub mod version;
pub mod write;

#[cfg(test)]
pub(crate) mod testing;

use std::path::{Path, PathBuf};

use clap::Subcommand;

pub use state::Suite;

use super::aws::{Answer, RealAws};
use super::{Error, is_plain_version};
use state::{DEFAULT_ARCHS, PLATFORMS, asset_name, file_name, is_commit};

/// The `channels apt` verbs.
#[derive(Subcommand)]
pub enum Verb {
    /// The version the release's packages carry, from their control files.
    Version {
        /// The release's files (the `release-assets` artifact).
        #[arg(long)]
        debs: PathBuf,
        /// The release's full commit, which names its package files in `--debs`.
        #[arg(long)]
        commit: String,
        /// Append `package-version=<version>` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: PathBuf,
    },
    /// The current index and the ETag of each of its objects, from S3.
    Read {
        /// The suite of the repository: `stable`, or `testing`, where a release is published
        /// before its install checks and promoted from.
        #[arg(long, value_enum, default_value_t = Suite::Stable)]
        suite: Suite,
        /// The apt repository's bucket.
        #[arg(long)]
        bucket: String,
        /// Where the copy and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
    /// The packages merged into the index that was read.
    Index {
        /// The suite of the repository: `stable`, or `testing`, where a release is published
        /// before its install checks and promoted from.
        #[arg(long, value_enum, default_value_t = Suite::Stable)]
        suite: Suite,
        /// The directory `apt read` wrote.
        #[arg(long)]
        current: PathBuf,
        /// Where the new unsigned index and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// The release's files (the `release-assets` artifact).
        #[arg(long)]
        debs: PathBuf,
        /// The release's full commit, which names its package files in `--debs`.
        #[arg(long)]
        commit: String,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The `Origin` of a first `Release`; an existing one keeps its own.
        #[arg(long)]
        origin: String,
        /// The `Label` of a first `Release`.
        #[arg(long)]
        label: String,
        /// The `Description` of a first `Release`.
        #[arg(long)]
        description: String,
        /// How many versions of a package and architecture the index keeps.
        #[arg(long, default_value_t = build::KEEP)]
        keep: usize,
    },
    /// `InRelease` and `Release.gpg`, verified.
    Sign {
        /// `dists/<suite>` of the new index.
        #[arg(long)]
        dir: PathBuf,
        /// Where the private keyring is made; it is removed at the end.
        #[arg(long)]
        work: PathBuf,
        /// The signing subkey's fingerprint, or several separated by commas (a key rotation).
        #[arg(long)]
        fingerprint: String,
        /// The file holding the armored private key; removed at the end. Exactly one of this and
        /// `--key-env`.
        #[arg(long, conflicts_with = "key_env", required_unless_present = "key_env")]
        key_file: Option<PathBuf>,
        /// The environment variable that holds the armored private key: it is written to a file of
        /// mode 0600 below `--work`, which is removed at the end.
        #[arg(long, conflicts_with = "key_file")]
        key_env: Option<String>,
        /// The file holding the key's passphrase; removed at the end. Exactly one of this and
        /// `--passphrase-env`.
        #[arg(
            long,
            conflicts_with = "passphrase_env",
            required_unless_present = "passphrase_env"
        )]
        passphrase_file: Option<PathBuf>,
        /// The environment variable that holds the key's passphrase: it is written to a file of
        /// mode 0600 below `--work`, which is removed at the end.
        #[arg(long, conflicts_with = "passphrase_file")]
        passphrase_env: Option<String>,
        /// The `gpg` program.
        #[arg(long, default_value = "gpg")]
        gpg: PathBuf,
        /// Append `signed=true` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// The entries of a release that `testing` lists, merged into the index of `stable`.
    Promote {
        /// The directory `apt read --suite testing` wrote.
        #[arg(long)]
        testing: PathBuf,
        /// The directory `apt read --suite stable` wrote.
        #[arg(long)]
        stable: PathBuf,
        /// Where the new unsigned index of `stable` and `etags.txt` are written.
        #[arg(long)]
        out: PathBuf,
        /// `<major>.<minor>.<patch>`.
        #[arg(long)]
        version: String,
        /// The release's `SHA256SUMS`: the entries promoted are held to its digests.
        #[arg(long)]
        sums: PathBuf,
        /// The release's full commit, which names its package files in `--sums`.
        #[arg(long)]
        commit: String,
        /// The `Origin` of a first `Release`; an existing one keeps its own.
        #[arg(long)]
        origin: String,
        /// The `Label` of a first `Release`.
        #[arg(long)]
        label: String,
        /// The `Description` of a first `Release`.
        #[arg(long)]
        description: String,
        /// How many versions of a package and architecture the index keeps.
        #[arg(long, default_value_t = build::KEEP)]
        keep: usize,
    },
    /// The packages and the signed index into the repository.
    Write {
        /// The suite of the repository: `stable`, or `testing`, where a release is published
        /// before its install checks and promoted from.
        #[arg(long, value_enum, default_value_t = Suite::Stable)]
        suite: Suite,
        /// The apt repository's bucket.
        #[arg(long)]
        bucket: String,
        /// The CDN distribution to invalidate `/apt/dists/*` in.
        #[arg(long)]
        distribution_id: String,
        /// The repository's host name, for the closing message.
        #[arg(long)]
        hostname: String,
        /// The directory `apt index` wrote, signed by `apt sign`.
        #[arg(long)]
        next: PathBuf,
        /// The release's files (the `release-assets` artifact).
        #[arg(long, required_unless_present = "no_pool", conflicts_with = "no_pool")]
        debs: Option<PathBuf>,
        /// The release's full commit, which names its package files in `--debs`.
        #[arg(long, required_unless_present = "no_pool", conflicts_with = "no_pool")]
        commit: Option<String>,
        /// `<major>.<minor>.<patch>`.
        #[arg(long, required_unless_present = "no_pool", conflicts_with = "no_pool")]
        version: Option<String>,
        /// Write the index only: the packages are in the pool already (a promotion from
        /// `testing`).
        #[arg(long)]
        no_pool: bool,
        /// This is the last attempt: a lost race fails instead of asking for another.
        #[arg(long)]
        last: bool,
        /// Append `conflict=true` or `conflict=false` to this file (`GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
        /// The `aws` program.
        #[arg(long, default_value = "aws")]
        aws: PathBuf,
    },
}

/// Runs a verb: the lines to print.
///
/// # Errors
/// Whatever the verb refuses or fails on.
pub fn run(verb: Verb) -> Result<Vec<String>, Error> {
    match verb {
        Verb::Version {
            debs,
            commit,
            output,
        } => version::version(&debs, &commit, &output),
        Verb::Read {
            suite,
            bucket,
            out,
            aws,
        } => read::read(&RealAws(aws), suite, &bucket, &out),
        Verb::Index {
            suite,
            current,
            out,
            debs,
            commit,
            version,
            origin,
            label,
            description,
            keep,
        } => build::build(&build::Request {
            suite,
            current: &current,
            out: &out,
            debs: &debs,
            commit: &commit,
            version: &version,
            origin: &origin,
            label: &label,
            description: &description,
            date: now()?,
            keep,
        }),
        Verb::Sign {
            dir,
            work,
            fingerprint,
            key_file,
            key_env,
            passphrase_file,
            passphrase_env,
            gpg,
            output,
        } => {
            let key = sign::Source::choose("key", key_file.as_deref(), key_env.as_deref())?;
            let passphrase = sign::Source::choose(
                "passphrase",
                passphrase_file.as_deref(),
                passphrase_env.as_deref(),
            )?;
            sign::sign(
                &sign::RealGpg(gpg),
                &sign::Request {
                    dir: &dir,
                    work: &work,
                    fingerprints: &fingerprint,
                    key,
                    passphrase,
                },
                &sign::process_env,
                output.as_deref(),
            )
        }
        Verb::Promote {
            testing,
            stable,
            out,
            version,
            sums,
            commit,
            origin,
            label,
            description,
            keep,
        } => promote::promote(&promote::Request {
            testing: &testing,
            stable: &stable,
            out: &out,
            version: &version,
            sums: &sums,
            commit: &commit,
            origin: &origin,
            label: &label,
            description: &description,
            date: now()?,
            keep,
        }),
        Verb::Write {
            suite,
            bucket,
            distribution_id,
            hostname,
            next,
            debs,
            commit,
            version,
            no_pool,
            last,
            output,
            aws,
        } => {
            let pool = match (&debs, &commit, &version, no_pool) {
                (Some(debs), Some(commit), Some(version), false) => Some(write::Pool {
                    debs,
                    commit,
                    version,
                }),
                (None, None, None, true) => None,
                _ => {
                    return Err(Error::Usage(
                        "give --debs, --commit and --version, or --no-pool and none of them"
                            .to_owned(),
                    ));
                }
            };
            write::write(
                &RealAws(aws),
                &write::Request {
                    suite,
                    bucket: &bucket,
                    distribution_id: &distribution_id,
                    hostname: &hostname,
                    next: &next,
                    pool,
                    last,
                },
                output.as_deref(),
            )
        }
    }
}

/// The `Date` of a `Release`: seconds since the epoch on the clock of this machine.
///
/// # Errors
/// A clock set before 1970.
fn now() -> Result<u64, Error> {
    epoch_secs(std::time::SystemTime::now())
}

/// `time` as seconds since the epoch.
///
/// # Errors
/// A time before 1970.
fn epoch_secs(time: std::time::SystemTime) -> Result<u64, Error> {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| Error::Failed(format!("the clock is before 1970: {e}")))
}

/// What an `aws` call that failed was, from its standard error.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The object is not there (`NoSuchKey`).
    NotFound,
    /// S3 answers `AccessDenied` for a key that does not exist when the caller may not list the
    /// bucket, as the publish role may not.
    Denied,
    /// A conditional write lost: the version it named is not the current one (`412`), or another
    /// write to the same key is in flight (`409`).
    Precondition,
    /// Anything else.
    Other,
}

/// What a failed `aws` call was.
pub fn classify(stderr: &str) -> Failure {
    let has = |words: &[&str]| words.iter().any(|w| stderr.contains(w));
    if has(&["PreconditionFailed", "ConditionalRequestConflict", "(412)"]) {
        Failure::Precondition
    } else if has(&["NoSuchKey", "(404)", "Not Found"]) {
        Failure::NotFound
    } else if has(&["AccessDenied", "(403)", "Forbidden"]) {
        Failure::Denied
    } else {
        Failure::Other
    }
}

/// `aws <args>` as the verbs ask it.
pub(crate) fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// The words of a failed call for a message.
pub(crate) fn why(answer: &Answer) -> String {
    answer.stderr.trim().to_owned()
}

/// The pool file names of the packages of `version`, one per Debian architecture, sorted.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`.
pub fn package_names(version: &str) -> Result<Vec<String>, Error> {
    if !is_plain_version(version) {
        return Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )));
    }
    let mut names: Vec<String> = DEFAULT_ARCHS
        .iter()
        .map(|arch| file_name(version, arch))
        .collect();
    names.sort();
    Ok(names)
}

/// The release's package files for `commit` in `dir` (`kbf-native-<commit>-<platform>.deb`), as
/// `(pool name, path)`, one per Debian architecture, sorted by pool name.
///
/// # Errors
/// A version that is not `<major>.<minor>.<patch>`, a commit that is not 40 hex digits, or a
/// package that is not there.
pub fn package_files(
    dir: &Path,
    commit: &str,
    version: &str,
) -> Result<Vec<(String, PathBuf)>, Error> {
    package_names(version)?;
    if !is_commit(commit) {
        return Err(Error::Usage(format!(
            "--commit {commit:?} is not a full commit (40 lower case hex digits)"
        )));
    }
    let files: Vec<(String, PathBuf)> = PLATFORMS
        .iter()
        .map(|(arch, platform)| {
            (
                file_name(version, arch),
                dir.join(asset_name(commit, platform)),
            )
        })
        .collect();
    match files.iter().find(|(_, path)| !path.is_file()) {
        Some((_, path)) => Err(Error::Failed(format!(
            "{} is not in {}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            dir.display()
        ))),
        None => Ok(files),
    }
}

#[cfg(test)]
mod tests;
