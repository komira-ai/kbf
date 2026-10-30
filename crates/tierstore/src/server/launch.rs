//! From the command line to the settings: the flags as they are, or the file `--config` names.
//!
//! Without `--config` the flags are parsed as they always were: the same usage, the same
//! refusals. With it, every flag that describes the data path is refused (the file holds
//! them), the knobs of the run (the two parts of the drain) are read from the flags, and the
//! settings are those of the file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::parser::ValueSource;
use clap::{ArgMatches, Command, CommandFactory, Parser};

use super::cli::Cli;
use super::config::{
    ConfigError, DEFAULT_DRAIN_DEADLINE, DEFAULT_DRAIN_LB_WAIT, PER_RUN, PerRun, Settings, load,
};

/// What the command line made.
#[derive(Debug)]
pub(super) struct Launch {
    /// The settings: the flags', or those of the file (of either shape).
    pub settings: Settings,
    /// Whether they were read from a file, so that a refusal speaks of its keys.
    pub from_file: bool,
}

/// Why there are no settings.
#[derive(Debug)]
pub(super) enum Refused {
    /// The parser refused the flags, or was asked for the usage or the version.
    Usage(clap::Error),
    /// The file, or its use with the flags, was refused.
    Config(ConfigError),
}

/// The settings that `args` (the program name first) stand for.
///
/// # Errors
/// [`Refused::Usage`] for flags the parser refuses, and [`Refused::Config`] for a file that is
/// refused or given with flags that describe the data path.
pub(super) fn resolve(args: Vec<OsString>) -> Result<Launch, Refused> {
    let command = Cli::command();
    // With --config the flags the file replaces are not needed, so they are not required here;
    // an argument line that fails this parse (the usage, the version, a value that is not valid)
    // is parsed again below, where the parser says what is wrong exactly as it always has.
    let relaxed = command.clone().mut_args(|arg| arg.required(false));
    let given = relaxed.try_get_matches_from(args.clone()).ok();
    let path = given
        .as_ref()
        .and_then(|matches| matches.get_one::<PathBuf>("config"));
    if let (Some(matches), Some(path)) = (given.as_ref(), path) {
        let settings = from_file(&command, matches, path).map_err(Refused::Config)?;
        return Ok(Launch {
            settings,
            from_file: true,
        });
    }
    let cli = Cli::try_parse_from(args).map_err(Refused::Usage)?;
    Ok(Launch {
        settings: Settings::Storage(Box::new(cli)),
        from_file: false,
    })
}

/// The settings of the file at `path`, given the flags `matches` holds.
fn from_file(
    command: &Command,
    matches: &ArgMatches,
    path: &Path,
) -> Result<Settings, ConfigError> {
    if matches.get_flag("print_config") {
        return Err(ConfigError::PrintWithConfig);
    }
    let flags: Vec<String> = command
        .get_arguments()
        .filter(|arg| arg.get_long().is_some_and(|long| !PER_RUN.contains(&long)))
        .filter(|arg| matches.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine))
        .filter_map(|arg| arg.get_long())
        .map(|long| format!("--{long}"))
        .collect();
    if !flags.is_empty() {
        return Err(ConfigError::Conflict { flags });
    }
    let per_run = PerRun {
        drain_lb_wait: matches
            .get_one::<Duration>("drain_lb_wait")
            .copied()
            .unwrap_or(DEFAULT_DRAIN_LB_WAIT),
        drain_deadline: matches
            .get_one::<Duration>("drain_deadline")
            .copied()
            .unwrap_or(DEFAULT_DRAIN_DEADLINE),
    };
    load(path, per_run)
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
