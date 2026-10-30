//! `profiles-complete`: refuses a directory of raw coverage profiles in which a shard's
//! profiles are missing (not written yet: the tests come first).

use std::path::PathBuf;

use clap::Args as ClapArgs;

use crate::Error;

/// Flags of `profiles-complete`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The directory the profiles are in.
    #[arg(long)]
    pub dir: PathBuf,
    /// The file-name prefix of one shard's profiles; once for each shard.
    #[arg(long = "prefix", required = true)]
    pub prefixes: Vec<String>,
}

/// Check the directory.
pub fn run(_args: &Args) -> Result<Vec<String>, Error> {
    Err(Error::Refused(vec![
        "profiles-complete: not written".to_owned(),
    ]))
}

#[cfg(test)]
mod tests;
