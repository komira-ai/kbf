//! The flags of `kbf update-hook`, the ones before the step. Not written yet: every command
//! line is refused.

use std::path::PathBuf;

use super::super::build::Access;
use super::super::flags::Timing;

/// The settings of one hook run.
#[derive(Clone, Debug)]
pub struct Flags {
    /// The hook's record directory.
    pub record_dir: PathBuf,
    /// The release channel the helper's allowlist names.
    pub channel: String,
    /// The socket of `kbf-host-install --serve`.
    pub helper_socket: PathBuf,
    /// How the node is reached at the frontend.
    pub access: Access,
    /// How long, and how often, the update waits for things.
    pub timing: Timing,
}

/// The flags before the step, and the words from the step on.
///
/// # Errors
/// What is wrong with the flags, in a line.
pub fn split(args: &[String]) -> Result<(Flags, &[String]), String> {
    let _ = args;
    Err("the flags are not written yet".to_owned())
}
