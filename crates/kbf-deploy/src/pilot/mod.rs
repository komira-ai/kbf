//! `kbf-deploy pilot`: a single pilot kbf-server run as a systemd user service
//! ([`server`], `pilot server-install`), and the buck2 overlay that points one linked
//! worktree at it ([`buckconfig`], `pilot buckconfig`).
//!
//! Both verbs run as the operator's own user, never as root, and plan by default:
//! without `--apply` they print each step as `DRY-RUN:` and change nothing. Every read
//! of the host (a tool's answer, a file, what is at a path) goes through
//! [`Exec`](kbf_host::Exec), in a dry run as in an apply, so a dry run refuses what an
//! apply would. Both read `--config FILE` over the kit's closed key set ([`config`]).
//!
//! Exit codes: 0 done (or nothing to do), 1 a step failed or kbf-server did not report
//! ready after its start, 2 refused (`REFUSED: ...` on stderr).

use std::fmt;
use std::io::{self, Write};
use std::process::Output;

use crate::config::ConfigError;
use crate::template::TemplateError;
use kbf_host::exec::{Entry, Exec, Meta};
use kbf_host::path::{HostPath, PathError};
use kbf_host::step::{Cmd, Perms};

pub mod buckconfig;
pub mod certs;
pub mod cli;
pub mod config;
pub mod server;

/// The tools the verbs run, where Debian and Ubuntu put them.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
pub const SS: &str = "/usr/bin/ss";
pub const IP: &str = "/usr/bin/ip";
pub const GIT: &str = "/usr/bin/git";
pub const ID: &str = "/usr/bin/id";

/// Why a `pilot` verb refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum PilotError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Host(#[from] kbf_host::HostError),
    #[error("{key}: {source}")]
    KeyPath {
        key: &'static str,
        source: PathError,
    },
    #[error("cannot read {path}: {source}")]
    Inspect { path: HostPath, source: io::Error },
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("cannot write the plan out: {0}")]
    Write(io::Error),
    #[error("{0} is not set in the environment")]
    NoEnv(&'static str),
    #[error("give --bin FILE, the kbf-server binary to install")]
    NoBin,

    // settings
    #[error(
        "no bind address (--bind, KBF_SERVER_BIND or KBF_SERVER_HOST): the server host's address"
    )]
    NoBind,
    #[error("bind address {0:?} is not an IPv4 address")]
    NotIpv4(String),
    #[error("{key} must be a port number from 1 to 65535, not '{value}'")]
    NotPort { key: &'static str, value: String },
    #[error("{key}={value} is not like {like}")]
    Cap {
        key: &'static str,
        value: String,
        like: &'static str,
    },
    #[error("KBF_S3_CONDITIONAL_PUT={0}: 1 or 0")]
    ZeroOrOne(String),
    #[error("{what} holds a character the {into} cannot carry: {value:?}")]
    Chars {
        what: String,
        into: &'static str,
        value: String,
    },

    // server-install
    #[error("server-install runs on Linux (systemd user services), not {0}")]
    NotLinux(String),
    #[error("this verb installs a systemd user service: run it as yourself, not root (no sudo)")]
    IsRoot,
    #[error("{iface} does not carry {bind} (ip -4 addr show dev {iface})")]
    NotCarried { iface: String, bind: String },
    #[error("port {0} is held by another process (ss -ltnp 'sport = :{0}')")]
    PortHeld(u16),
    #[error("{0} is missing")]
    Missing(HostPath),
    #[error(
        "{path} is inside a git work tree ({git} exists): keep keys and credentials outside every repository"
    )]
    InGitTree { path: HostPath, git: HostPath },
    #[error("{path} has mode {perms}, not 0600: chmod 600 {path}")]
    NotPrivate { path: HostPath, perms: Perms },
    #[error("{0} needs AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY= lines, each with a value")]
    S3Env(HostPath),
    #[error("{path}: {why}")]
    Cert { path: HostPath, why: String },
    #[error("{0} is not an executable file")]
    NotExecutable(HostPath),
    #[error("{bin} is not an ELF executable for {arch}")]
    WrongBinary { bin: HostPath, arch: String },
    #[error("no binary of this host's CPU ({0}) is known to server-install")]
    UnknownArch(String),
    #[error("{bin} has no {flag} (its --help does not list it), which its unit passes")]
    NoFlag { bin: HostPath, flag: String },
    #[error("{url} answered {got}; --apply refuses until it answers 200")]
    StoreDown { url: String, got: String },
    /// Steps ran, so this is a failed verb (exit 1), not a refusal.
    #[error("kbf-server did not report ready within {secs} s")]
    NotReady { secs: u64 },

    // buckconfig
    #[error("{0} has no .buckconfig: not a buck2 checkout")]
    NotCheckout(HostPath),
    #[error("{0} is the main checkout; write the overlay into a linked worktree only")]
    MainCheckout(HostPath),
    #[error(".buckconfig.local is not gitignored in {0}")]
    NotIgnored(HostPath),
    #[error(
        "{0} was not written by this verb (its first line is not '{MARKER}'); move it aside first. Left alone"
    )]
    Foreign(HostPath),
    #[error("the linux and darwin property sets must differ")]
    SameProperties,
    #[error("{key}: property '{pair}' is not KEY=VALUE")]
    NotPair { key: &'static str, pair: String },
    #[error("{key} names OSFamily {n} times ({props}); kbf refuses that")]
    OsFamilyTwice {
        key: &'static str,
        n: usize,
        props: String,
    },
    #[error(
        "{key} ({props}) names no OSFamily: kbf ignores names it does not know, so it would match every node. Add OSFamily={want}"
    )]
    NoOsFamily {
        key: &'static str,
        props: String,
        want: &'static str,
    },
    #[error("{key}: OSFamily={value} does not name {os}")]
    WrongOsFamily {
        key: &'static str,
        value: String,
        os: &'static str,
    },
    #[error(
        "KBF_DARWIN_PROPERTIES asks for {pair}, but no daemon carries --label {want} (KBF_DARWIN_LABELS={labels})"
    )]
    LabelNotCarried {
        pair: String,
        want: String,
        labels: String,
    },
}

impl PilotError {
    fn path(key: &'static str, source: PathError) -> PilotError {
        PilotError::KeyPath { key, source }
    }

    /// 1 for a step that failed or a server that never reported ready, 2 for a refusal.
    pub fn exit_code(&self) -> u8 {
        match self {
            PilotError::Host(kbf_host::HostError::Step { .. }) | PilotError::NotReady { .. } => 1,
            _ => 2,
        }
    }
}

/// The first line of every overlay `buckconfig` writes; it removes or replaces only a
/// file that starts with it.
pub const MARKER: &str = "# kbf-pilot-overlay";

/// A tool's path.
fn tool(path: &str) -> HostPath {
    HostPath::new(path).expect("a tool's path is absolute")
}

/// `program` with `args`.
fn command(program: &str, args: &[&str]) -> Cmd {
    args.iter()
        .fold(Cmd::new(tool(program)), |cmd, a| cmd.arg(*a))
}

/// Print one line of what the plan found, before its steps.
fn note(out: &mut dyn Write, line: fmt::Arguments<'_>) -> Result<(), PilotError> {
    writeln!(out, "{line}").map_err(PilotError::Write)
}

/// Run `cmd` as a query; its exit status is the caller's to read.
fn query(exec: &dyn Exec, cmd: &Cmd) -> Result<Output, PilotError> {
    exec.query(cmd).map_err(|source| PilotError::Query {
        cmd: cmd.to_string(),
        source,
    })
}

fn inspect(exec: &dyn Exec, path: &HostPath) -> Result<Option<Entry>, PilotError> {
    exec.inspect(path).map_err(|source| PilotError::Inspect {
        path: path.clone(),
        source,
    })
}

fn stat(exec: &dyn Exec, path: &HostPath) -> Result<Option<Meta>, PilotError> {
    exec.stat(path).map_err(|source| PilotError::Inspect {
        path: path.clone(),
        source,
    })
}

/// `name` under the directory `dir`.
fn under(dir: &HostPath, name: &str) -> Result<HostPath, PilotError> {
    Ok(HostPath::new(&format!("{dir}/{name}"))?)
}

/// `value` carries only letters, digits and `._:/=,+@-`, and a space when `space`:
/// what a unit line or the overlay can hold as it is. A `%` is never one (systemd
/// reads it as a specifier in `ExecStart=`).
fn carried(what: &str, into: &'static str, value: &str, space: bool) -> Result<(), PilotError> {
    let ok = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._:/=,+@-".contains(&b) || (space && b == b' '));
    if ok {
        Ok(())
    } else {
        Err(PilotError::Chars {
            what: what.to_owned(),
            into,
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
