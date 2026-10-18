//! The `kbf-signer` command line.

use std::convert::Infallible;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_ca::{FileSigner, Policy, SystemEntropy};

use crate::ca_dir::ensure;
use crate::error::SignerError;
use crate::server::{SignerServer, bind, harden};

/// `kbf-signer`'s flags. Configuration is flags: nothing is read from the environment.
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(
    name = "kbf-signer",
    about = "Holds the farm's CA key and signs certificates for the frontend's account alone."
)]
pub struct Flags {
    /// The CA directory (`ca.key` mode 0600, `ca.pem`), owned by this account.
    #[arg(long, value_name = "DIR")]
    pub ca_dir: PathBuf,
    /// The socket to serve, in a directory of mode 0700 (made if missing).
    #[arg(long, value_name = "PATH")]
    pub socket: PathBuf,
    /// The user id of the frontend: the only peer served.
    #[arg(long, value_name = "UID")]
    pub frontend_uid: u32,
    /// The longest certificate lifetime the signer agrees to, in seconds.
    #[arg(long, value_name = "SECS", default_value_t = Policy::DEFAULT_LIFETIME_S)]
    pub lifetime_secs: i64,
}

fn start(flags: &Flags) -> Result<Infallible, SignerError> {
    harden()?;
    let policy = Policy::new(flags.lifetime_secs)?;
    // The signer owns ca.key, so the signer makes the CA at first start.
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(0);
    let (authority, _) = ensure(&flags.ca_dir, policy, now, &mut SystemEntropy)?;
    let server = SignerServer::new(
        FileSigner::load(&flags.ca_dir)?,
        authority.root_der(),
        flags.frontend_uid,
        policy,
    )?;
    let listener = bind(&flags.socket)?;
    Ok(server.serve(&listener)?)
}

/// Starts the signer and serves until killed; returns only to report why it could not.
#[must_use]
pub fn run(flags: &Flags) -> ExitCode {
    let Err(e) = start(flags);
    eprintln!("kbf-signer: {e}");
    ExitCode::FAILURE
}

/// The `kbf-signer` binary.
#[must_use]
pub fn main() -> ExitCode {
    run(&Flags::parse())
}
