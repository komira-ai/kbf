//! `kbf-installer setup`: the first configuration of a host (docs/design/kbf-installer.md
//! sections 2 and 3, docs/design/distribution.md section 4).
//!
//! It validates its flags, then makes the host what they say: the fetch account (and for the
//! modes that run actions, the `kbf` user's subid range and lingering), the installer's tree
//! with its `orders/` directory, the enrollment token (node modes), the first operator token
//! (frontend and all-in-one hosts, once), `/etc/kbf/installer.toml`, the first release
//! (`--commit`), `/etc/kbf/kbf.toml`, and last the units enabled for the mode.
//!
//! - It is root's: [`Os::euid`] is checked first, so only a fake that says 0 gets past it
//!   without root. On a frontend or all-in-one host the installed Envoy is checked against
//!   the tested range before anything else (`envoy_range`).
//! - Files are written by `kbf-host` steps, each a temporary name, fsync, rename and a sync
//!   of the directory. Without `--apply` the steps are printed as `DRY-RUN:` and nothing
//!   changes.
//! - Idempotent: steps are planned only where the host differs. The same flags again is an
//!   empty plan; changed flags rewrite the files atomically and never restart a running
//!   service. An operator floor (`min_generation`) only goes up.
//! - The first release comes through [`Releases`], the installer's one downloader (the fetch
//!   child and the verify pipeline); the binary's own is [`NoReleases`] until those land.
//! - `kbf.toml` is written after every file a unit needs, and the units after it.

pub mod kbf_toml;
pub mod os;
pub mod paths;
pub mod plan;
pub mod validate;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::io::{self, Write};
use std::path::PathBuf;

use kbf_host::{Entry, Exec, HostError, HostExec, HostPath, PathError, Root, Untrusted};

use self::os::{Os, SystemOs};
use self::validate::{Raw, Valid};
use crate::CommitId;
use crate::cli::{Mode, SetupArgs};
use crate::envoy::VersionError;
use crate::exit::Exit;
use crate::installer_toml::{InstallerToml, Source};
use crate::layout::Platform;

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
    #[error(
        "no user {name}: the package creates it (systemd-sysusers); run it, or reinstall the package"
    )]
    NoUser { name: &'static str },
    #[error("no room for a {0} range of 65536 ids above the existing ones")]
    SubIdSpace(&'static str),
    #[error("no free id between 300 and 399 for the fetch account")]
    NoFreeId,
    #[error("envoy_range: {0}")]
    EnvoyRange(VersionError),
    /// Only a person can go on: the reason, and what to do about it.
    #[error("{0}")]
    NeedsOperator(String),
    /// A step the installer does not have yet.
    #[error("{0}")]
    NotBuilt(String),
    #[error("{0}")]
    Path(#[from] PathError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Host(#[from] HostError),
}

impl SetupError {
    /// The exit code (contract section 2).
    pub fn exit(&self) -> Exit {
        match self {
            SetupError::NotRoot { .. }
            | SetupError::Untrusted(_)
            | SetupError::Invalid(_)
            | SetupError::Token(_)
            | SetupError::Unsupported(_)
            | SetupError::NoUser { .. } => Exit::Usage,
            SetupError::EnvoyRange(_) => Exit::Refused,
            SetupError::NeedsOperator(_) => Exit::NeedsOperator,
            SetupError::SubIdSpace(_)
            | SetupError::NoFreeId
            | SetupError::NotBuilt(_)
            | SetupError::Path(_)
            | SetupError::Io(_)
            | SetupError::Host(_) => Exit::Failed,
        }
    }

    /// A stable word for `--json`.
    pub fn code(&self) -> &'static str {
        match self {
            SetupError::NotRoot { .. } => "needs_root",
            SetupError::Untrusted(_) => "untrusted_binary",
            SetupError::Invalid(_) => "invalid_flags",
            SetupError::Token(_) => "token_file",
            SetupError::Unsupported(_) => "unsupported",
            SetupError::NoUser { .. } => "no_user",
            SetupError::SubIdSpace(_) | SetupError::NoFreeId => "no_room",
            SetupError::EnvoyRange(_) => "envoy_range",
            SetupError::NeedsOperator(_) => "needs_operator",
            SetupError::NotBuilt(_) => "not_implemented",
            SetupError::Path(_) | SetupError::Io(_) | SetupError::Host(_) => "failed",
        }
    }
}

/// The installer's one downloader, as `setup` uses it: fetch the release for `commit` from
/// `source`, verify it as an update is verified, and install it as `releases/<commit>`
/// with `releases/current` pointing at it.
pub trait Releases {
    fn install_first(
        &self,
        host: &mut dyn Exec,
        commit: &CommitId,
        source: &Source,
    ) -> Result<(), SetupError>;
}

/// The downloader of a build without the fetch child (contract P7) and the verify pipeline
/// (P8): it installs nothing, and says so.
pub struct NoReleases;

impl Releases for NoReleases {
    fn install_first(
        &self,
        _: &mut dyn Exec,
        commit: &CommitId,
        _: &Source,
    ) -> Result<(), SetupError> {
        Err(SetupError::NotBuilt(format!(
            "cannot install release {commit}: fetching a release needs the fetch child (contract P7) and the verify pipeline (P8), which this build does not have; setup stopped before it wrote kbf.toml"
        )))
    }
}

/// What `setup` was asked to do besides the flags.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Carry the plan out (`--apply`); otherwise print it.
    pub apply: bool,
    /// Enable the units without starting them.
    pub no_start: bool,
}

/// The `installer.toml` this run leaves: the file that is there with the flags' settings put
/// in, or the defaults with them. A file that is not valid is not overwritten (the operator
/// decides), and a floor never goes down.
pub fn installer_file<E: Exec>(
    exec: &E,
    v: &Valid,
    platform: Platform,
) -> Result<String, SetupError> {
    let path = HostPath::new(paths::INSTALLER_TOML)?;
    let mut settings = match exec.inspect(&path)? {
        None => InstallerToml::default(),
        Some(Entry::File { bytes, .. }) => {
            let text = String::from_utf8_lossy(&bytes);
            InstallerToml::parse(&text).map_err(|e| {
                SetupError::NeedsOperator(format!(
                    "{e}\nfix {} or remove it, then run setup again",
                    paths::INSTALLER_TOML
                ))
            })?
        }
        Some(_) => {
            return Err(SetupError::NeedsOperator(format!(
                "{} is not a regular file",
                paths::INSTALLER_TOML
            )));
        }
    };
    if let Some(floor) = v.min_generation {
        if floor < settings.min_generation {
            return Err(SetupError::NeedsOperator(format!(
                "--min-generation {floor} is below the {} in {}: a floor only goes up; lower it by editing the file",
                settings.min_generation,
                paths::INSTALLER_TOML
            )));
        }
        settings.min_generation = floor;
    }
    settings.source = v.source.clone();
    (settings.node_cert, settings.node_key) = match (&v.source, node_role(v.mode)) {
        (Source::Frontend, Some(role)) => {
            let (cert, key) = paths::node_identity(platform, role);
            (Some(PathBuf::from(cert)), Some(PathBuf::from(key)))
        }
        _ => (None, None),
    };
    Ok(settings.render())
}

/// The role whose certificate and key the fetch child is lent: the daemon's if the host has
/// one, else the storage node's (contract section 3).
fn node_role(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Daemon | Mode::StorageAndDaemon => Some("daemon"),
        Mode::Storage => Some("storage"),
        Mode::Frontend | Mode::AllInOne => None,
    }
}

/// Whether a release is installed: `releases/current` is there (a link counts).
fn released<E: Exec>(exec: &E, platform: Platform) -> Result<bool, SetupError> {
    let current = HostPath::new(&paths::current_release(platform))?;
    Ok(exec.stat(&current)?.is_some() || exec.inspect(&current)?.is_some())
}

/// Print the steps of `plan` and, when applying, carry them out. Returns how many there were.
fn emit<E: Exec, W: Write + ?Sized>(
    plan: &kbf_host::Plan,
    exec: &mut E,
    apply: bool,
    out: &mut W,
) -> Result<usize, SetupError> {
    for (index, step) in plan.steps().iter().enumerate() {
        if apply {
            writeln!(out, "+ {step}")?;
            exec.apply(step).map_err(|source| HostError::Step {
                index,
                step: step.to_string(),
                source,
            })?;
        } else {
            writeln!(out, "DRY-RUN: {step}")?;
        }
    }
    Ok(plan.steps().len())
}

/// Validate, plan and (with `options.apply`) carry out `setup` for `raw` on the host `os` and
/// `exec` describe, printing each step and each note (never a secret).
pub fn run<O: Os, E: Exec, R: Releases, W: Write + ?Sized>(
    os: &O,
    exec: &mut E,
    releases: &R,
    raw: &Raw,
    options: Options,
    out: &mut W,
) -> Result<(), SetupError> {
    let euid = os.euid();
    if euid != 0 {
        return Err(SetupError::NotRoot { euid });
    }
    if options.apply {
        os.trusted_binary()?;
    }
    let valid = raw.validate().map_err(SetupError::Invalid)?;
    let ctx = plan::context(os, valid.mode)?;
    if matches!(valid.mode, Mode::Frontend | Mode::AllInOne) {
        os.envoy().map_err(SetupError::EnvoyRange)?;
    }
    let mut notes = Vec::new();

    let before = plan::before_release(os, &*exec, &valid, &ctx)?;
    let mut steps = emit(&before.plan, exec, options.apply, out)?;
    notes.extend(before.notes);

    let mut have_release = released(&*exec, ctx.platform)?;
    match (&valid.commit, have_release) {
        (Some(commit), false) => {
            steps += 1;
            if options.apply {
                writeln!(out, "+ fetch and verify release {commit}")?;
                releases.install_first(&mut *exec, commit, &valid.source)?;
                have_release = released(&*exec, ctx.platform)?;
                if !have_release {
                    return Err(SetupError::NotBuilt(format!(
                        "release {commit} was fetched but releases/current is not there: setup stopped before it wrote kbf.toml"
                    )));
                }
            } else {
                writeln!(out, "DRY-RUN: fetch and verify release {commit}")?;
                have_release = true;
            }
        }
        (Some(commit), true) => notes.push(format!(
            "a release is installed already: --commit {commit} is not fetched (setup installs the first release only; use update)"
        )),
        (None, _) => {}
    }

    let after = plan::after_release(os, &*exec, &valid, &ctx, have_release, options.no_start)?;
    steps += emit(&after.plan, exec, options.apply, out)?;
    notes.extend(after.notes);

    if steps == 0 {
        writeln!(out, "{}", kbf_host::plan::NOTHING_TO_DO)?;
    } else if !options.apply {
        writeln!(out, "{}", kbf_host::plan::DRY_RUN_NOTE)?;
    }
    for note in &notes {
        writeln!(out, "note: {note}")?;
    }
    Ok(())
}

/// The verb as the binary runs it, on the host under `root`.
pub fn cli(
    root: PathBuf,
    args: &SetupArgs,
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), SetupError> {
    let mut exec = HostExec::new(Root::new(root)?);
    // `os` reads through its own copy of the host, so `run` can change the real one.
    let reader = exec.clone();
    let os = SystemOs::new(&reader, Platform::current());
    let raw = Raw {
        mode: args.mode,
        frontend_url: args.frontend_url.clone(),
        token_file: args.token_file.clone(),
        release_source: args.release_source,
        release_url: args.release_url.clone(),
        min_generation: args.min_generation,
        commit: args.commit.clone(),
    };
    let options = Options {
        apply,
        no_start: args.no_start,
    };
    run(&os, &mut exec, &NoReleases, &raw, options, out)
}
