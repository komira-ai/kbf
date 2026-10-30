//! `wait-artifacts`: waits until the artifacts of the sibling jobs of a run have been
//! uploaded, or one of those jobs has failed (not written yet: the tests come first).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Error;

/// The arguments of `wait-artifacts`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>`.
    #[arg(long)]
    pub repo: String,
    /// The run whose artifacts and jobs are looked at.
    #[arg(long)]
    pub run_id: u64,
    /// The exact name of an artifact to wait for; once for each.
    #[arg(long = "artifact", required = true)]
    pub artifacts: Vec<String>,
    /// A job whose name contains this and which did not succeed ends the wait.
    #[arg(long)]
    pub job_prefix: String,
    /// Seconds between two looks at the run.
    #[arg(long, default_value_t = 15)]
    pub poll_seconds: u64,
    /// The most looks at the run.
    #[arg(long, default_value_t = 240)]
    pub polls: u32,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// What the verb asks of GitHub, so that its decisions are tested without it.
pub trait Github {
    /// The JSON reply to a GET of `path`.
    fn get(&mut self, path: &str) -> Result<Value, Error>;
    /// Wait before the next look.
    fn pause(&mut self);
}

/// Waits as the module docs will say.
pub fn wait(_github: &mut dyn Github, _args: &Args) -> Result<String, Error> {
    Err(Error::Failed("wait-artifacts: not written".to_owned()))
}

/// `wait-artifacts`: the lines to print when it finished.
pub fn run(_args: &Args, _root: &Path, _cargo: &std::ffi::OsStr) -> Result<Vec<String>, Error> {
    Err(Error::Failed("wait-artifacts: not written".to_owned()))
}

#[cfg(test)]
mod tests;
