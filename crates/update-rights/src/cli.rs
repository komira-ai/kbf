//! The `approve` subcommand: `approve <service> <sha256>`.
//!
//! It records that an operator has looked at one exact declaration (the digest `sha256sum` prints
//! for its file, which a deferred update reports) and lets an update that needs it go on. It is a
//! root act and says what it did on standard output: that line is the audit record, kept by
//! whatever runs the command (journald, an ssh log).
//!
//! The root part of the updater (`kbf-host-install`) hands its `approve` arguments to [`main`].

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::approval::APPROVALS_FILE;

/// What the command runs against.
pub struct Env {
    pub approvals: PathBuf,
    /// The account that owns the approval file.
    pub trusted_uid: u32,
    /// The account the process runs as.
    pub effective_uid: u32,
}

impl Env {
    /// This host: `/etc/updater.d/approved`, owned by root, run as the effective user.
    pub fn production() -> Env {
        Env {
            approvals: PathBuf::from(APPROVALS_FILE),
            trusted_uid: 0,
            effective_uid: rustix::process::geteuid().as_raw(),
        }
    }
}

/// Runs the command. `args` follow the word `approve`. Exit codes: 0 done, 1 refused or
/// failed, 2 usage.
pub fn run(_args: &[String], _env: &Env, _out: &mut dyn Write, _err: &mut dyn Write) -> u8 {
    1
}

/// The command as the root part of the updater runs it.
pub fn main(args: &[String]) -> ExitCode {
    ExitCode::from(run(
        args,
        &Env::production(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
