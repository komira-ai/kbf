//! The command line: `check` and `print`, and their exit statuses (0 every item
//! passes, 1 one fails, 2 bad usage or a refused profile).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::host::{Host, RealHost};
use crate::items::{self, UnknownItem};
use crate::profile::{Profile, ProfileError};

#[derive(Debug, clap::Parser)]
#[command(
    name = "kbf-mac-provision",
    version,
    about = "Checks a Mac node's host settings and worker profile against a profile file"
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
    /// The resolved profile, its SHA-256 and the node label.
    Print {
        /// The profile file.
        #[arg(long)]
        profile: PathBuf,
    },
}

fn absolute(root: &str) -> Result<String, String> {
    if root.starts_with('/') {
        Ok(root.to_owned())
    } else {
        Err("--root must be an absolute path".into())
    }
}

/// Why a verb did not run. The binary prints it and exits 2.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Keys(#[from] UnknownItem),
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

/// `print`: what it prints.
///
/// # Errors
/// The profile is refused.
pub fn print(profile: &Path) -> Result<String, Error> {
    Ok(Profile::load(profile)?.print())
}

/// Runs the verb on this Mac. What it prints goes to stdout, a refusal to stderr.
pub fn main(cli: &Cli) -> ExitCode {
    let result = match &cli.verb {
        Verb::Check {
            profile,
            keys,
            root,
        } => check(profile, keys.as_deref(), &RealHost::new(root.as_str()))
            .map(|r| (r.text, if r.failed == 0 { 0 } else { 1 })),
        Verb::Print { profile } => print(profile).map(|text| (text, 0)),
    };
    match result {
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
