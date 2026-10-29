//! What one run is told: the pack key it may write, and the command line of the task tool.
//!
//! A job of the lease server has no secret field yet, so the pack url and the read token
//! travel in the arguments, where the action can read them (`/proc/1/cmdline`) and the
//! engine's inspect shows them. That is [`Delivery::InsecureArgv`], and the executor
//! refuses to run without being told to use it. Nothing here prints a secret: [`Secret`]
//! formats as `<redacted>`, and so does a [`TaskArgs`].

use std::fmt;
use std::time::Duration;

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
/// attempt. A zombie of an earlier run holds the url of its own key, so a later run, which
/// has another nonce, never meets the create-only refusal of a key it did not write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nonce(u128);

impl Nonce {
    /// A nonce from the operating system's random source, which cannot fail in a way a
    /// caller could repair: a failure stops the process rather than reuse a key.
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

/// The one object key a task may write: `cas/b/<build>/<nonce>-<index>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackKey {
    /// The build the key belongs to: the executor's frontend id.
    pub build: String,
    /// Random, per `POST`.
    pub nonce: Nonce,
    /// Which pack of the task; one pack per task, so 0.
    pub index: u32,
}

impl fmt::Display for PackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cas/b/{}/{}-{}", self.build, self.nonce, self.index)
    }
}

/// How the secrets reach the task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// As arguments, behind the tool's `--insecure-argv-secrets`. Until the lease server's
    /// job takes a secret field; an `Env` arm follows it.
    InsecureArgv,
}

/// The command line of one task: what the task tool is given.
pub struct TaskArgs<'a> {
    /// The path of the tool in the container.
    pub tool: &'a str,
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
    /// The farm's root certificate in the container, when the reads are `https`.
    pub root_der: Option<&'a str>,
    /// Whether the `PUT` goes over plain http (until the store serves TLS).
    pub allow_plain_http_put: bool,
}

impl TaskArgs<'_> {
    /// The arguments, tool first, for [`Delivery::InsecureArgv`].
    #[must_use]
    pub fn argv(&self, delivery: Delivery) -> Vec<String> {
        let Delivery::InsecureArgv = delivery;
        let mut argv = vec![
            self.tool.to_owned(),
            "--action".to_owned(),
            format!("{}/{}", self.action.hash, self.action.size_bytes),
            "--lease".to_owned(),
            self.lease.to_string(),
            "--grant-url".to_owned(),
            self.grant_url.to_owned(),
            "--deadline-ms".to_owned(),
            self.deadline.as_millis().to_string(),
            "--insecure-argv-secrets".to_owned(),
            "--pack-url".to_owned(),
            self.pack_url.expose().to_owned(),
            "--grant-token".to_owned(),
            self.grant_token.expose().to_owned(),
        ];
        if let Some(root) = self.root_der {
            argv.extend(["--root-der".to_owned(), root.to_owned()]);
        }
        if self.allow_plain_http_put {
            argv.push("--allow-plain-http-put".to_owned());
        }
        argv
    }
}

impl fmt::Debug for TaskArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskArgs")
            .field("tool", &self.tool)
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
