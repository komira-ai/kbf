//! The `package` kit: what the kbf `.deb` does to a Linux host when dpkg installs,
//! stops, removes or purges it (docs/design/install-and-enrollment.md, sections 3.3 and
//! 3.5). The maintainer scripts are a small Rust executable that runs these verbs, so no
//! shell logic exists to budget; each verb is also reachable here. Every verb is
//! idempotent, prints its plan as `DRY-RUN:` lines and changes the host only with
//! `--apply` (the executable passes it), and a run after an apply plans nothing.
//!
//! - `kbf-deploy package postinst [--apply]` ([`postinst`]): checks the dependencies
//!   (`crun`, `newuidmap`, `newgidmap`; never installs them, and never Podman), creates
//!   the `kbf` group and system user, gives it a block of subordinate uids and gids that
//!   overlaps no range in `/etc/subuid` and `/etc/subgid` ([`subid`]), enables lingering,
//!   makes the config and state directories with their modes, copies the offline
//!   verification trust files to `/usr/local/libexec/kbf-deploy`, installs the release
//!   the package ships beside the updater's releases ([`release`]), reloads systemd when
//!   it says a unit changed and enables `kbf-node` once the node has what it starts from.
//! - `kbf-deploy package prerm [--upgrade] [--apply]` ([`remove`]): stops `kbf-node`
//!   (it drains on SIGTERM, and the unit's `TimeoutStopSec=infinity` stays) unless the
//!   package is being upgraded.
//! - `kbf-deploy package postrm [--purge [--purge-data]] [--apply]` ([`remove`]): after
//!   a remove, reloads systemd; after a purge, also removes `/etc/kbf`, the node's keys
//!   (`/var/lib/kbf/pki`), the copied trust files, the lingering and the `kbf` user and
//!   group with its subordinate ranges. The hot store, metadata log and data directories,
//!   and the releases with the deploy state, stay unless `--purge-data` is given too.
//!
//! The executable maps dpkg's words: `postinst configure` is `postinst`; `prerm remove`
//! (and `deconfigure`) is `prerm`, `prerm upgrade` is `prerm --upgrade`; `postrm remove`
//! is `postrm`, `postrm purge` is `postrm --purge`; every other word does nothing.
//!
//! There is no `--root`: the verbs run `useradd`, `loginctl` and `systemctl`, which act
//! on the running system whatever directory the files are read from. Tests run the
//! library functions on a scratch root ([`Env`]).
//!
//! The package and the pull updater share the host (design section 3.5): apt owns the
//! root helper, `kbf-deploy`, the units and the trust files; the updater owns the release
//! that runs. So `postinst` points `current` at the shipped release only when no
//! `current` link exists and the deploy state names none (a first install), and never
//! moves `current` or `previous` after that: a package upgrade installs its release
//! beside the others and activates nothing.
//!
//! Exit codes: 0 done (or nothing to do), 1 a step failed, 2 refused (a missing
//! dependency, a subordinate range to clash with, a shipped release that differs from
//! the one installed, a state file it cannot read), with a `REFUSED:` line on stderr.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use kbf_host::exec::Exec;
use kbf_host::path::{HostPath, PathError, Root};
use kbf_host::plan::{HostError, Mode, Plan, RunAs, execute};
use kbf_host::step::Owner;
use kbf_host::user::UserError;

use crate::host::release::{ROOT, ReleaseError};
use crate::host::state::StateError;

pub mod account;
pub mod postinst;
pub mod release;
pub mod remove;
pub mod subid;

#[cfg(test)]
mod testkit;

/// The service account and its group.
pub const USER: &str = "kbf";
/// The node's config directory: `kbf.toml`, group-readable by [`USER`].
pub const ETC: &str = "/etc/kbf";
/// The node's own directory: its home, keys (`pki/`), hot store, metadata log and data.
pub const STATE: &str = "/var/lib/kbf";
/// The node's token file, which the node unlinks after it enrolls.
pub const TOKEN: &str = "/var/lib/kbf/enroll.token";
/// The node's keys, certificates and `node.json`, which `kbf enroll` writes.
pub const PKI: &str = "/var/lib/kbf/pki";
/// What `kbf enroll` writes once the node is enrolled.
pub const NODE_JSON: &str = "/var/lib/kbf/pki/node.json";
/// The node's config, written by the operator.
pub const CONFIG: &str = "/etc/kbf/kbf.toml";
/// The unit the package ships.
pub const UNIT: &str = "kbf-node.service";

/// Where dpkg puts the release the package ships, never modified.
pub const SHIPPED_RELEASE: &str = "/usr/lib/kbf/release";
/// Where dpkg puts the pinned `gh`, the trusted root and the default installer settings.
pub const SHIPPED_LIBEXEC: &str = "/usr/lib/kbf/libexec";
/// Where offline verification reads them from (root's, so the node's user cannot
/// replace what decides which releases are trusted).
pub const TRUST_DIR: &str = "/usr/local/libexec/kbf-deploy";

/// The tools the verbs run, where Debian and Ubuntu put them.
pub const GROUPADD: &str = "/usr/sbin/groupadd";
pub const GROUPDEL: &str = "/usr/sbin/groupdel";
pub const USERADD: &str = "/usr/sbin/useradd";
pub const USERDEL: &str = "/usr/sbin/userdel";
pub const USERMOD: &str = "/usr/sbin/usermod";
pub const LOGINCTL: &str = "/usr/bin/loginctl";
pub const NOLOGIN: &str = "/usr/sbin/nologin";
pub const CRUN: &str = "/usr/bin/crun";
pub const NEWUIDMAP: &str = "/usr/bin/newuidmap";
pub const NEWGIDMAP: &str = "/usr/bin/newgidmap";

/// Where a host being booted by systemd has this directory (`sd_booted(3)`).
pub const SYSTEMD_RUNNING: &str = "/run/systemd/system";
/// Where lingering users are recorded; `loginctl enable-linger` creates the file, and
/// without a running systemd (a container build) the verb does.
pub const LINGER_DIR: &str = "/var/lib/systemd/linger";

/// The `package` verbs of the `kbf-deploy` command line ([`crate::cli`]).
#[derive(Subcommand)]
pub enum Package {
    /// After the package is unpacked: account, directories, trust files, first release.
    /// Prints the plan unless --apply.
    Postinst(ApplyArgs),
    /// Before the package is removed or upgraded: stop kbf-node. Prints the plan unless
    /// --apply.
    Prerm(PrermArgs),
    /// After the package is removed or purged. Prints the plan unless --apply.
    Postrm(PostrmArgs),
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Carry the plan out (as root).
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args)]
pub struct PrermArgs {
    /// The package is being upgraded: leave the running node alone.
    #[arg(long)]
    pub upgrade: bool,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

#[derive(Args)]
pub struct PostrmArgs {
    /// The package is purged: remove its configuration, keys, account and trust files.
    #[arg(long)]
    pub purge: bool,
    /// With --purge, also remove the hot store, metadata log and data directories, the
    /// releases and the deploy state.
    #[arg(long, requires = "purge")]
    pub purge_data: bool,
    #[command(flatten)]
    pub apply: ApplyArgs,
}

impl ApplyArgs {
    fn mode(&self) -> Mode {
        if self.apply {
            Mode::Apply
        } else {
            Mode::DryRun
        }
    }
}

/// The host the verbs work on: the directory every path is read under (`/`, or a scratch
/// directory in a test), and the owner of what root writes (a test can chown only to its
/// own user).
#[derive(Clone, Debug)]
pub struct Env {
    pub root: Root,
    pub root_owner: Owner,
}

impl Env {
    /// This machine, as root.
    #[must_use]
    pub fn host() -> Env {
        Env {
            root: Root::host(),
            root_owner: ROOT,
        }
    }
}

/// Why a verb refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error(
        "{what} is missing or does not run ({why}): it is a dependency of this package (Depends: crun, uidmap), which this verb never installs"
    )]
    Dependency { what: &'static str, why: String },
    #[error("a user named {USER} has uid 0: the node must not run as root")]
    RootUser,
    #[error(transparent)]
    SubId(#[from] subid::SubIdError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error(transparent)]
    Release(#[from] ReleaseError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    User(#[from] UserError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Linux(#[from] crate::linux::HostError),
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{0} is not a regular file or directory the package ships")]
    Shipped(PathBuf),
    #[error("{path} is not a manifest naming the release's commit: {why}")]
    Manifest { path: PathBuf, why: String },
    #[error("{0:?} is not a UTF-8 name")]
    NotUtf8(PathBuf),
    #[error("the copy at {path} has sha256 {got}; the shipped release has {want}")]
    Copy {
        path: HostPath,
        want: String,
        got: String,
    },
    #[error("cannot write the plan out: {0}")]
    Output(io::Error),
}

impl PackageError {
    /// 1 when a step of the plan failed, 2 for every refusal.
    fn exit(&self) -> u8 {
        match self {
            PackageError::Host(HostError::Step { .. } | HostError::Output(_))
            | PackageError::Linux(crate::linux::HostError::Deploy(HostError::Step { .. }))
            | PackageError::Copy { .. } => 1,
            _ => 2,
        }
    }
}

/// The error for a failed read of `path`, for `.map_err`.
pub(crate) fn read_failed(path: &Path) -> impl FnOnce(io::Error) -> PackageError + '_ {
    move |source| PackageError::Read {
        path: path.to_owned(),
        source,
    }
}

/// Print `line` to `out`.
pub(crate) fn say(out: &mut dyn Write, line: &str) -> Result<(), PackageError> {
    writeln!(out, "{line}").map_err(PackageError::Output)
}

/// Print `plan` under the heading `title` and carry it out in `mode`.
pub(crate) fn phase<E: Exec>(
    exec: &mut E,
    title: &str,
    plan: &Plan,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), PackageError> {
    say(out, &format!("{title}:"))?;
    execute(plan, RunAs::Root, mode, exec, &mut out)?;
    Ok(())
}

/// One of this module's path constants.
pub(crate) fn path(path: &str) -> HostPath {
    HostPath::new(path).expect("a constant of this kit is an absolute host path")
}

/// Run a verb on the host `exec` and `env` stand for (this machine, from the command
/// line): see the module docs for each verb and its exit.
pub fn run<E: Exec>(
    verb: Package,
    exec: &mut E,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let done = match verb {
        Package::Postinst(a) => postinst::run(exec, env, a.mode(), out),
        Package::Prerm(a) => remove::prerm(exec, a.upgrade, a.apply.mode(), out),
        Package::Postrm(a) => {
            let what = remove::Postrm {
                purge: a.purge,
                purge_data: a.purge_data,
            };
            remove::postrm(exec, env, what, a.apply.mode(), out)
        }
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let code = e.exit();
            let _ = if code == 1 {
                writeln!(err, "kbf-deploy: {e}")
            } else {
                writeln!(err, "REFUSED: {e}")
            };
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
