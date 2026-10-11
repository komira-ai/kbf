//! `kbf-deploy minio`: kbf's buckets and their key on a MinIO ([`buckets`]), and a
//! NodePort Service that opens a MinIO's S3 port to hosts off its Kubernetes cluster
//! ([`nodeport`]).
//!
//! Both drive pinned external tools, `mc` (the MinIO client) and `kubectl`, by absolute
//! path with an empty environment. Every read (does the bucket exist, what does the
//! Service hold) is a query through [`Exec::query`], made in a dry run as in an apply;
//! every change is a planned step, so the default run prints what it would do and
//! changes nothing.
//!
//! **No secret on an argv.** argv is world-readable while a tool runs, and printed
//! steps show every argument. The MinIO admin key pair is read from the environment
//! (`MINIO_ROOT_USER`, `MINIO_ROOT_PASSWORD`), never from a flag or a file, and
//! reaches `mc` only inside the `MC_HOST_<alias>` URL in its environment, with a
//! scratch `MC_CONFIG_DIR` so no `mc` config file keeps it. A minted secret key goes
//! from `mc`'s output to the key file and nowhere else. Access key ids (the root
//! user's name, a service account's id) are names, like a user name, and do appear on
//! argv.

use std::fmt;
use std::io::{self, Write};
use std::process::Output;

use serde::de::DeserializeOwned;

use crate::config::ConfigError;
use crate::exec::{Entry, Exec, succeeded};
use crate::path::{HostPath, PathError};
use crate::plan::DeployError;
use crate::step::{Cmd, HttpUrl, UrlError};

pub mod buckets;
pub mod nodeport;

/// A bucket, policy or account name this kit may make and remove: `kbf-` and then
/// lowercase letters, digits and `-`, neither first nor last, 63 characters at most.
/// Every other name on the MinIO is someone else's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KbfName(String);

impl KbfName {
    pub fn new(name: &str) -> Result<KbfName, ArgError> {
        match name.strip_prefix("kbf-") {
            Some(rest) if dns_label(rest, 59) => Ok(KbfName(name.to_owned())),
            _ => Err(ArgError::KbfName(name.to_owned())),
        }
    }
}

impl fmt::Display for KbfName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Kubernetes namespace or Service name: lowercase letters, digits and `-`, neither
/// first nor last, 56 characters at most (so `<name>-kbf-np` is a Service name too).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct K8sName(String);

impl K8sName {
    pub fn new(name: &str) -> Result<K8sName, ArgError> {
        if dns_label(name, 56) {
            Ok(K8sName(name.to_owned()))
        } else {
            Err(ArgError::K8sName(name.to_owned()))
        }
    }
}

impl fmt::Display for K8sName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn dns_label(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A MinIO endpoint: `http://host[:port]`, with no path and no user in it.
pub fn endpoint(url: &str) -> Result<HttpUrl, ArgError> {
    let parsed = HttpUrl::new(url).map_err(ArgError::Url)?;
    if parsed.path() == "/" && !parsed.authority().contains('@') {
        Ok(parsed)
    } else {
        Err(ArgError::Endpoint(url.to_owned()))
    }
}

/// Why a flag's value was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    #[error("{0:?}: name it kbf-<lowercase letters, digits and ->, 63 characters at most")]
    KbfName(String),
    #[error(
        "{0:?} is not a Kubernetes name (lowercase letters, digits and -, 56 characters at most)"
    )]
    K8sName(String),
    #[error("{0:?} is not http://host[:port]: no path and no user")]
    Endpoint(String),
    #[error(transparent)]
    Url(UrlError),
}

/// The MinIO admin key pair, from the environment only.
pub struct Admin {
    pub user: String,
    pub password: String,
}

/// Why a `minio` plan could not be made or carried out.
#[derive(Debug, thiserror::Error)]
pub enum MinioError {
    #[error(
        "{0} is not set in the environment (the admin key pair is never read from a flag or a file)"
    )]
    NoAdmin(&'static str),
    #[error("{url} is not live: {why}")]
    NotLive { url: String, why: String },
    #[error("{0} is not the MinIO client: its --version names no RELEASE")]
    NotMc(HostPath),
    #[error(
        "{path} is inside a git work tree ({git} exists): a key file never goes into a checkout"
    )]
    InGitTree { path: HostPath, git: HostPath },
    #[error("{0} is not a regular file")]
    NotAFile(HostPath),
    #[error("{path}: {source}")]
    KeyFile { path: HostPath, source: ConfigError },
    #[error("no user {0} on this host (id -u, id -g)")]
    NoUser(String),
    #[error("the key in {path} cannot list bucket {bucket}")]
    KeyCannotList { path: HostPath, bucket: KbfName },
    #[error("cannot read {path}: {source}")]
    Inspect { path: HostPath, source: io::Error },
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("{cmd} printed no JSON this kit reads: {source}")]
    Json {
        cmd: String,
        source: serde_json::Error,
    },
    #[error("cannot write JSON: {0}")]
    Serialize(serde_json::Error),
    #[error("Service {0} has no port named s3 at port 9000")]
    NoS3Port(String),
    #[error("Service {0} has no selector")]
    NoSelector(String),
    #[error("nodePort {port} is held by {by}: pick another with --node-port")]
    Held { port: u16, by: String },
    #[error("Service {0} does not carry {label}={value}: left alone", label = nodeport::LABEL, value = nodeport::LABEL_VALUE)]
    NotOurs(String),
    #[error("cannot make a scratch directory: {0}")]
    Scratch(io::Error),
    #[error("cannot write the plan out: {0}")]
    Output(io::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error(transparent)]
    Deploy(#[from] DeployError),
}

/// Print one line of what the plan found, before its steps.
fn note<W: Write>(out: &mut W, line: fmt::Arguments<'_>) -> Result<(), MinioError> {
    writeln!(out, "{line}").map_err(MinioError::Output)
}

/// Run `cmd` as a query; its exit status is the caller's to read.
fn query<E: Exec>(exec: &E, cmd: &Cmd) -> Result<Output, MinioError> {
    exec.query(cmd).map_err(|source| MinioError::Query {
        cmd: cmd.to_string(),
        source,
    })
}

/// Run `cmd` as a query; a non-zero exit is an error naming its stderr.
fn checked<E: Exec>(exec: &E, cmd: &Cmd) -> Result<Output, MinioError> {
    succeeded(cmd, query(exec, cmd)?).map_err(|source| MinioError::Query {
        cmd: cmd.to_string(),
        source,
    })
}

/// Run `cmd` as a query and read its output as JSON.
fn read_json<T: DeserializeOwned, E: Exec>(exec: &E, cmd: &Cmd) -> Result<T, MinioError> {
    let out = checked(exec, cmd)?;
    serde_json::from_slice(&out.stdout).map_err(|source| MinioError::Json {
        cmd: cmd.to_string(),
        source,
    })
}

/// Run `cmd` as a query and tell whether it exited 0.
fn exits_0<E: Exec>(exec: &E, cmd: &Cmd) -> Result<bool, MinioError> {
    Ok(query(exec, cmd)?.status.success())
}

fn inspect<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Entry>, MinioError> {
    exec.inspect(path).map_err(|source| MinioError::Inspect {
        path: path.clone(),
        source,
    })
}

/// Percent-encode every byte of `s` but the unreserved ones, for a URL's user part.
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
#[path = "minio_tests.rs"]
mod tests;
