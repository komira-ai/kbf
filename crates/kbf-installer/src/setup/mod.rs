//! `kbf-installer setup`: the first configuration of a host (docs/design/distribution.md
//! sections 3, 4 and 7).
//!
//! It validates its flags, then makes the host what they say: the accounts the services need
//! (the fetch user; on Linux the `kbf` user's subid range and lingering), root's state
//! directory, `/etc/kbf/enroll.token` and `/etc/kbf/kbf.toml`, and the units enabled.
//!
//! - It is root's: [`Os::euid`] is checked first, so only a fake that says 0 gets past it
//!   without root.
//! - Files are written by `kbf-host` steps, each a temporary name, fsync, rename and a
//!   sync of the directory, with the token before `kbf.toml`.
//! - Idempotent: steps are planned only where the host differs. The same flags again is an
//!   empty plan; changed flags rewrite the file atomically and never restart a running service.
//! - The frontend (and the all-in-one, which serves it) is enabled but never started unless
//!   `--start-frontend` says so: a restart fails every build in flight. Nothing is started
//!   while no release is installed.
//! - No download happens here. The first release comes from the fetch module, which `setup`
//!   will call through its `Fetcher` (the installer's one downloader) once that has landed.
//!
//! [`config`] is the minimal `kbf.toml` writer standing in for W2's types.

pub mod config;
pub mod os;
pub mod plan;
pub mod validate;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::io::{self, Write};
use std::path::PathBuf;

use kbf_host::{Exec, HostError, HostExec, PathError, Root, Untrusted};

use self::os::{Os, SystemOs};
use self::validate::Raw;
use crate::layout::Platform;

/// The flags of `kbf-installer setup`.
#[derive(clap::Args)]
pub struct SetupArgs {
    /// The frontend, as https://host[:port].
    #[arg(long)]
    frontend_url: String,
    /// What this host runs: frontend, storage, daemon, storage-and-daemon or all-in-one.
    #[arg(long)]
    role: String,
    /// The file holding the enrollment token (mode 0600 or tighter); setup keeps a copy.
    #[arg(long)]
    token_file: String,
    /// Where releases are fetched from: the frontend, or an https mirror (needs --release-url).
    #[arg(long, default_value = "frontend")]
    release_source: String,
    /// The https base URL of the mirror, for --release-source url.
    #[arg(long)]
    release_url: Option<String>,
    /// Refuse releases below this commit (40 hex characters).
    #[arg(long)]
    min_commit: Option<String>,
    /// Refuse releases below this protocol.
    #[arg(long)]
    min_protocol: Option<u32>,
    /// Enable the units without starting them.
    #[arg(long)]
    no_start: bool,
    /// Also start the frontend's units (never started unasked).
    #[arg(long)]
    start_frontend: bool,
}

/// Which units setup may start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Start {
    pub no_start: bool,
    pub start_frontend: bool,
}

/// Why setup stopped.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("setup writes /etc and manages services: run it with sudo (effective uid is {euid})")]
    NotRoot { euid: u32 },
    #[error("refusing to run as root from a binary root does not own alone: {0}")]
    Untrusted(#[from] Untrusted),
    #[error("{}", .0.join("\n"))]
    Invalid(Vec<String>),
    #[error("token file {0}")]
    Token(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("no user {name}: {fix}")]
    NoUser {
        name: &'static str,
        fix: &'static str,
    },
    #[error("no room for a {0} range of 65536 ids above the existing ones")]
    SubIdSpace(&'static str),
    #[error("no free id between 300 and 399 for the fetch account")]
    NoFreeId,
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Host(#[from] HostError),
}

impl SetupArgs {
    fn raw(&self) -> Raw {
        Raw {
            frontend_url: self.frontend_url.clone(),
            role: self.role.clone(),
            token_file: self.token_file.clone(),
            release_source: self.release_source.clone(),
            release_url: self.release_url.clone(),
            min_commit: self.min_commit.clone(),
            min_protocol: self.min_protocol,
        }
    }

    fn start(&self) -> Start {
        Start {
            no_start: self.no_start,
            start_frontend: self.start_frontend,
        }
    }
}

/// Validate, plan and apply `setup` for `raw` on the host `os` and `exec` describe, printing
/// each step as `+ <step>` (never file contents) and each note.
pub fn run<O: Os, E: Exec, W: Write>(
    os: &O,
    exec: &mut E,
    raw: &Raw,
    start: Start,
    out: &mut W,
) -> Result<(), SetupError> {
    let euid = os.euid();
    if euid != 0 {
        return Err(SetupError::NotRoot { euid });
    }
    os.trusted_binary()?;
    let valid = raw.validate().map_err(SetupError::Invalid)?;
    let outcome = plan::build(os, &*exec, &valid, start)?;
    if outcome.plan.is_empty() {
        writeln!(out, "{}", kbf_host::plan::NOTHING_TO_DO)?;
    }
    for (index, step) in outcome.plan.steps().iter().enumerate() {
        writeln!(out, "+ {step}")?;
        exec.apply(step).map_err(|source| HostError::Step {
            index,
            step: step.to_string(),
            source,
        })?;
    }
    for note in &outcome.notes {
        writeln!(out, "note: {note}")?;
    }
    Ok(())
}

/// The verb as the binary runs it, on the host under `root`.
pub fn cli(root: PathBuf, args: &SetupArgs) -> Result<(), SetupError> {
    let mut exec = HostExec::new(Root::new(root)?);
    // `os` reads through its own copy of the host, so `run` can change the real one.
    let reader = exec.clone();
    let os = SystemOs::new(&reader, Platform::current());
    run(&os, &mut exec, &args.raw(), args.start(), &mut io::stdout())
}
