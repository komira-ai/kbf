//! `release version-facts` (docs/artifacts.md): the signatures only, with bodies that do not
//! do what their names say, so that the tests of this commit fail by assertion and not by a
//! compile error. The next commit builds it.

use std::path::PathBuf;
use std::time::Duration;

use crate::Error;
use crate::package::Platform;

/// The binary whose facts are kept: the one a node runs.
pub const BINARY: &str = "kbf";

/// What the facts file's name adds to the stem of the tarball's.
pub const SUFFIX: &str = ".version.json";

/// How long the binary has to print its facts.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The arguments of `release version-facts`.
#[derive(clap::Args)]
pub struct Args {
    /// The `kbf` just built (on macOS, signed): it is run with `--version --json`.
    #[arg(long)]
    pub binary: PathBuf,
    /// The commit it was built from, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub commit: String,
    /// The platform it was built for.
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// The directory the facts and their line of `SHA256SUMS.<platform>` go in; created
    /// when missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// The name of the facts file of the `kbf` built from `commit` for `platform`.
pub fn name(_commit: &str, _platform: Platform) -> String {
    String::new()
}

/// The facts file that goes with `asset`, when `asset` is a `kbf` tarball.
pub fn facts_of(_asset: &str) -> Option<String> {
    None
}

/// The `release version-facts` verb with `timeout` for the binary: the lines to print.
pub fn emit(_a: &Args, _timeout: Duration) -> Result<Vec<String>, Error> {
    Err(Error::Failed("not built yet".to_owned()))
}

/// The `release version-facts` verb: the lines to print.
pub fn run(a: &Args) -> Result<Vec<String>, Error> {
    emit(a, TIMEOUT)
}

#[cfg(test)]
mod tests;
