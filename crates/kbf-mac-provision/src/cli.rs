//! The command line: `check`, `apply`, `print`, `restart` and `revert`, and their exit
//! statuses (0 every item passes or the verb did what it says, 1 an item fails or a
//! setting `revert` did not put back, 2 bad usage, a refused profile, or a verb that
//! refused or could not finish).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::host::{Host, Mac, RealHost};
use crate::items::{self, Ctx, UnknownItem};
use crate::launchd::{self, RestartError};
use crate::profile::{Profile, ProfileError};
use crate::record::{self, RevertError};

#[derive(Debug, clap::Parser)]
#[command(
    name = "kbf-mac-provision",
    version,
    about = "Checks and applies a Mac node's host settings and worker profile from a profile file"
)]
pub struct Cli {
    #[command(subcommand)]
    pub verb: Verb,
}

#[derive(Debug, clap::Subcommand)]
pub enum Verb {
    /// One PASS or FAIL line per item, then a count; exit 1 on any FAIL. Changes nothing.
    Check {
        /// The profile file.
        #[arg(long)]
        profile: PathBuf,
        /// Check only these items (comma-separated keys).
        #[arg(long)]
        keys: Option<String>,
        /// The Mac's `/` (an absolute path); only tests set it.
        #[arg(long, default_value = "/", value_parser = absolute)]
        root: String,
    },
    /// As root: fixes each failing item that is not check only, one CHANGE line per
    /// change, then prints what still fails; exit 1 if anything does.
    Apply {
        /// The profile file.
        #[arg(long)]
        profile: PathBuf,
        /// Apply only these items (comma-separated keys).
        #[arg(long)]
        keys: Option<String>,
        /// The Mac's `/` (an absolute path); only tests set it.
        #[arg(long, default_value = "/", value_parser = absolute)]
        root: String,
    },
    /// The resolved profile, its SHA-256 and the node label.
    Print {
        /// The profile file.
        #[arg(long)]
        profile: PathBuf,
    },
    /// As root: restarts kbf-daemon's job, emptying its error log while it is stopped.
    Restart {
        /// The profile file.
        #[arg(long)]
        profile: PathBuf,
        /// The Mac's `/` (an absolute path); only tests set it.
        #[arg(long, default_value = "/", value_parser = absolute)]
        root: String,
    },
    /// As root: puts back the power and software update settings `apply` recorded
    /// before it first changed one; exit 1, keeping the record, if one is not put back.
    Revert {
        /// The Mac's `/` (an absolute path); only tests set it.
        #[arg(long, default_value = "/", value_parser = absolute)]
        root: String,
    },
}

impl Verb {
    /// The Mac's `/` the verb works on; `print` reads only the profile.
    pub fn root(&self) -> &str {
        match self {
            Self::Check { root, .. }
            | Self::Apply { root, .. }
            | Self::Restart { root, .. }
            | Self::Revert { root } => root,
            Self::Print { .. } => "/",
        }
    }
}

fn absolute(root: &str) -> Result<String, String> {
    if root.starts_with('/') {
        Ok(root.to_owned())
    } else {
        Err("--root must be an absolute path".into())
    }
}

/// Why a verb did not run, or did not finish. The binary prints it and exits 2.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Keys(#[from] UnknownItem),
    #[error("{0} runs as root")]
    NotRoot(&'static str),
    #[error(transparent)]
    Restart(#[from] RestartError),
    #[error(transparent)]
    Revert(#[from] RevertError),
}

fn need_root(host: &dyn Host, verb: &'static str) -> Result<(), Error> {
    if host.euid() == 0 {
        Ok(())
    } else {
        Err(Error::NotRoot(verb))
    }
}

/// `check`: the report, and whether every item passed.
///
/// # Errors
/// The profile is refused, or `keys` names no item.
pub fn check(profile: &Path, keys: Option<&str>, host: &dyn Host) -> Result<items::Report, Error> {
    let profile = Profile::load(profile)?;
    let selected = items::select(keys)?;
    Ok(items::check(host, &profile, &selected))
}

/// `apply`: what it changed and what still fails.
///
/// # Errors
/// The profile is refused, this is not root, or `keys` names no item.
pub fn apply(profile: &Path, keys: Option<&str>, host: &dyn Host) -> Result<items::Report, Error> {
    let profile = Profile::load(profile)?;
    need_root(host, "apply")?;
    let selected = items::select(keys)?;
    Ok(items::apply(host, &profile, &selected))
}

/// `print`: what it prints.
///
/// # Errors
/// The profile is refused.
pub fn print(profile: &Path) -> Result<String, Error> {
    Ok(Profile::load(profile)?.print())
}

/// `restart`: what it prints.
///
/// # Errors
/// The profile is refused, this is not root, the error log is not safe to empty
/// (nothing is stopped), or the job does not start again.
pub fn restart(profile: &Path, host: &dyn Host) -> Result<String, Error> {
    let profile = Profile::load(profile)?;
    need_root(host, "restart")?;
    let ctx = Ctx {
        mac: Mac::new(host),
        profile: &profile,
        key: "launchd_label",
    };
    launchd::restart_job(&ctx)?;
    Ok(format!(
        "CHANGE launchd_label: restarted system/{} (error log truncated)\n",
        profile.get("launchd_label")
    ))
}

/// `revert`: what it prints, and how many settings it did not put back.
///
/// # Errors
/// This is not root, or the record is missing or refused (nothing is changed), or it
/// could not be moved aside.
pub fn revert(host: &dyn Host) -> Result<items::Report, Error> {
    need_root(host, "revert")?;
    Ok(record::revert(host)?)
}

/// Runs `verb` on the Mac `host`: what it prints on stdout, and its exit status.
///
/// # Errors
/// Why the verb did not run or did not finish (exit status 2).
pub fn execute(verb: &Verb, host: &dyn Host) -> Result<(String, u8), Error> {
    let status = |r: items::Report| (r.text, u8::from(r.failed != 0));
    match verb {
        Verb::Check { profile, keys, .. } => check(profile, keys.as_deref(), host).map(status),
        Verb::Apply { profile, keys, .. } => apply(profile, keys.as_deref(), host).map(status),
        Verb::Print { profile } => print(profile).map(|text| (text, 0)),
        Verb::Restart { profile, .. } => restart(profile, host).map(|text| (text, 0)),
        Verb::Revert { .. } => revert(host).map(status),
    }
}

/// Runs the verb on this Mac. What it prints goes to stdout, a refusal to stderr.
pub fn main(cli: &Cli) -> ExitCode {
    match execute(&cli.verb, &RealHost::new(cli.verb.root())) {
        Ok((text, status)) => {
            // An unwritable stdout is a failure of its own: the caller saw no verdict.
            let mut stdout = std::io::stdout().lock();
            let written = stdout
                .write_all(text.as_bytes())
                .and_then(|()| stdout.flush());
            ExitCode::from(written.map_or(2, |()| status))
        }
        Err(e) => {
            eprintln!("kbf-mac-provision: {e}");
            ExitCode::from(2)
        }
    }
}
