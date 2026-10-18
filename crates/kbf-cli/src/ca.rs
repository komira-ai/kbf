//! `kbf ca init <dir>` and `kbf ca import <dir> --cert <file> --key <file>`: make the farm's
//! CA directory (`docs/design/install-and-enrollment.md`, sections 6.1 and 6.6).
//!
//! Both leave `<dir>/ca.key` (mode 0600 in a directory of mode 0700) and `<dir>/ca.pem`
//! (mode 0644), print the pin of the root (what a token carries) and refuse a directory
//! that already holds a `ca.key` or a `ca.pem`: neither overwrites a CA.
//!
//! - `init` creates the key and a ten-year self-signed root (`kbf_ca::Authority::init_dir`).
//! - `import` reads a root certificate (PEM) and its key (PKCS #8 PEM) made elsewhere,
//!   checks that the certificate is a self-signed CA certificate for that key
//!   (`kbf_ca::Authority::import`) and copies both into `<dir>`. The key file read is
//!   the operator's; the copy is the one the farm uses.
//!
//! Exit 0 on success; 1 with the reason on stderr; 2 for a command line that does not parse.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Command, FromArgMatches as _, Subcommand};
use kbf_ca::{Authority, Bundle, Entropy, FileSigner, Policy, SystemEntropy};

/// The word after `kbf` that makes the CA directory instead of starting a mode.
pub const WORD: &str = "ca";

/// `kbf ca`'s command line.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Ca {
    #[command(subcommand)]
    pub verb: Verb,
}

/// What `kbf ca` does.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Make a new CA: a key and a self-signed root, in DIR.
    Init {
        /// The CA directory (made with mode 0700 if missing).
        dir: PathBuf,
    },
    /// Copy an existing CA (its root certificate and key) into DIR.
    Import {
        /// The CA directory (made with mode 0700 if missing).
        dir: PathBuf,
        /// The root certificate, PEM.
        #[arg(long, value_name = "FILE")]
        cert: PathBuf,
        /// The root's private key, PKCS #8 PEM.
        #[arg(long, value_name = "FILE")]
        key: PathBuf,
    },
}

/// `kbf ca ...`'s command line.
#[must_use]
pub fn command() -> Command {
    let cmd = Command::new(WORD)
        .bin_name(format!("kbf {WORD}"))
        .about("Make the farm's CA directory: a new CA (init) or an existing one (import).")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .disable_help_subcommand(true);
    Ca::augment_args(cmd)
}

/// Parses `kbf ca ...`; `args` is the whole command line, its first word the program's
/// name and its second [`WORD`].
///
/// # Errors
/// What `clap` refuses.
pub fn parse(args: &[std::ffi::OsString]) -> Result<Ca, clap::Error> {
    let argv = args.iter().take(1).chain(args.iter().skip(2));
    Ca::from_arg_matches(&command().try_get_matches_from(argv)?)
}

/// Runs `ca` with the real clock and random source, printing to standard output.
#[must_use]
pub fn run(ca: &Ca) -> ExitCode {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    match execute(ca, now, &mut SystemEntropy, &mut std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf ca: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The directory must hold neither file of a CA yet.
fn refuse_existing(dir: &Path) -> Result<(), Box<dyn Error>> {
    // STUB (red run): an existing CA is not refused up front.
    for name in [FileSigner::KEY_FILE, Authority::CERT_FILE]
        .into_iter()
        .take(0)
    {
        if fs::symlink_metadata(dir.join(name)).is_ok() {
            return Err(format!(
                "{} already exists: a CA is never overwritten",
                dir.join(name).display()
            )
            .into());
        }
    }
    Ok(())
}

fn print_pin(out: &mut dyn Write, ca: &Authority, dir: &Path) -> Result<(), Box<dyn Error>> {
    writeln!(out, "ca: {}", dir.display())?;
    let _ = ca;
    writeln!(out, "pin: 0")?; // STUB (red run): a pin that is not the root's
    Ok(())
}

/// Does what `ca` says at unix time `now`, with `entropy` for the root's serial, and
/// writes the pin to `out`.
///
/// # Errors
/// Why the directory could not be made: it holds a CA, the files do not parse or do not
/// match, or the system refused.
pub fn execute(
    ca: &Ca,
    now: i64,
    entropy: &mut dyn Entropy,
    out: &mut dyn Write,
) -> Result<(), Box<dyn Error>> {
    match &ca.verb {
        Verb::Init { dir } => {
            refuse_existing(dir)?;
            let authority = Authority::init_dir(dir, Policy::default(), now, entropy)?;
            print_pin(out, &authority, dir)
        }
        Verb::Import { dir, cert, key } => {
            let pem = fs::read_to_string(key)?;
            let bundle = Bundle::from_pem(&fs::read(cert)?)?;
            let authority = Authority::import(
                Arc::new(FileSigner::from_pem(&pem)?),
                Policy::default(),
                &bundle.certs()[0],
            )?;
            refuse_existing(dir)?;
            FileSigner::from_pem(&pem)?.store_in(dir)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(dir.join(Authority::CERT_FILE))?;
            file.write_all(authority.bundle().to_pem().as_bytes())?;
            print_pin(out, &authority, dir)
        }
    }
}
