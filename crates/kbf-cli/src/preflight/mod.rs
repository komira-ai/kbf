//! Startup self-checks: what `kbf <mode>` needs of its host, checked before the mode
//! serves, every missing piece reported together, one line each: what is missing, why it
//! matters, how to fix it.
//!
//! `kbf-deploy` used to be the only place that knew these prerequisites, and it runs as
//! root at install time. The checks here are the ones the service user can make about
//! its own process, at every start, so a host that drifted after the install (a key
//! file chmod-ed, a certificate swapped for another role's, a unit that lost its
//! `AWS_*` lines) is refused with its cause instead of failing at the first call.
//!
//! What this module checks, by family (each is a function of [`Needs`] and a [`Host`]):
//!
//! - files: every certificate, key, CA, deny-list and token file the flags name is a
//!   regular file ([`files`]);
//! - secrets: every key and token file belongs to the user the process runs as and no
//!   group or other can read it ([`files`]);
//! - certificates: the certificate a mode presents names the role and carries the
//!   extended key usage its row of the farm's table says, a daemon's names exactly its
//!   `--node-id`, and it chains to its CA now ([`certs`]);
//! - each certificate's private key is PEM, of a kind TLS can sign with, and the pair of the
//!   certificate's public key ([`certs`]);
//! - the cold store's S3 key pair is in the environment ([`env`]);
//! - the container driver's `crun` ([`env`]);
//! - the open-file limit in force is the floor the units set ([`env`], a warning);
//! - a frontend names its state store, with `--state-store` or the `state-store` key
//!   ([`state_store`]; a function of the flags alone, not of [`Needs`]).
//! - the host rules `kbf-deploy linux check-units` had (re-homed here; the verb is retired): the directories above a `file://` `--hot-store`
//!   can be entered by the user kbf runs as ([`traverse`]); the cgroup kbf runs in keeps
//!   every CPU and NUMA node of the node, for a daemon on the container driver
//!   ([`cpuset`]); a mode with no storage half has no AWS credential variable in its environment,
//!   and the cold store's key file beside `kbf.toml`, if there is one, is owned and moded as
//!   its mode needs ([`credential`]).
//!
//! What it does not repeat, because the mode already refuses it with its fix: subordinate
//! ids and cgroup delegation (`kbf-driver-container`), image pins, the cold-store URI, the
//! hot store's room, TLS-or-loopback, and a port already bound.
//!
//! Exit codes: a missing or wrong piece is [`REFUSED`] (2), which the units'
//! `RestartPreventExitStatus=2` leaves down; a piece that heals shortly (a file system
//! that answers busy or timed out, a certificate not yet valid on a clock that is behind)
//! is [`NOT_READY`] (75, `EX_TEMPFAIL`), which systemd restarts after `RestartSec`.
//! Both exit with every problem printed, not the first.

mod certs;
#[cfg(test)]
mod cold_key_file_tests;
#[cfg(test)]
mod cold_registered_tests;
mod cpuset;
mod credential;
mod env;
mod files;
mod needs;
#[cfg(test)]
mod proxy_secret_tests;
mod state_store;
#[cfg(test)]
mod state_store_tests;
#[cfg(test)]
mod tests;
mod traverse;

pub mod host;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub use host::{Host, System};
pub use needs::{needs, needs_in};

use crate::Invocation;

/// The exit status of a start the host refuses: a restart would fail the same way.
pub const REFUSED: u8 = 2;
/// The exit status of a start the host is not ready for: a restart may succeed
/// (`EX_TEMPFAIL`).
pub const NOT_READY: u8 = 75;
/// The open-file limit the units set (`LimitNOFILE`): below it a burst of uploads fails
/// with EMFILE.
pub const NOFILE_FLOOR: u64 = 65_536;
/// The oldest `crun` the container driver is written against.
pub const CRUN_MINIMUM: (u64, u64) = (1, 14);

/// Whether a restart could change the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Missing or wrong; an operator must fix it.
    Refused,
    /// Not there yet; it may be there on the next start.
    NotReady,
}

/// One missing or wrong piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Whether a restart could change it.
    pub class: Class,
    /// What is missing or wrong.
    pub what: String,
    /// Why the mode needs it.
    pub why: String,
    /// How to fix it.
    pub fix: String,
}

/// Everything the checks found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// What stops the start.
    pub problems: Vec<Problem>,
    /// What does not (a certificate already expired: it starts and the reload picks the
    /// renewed one up; a low open-file limit).
    pub warnings: Vec<String>,
}

impl Report {
    /// The exit status of a start this report stops: [`REFUSED`] when any problem is,
    /// else [`NOT_READY`]; `None` when nothing stops it.
    #[must_use]
    pub fn exit_status(&self) -> Option<u8> {
        if self.problems.iter().any(|p| p.class == Class::Refused) {
            Some(REFUSED)
        } else if self.problems.is_empty() {
            None
        } else {
            Some(NOT_READY)
        }
    }

    /// The lines to print for `kbf <mode>`: each warning, then each problem as `kbf
    /// MODE: WHAT (WHY): FIX`.
    #[must_use]
    pub fn render(&self, mode: &str) -> String {
        let mut out = String::new();
        for w in &self.warnings {
            out.push_str(&format!("kbf {mode}: warning: {w}\n"));
        }
        for p in &self.problems {
            out.push_str(&format!("kbf {mode}: {} ({}): {}\n", p.what, p.why, p.fix));
        }
        out
    }

    fn refuse(&mut self, what: String, why: &str, fix: String) {
        self.push(Class::Refused, what, why, fix);
    }

    fn push(&mut self, class: Class, what: String, why: &str, fix: String) {
        self.problems.push(Problem {
            class,
            what,
            why: why.to_owned(),
            fix,
        });
    }
}

/// A file the flags name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileNeed {
    /// The flag that names it.
    pub flag: &'static str,
    /// Where it is.
    pub path: PathBuf,
    /// Whether it holds a secret (a private key, a token): owned by the process's user and
    /// readable by no one else.
    pub secret: bool,
    /// The exact size the file must have, in bytes, where the flag's value is a fixed-size
    /// secret (the proxy's, 32 bytes); `None` for any size.
    pub exact_len: Option<usize>,
}

/// A row of the farm's certificate table (docs/deploy/kbf-units.md, "A node's
/// certificate").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// The frontend's worker listener, and the frontend when it calls storage.
    Frontend,
    /// A storage node's listener, and its calls to other storage nodes.
    Storage,
    /// A daemon's client certificate.
    Daemon,
}

/// A certificate the mode presents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertNeed {
    /// The flag that names it.
    pub flag: &'static str,
    /// Where it is.
    pub path: PathBuf,
    /// The row of the farm's table it must be.
    pub row: Row,
    /// The private key it is presented with, and the flag that names it; `None` where the
    /// mode names no key for it. The key must be PEM and the certificate's own.
    pub key: Option<(&'static str, PathBuf)>,
    /// The CA it must chain to, and the flag that names it; `None` where the CA the
    /// mode holds is not the one that issued this certificate.
    pub ca: Option<(&'static str, PathBuf)>,
    /// The node id a daemon's certificate names exactly.
    pub node_id: Option<String>,
}

/// What the container driver runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerNeed {
    /// `--oci-runtime`: the program that runs the containers.
    pub oci_runtime: PathBuf,
}

/// What one invocation asks of its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needs {
    /// The mode's name, for the messages.
    pub mode: &'static str,
    /// Files to be there, regular, and secret ones to be private.
    pub files: Vec<FileNeed>,
    /// Certificates to be the table's.
    pub certs: Vec<CertNeed>,
    /// The cold store is S3: its key pair is in the environment.
    pub s3: bool,
    /// The container driver runs.
    pub container: Option<ContainerNeed>,
    /// The directory of a `file://` hot store: every directory above it that exists is
    /// one the user kbf runs as can enter ([`traverse`]).
    pub hot_store: Option<PathBuf>,
    /// The container driver places leases on the node's CPUs and NUMA nodes: the cgroup
    /// kbf runs in keeps all of them ([`cpuset`]).
    pub cpuset: bool,
    /// How the mode holds the cold store's key ([`credential`]).
    pub key: KeyRule,
}

/// Where the legacy cold store key file is: `s3.env` beside the `kbf.toml` kbf loads.
#[must_use]
pub fn key_file(config: &Path) -> PathBuf {
    config.with_file_name("s3.env")
}

/// What a mode's rule `credential` asks of the host ([`credential`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum KeyRule {
    /// Nothing (the frontend).
    #[default]
    None,
    /// A mode with no storage half: no AWS credential variable in the environment.
    Absent,
    /// The storage node, which holds the key as a user of its own: the key file, if
    /// there is one, is root's or that user's and no one else can read it.
    Holder(PathBuf),
    /// The mode that holds the key beside its builds: the key file, if there is one, is
    /// root's alone, mode 0600.
    RootAlone(PathBuf),
}

/// Checks `needs` against `host`, everything, in a fixed order.
#[must_use]
pub fn check(needs: &Needs, host: &dyn Host) -> Report {
    let mut report = Report::default();
    files::check(needs, host, &mut report);
    certs::check(needs, host, &mut report);
    env::check(needs, host, &mut report);
    traverse::check(needs, host, &mut report);
    cpuset::check(needs, host, &mut report);
    credential::check(needs, host, &mut report);
    report
}

/// What a start of `invocation` is checked on: its needs against this host, and the flags
/// it must be given. A start and `--check` both print this report, so they agree.
fn findings(invocation: &Invocation, config: &Path) -> (Needs, Report) {
    let needs = needs_in(invocation, config);
    let mut report = check(&needs, &System);
    state_store::check(invocation, &mut report);
    (needs, report)
}

/// The refusal of `invocation`, printed to standard error, if its host does not meet
/// what it needs or its flags leave out a store it cannot start without; `None` to go on
/// and start.
#[must_use]
pub fn refusal(invocation: &Invocation, config: &Path) -> Option<ExitCode> {
    let (needs, report) = findings(invocation, config);
    eprint!("{}", report.render(needs.mode));
    report.exit_status().map(ExitCode::from)
}

/// `kbf <mode> --check`: the checks of a start, printed, and nothing started. Standard
/// output says `ok` when nothing stops the start (warnings and problems go to standard
/// error, as a start prints them); the exit status is the one a start would exit with:
/// 0, [`REFUSED`] or [`NOT_READY`].
#[must_use]
pub fn check_only(invocation: &Invocation, config: &Path) -> ExitCode {
    let (needs, report) = findings(invocation, config);
    eprint!("{}", report.render(needs.mode));
    match report.exit_status() {
        Some(code) => ExitCode::from(code),
        None => {
            println!("kbf {}: ok", needs.mode);
            ExitCode::SUCCESS
        }
    }
}
