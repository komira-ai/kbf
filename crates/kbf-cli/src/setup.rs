//! `kbf setup`: the first configuration of a host whose kbf came from the packages.
//!
//! It is `kbf-installer setup` ([`kbf_setup`], the same library) run by the `kbf` binary, so a
//! host from the packages needs no other program to be set up: the same flags, the same plan of
//! steps, the same files (`kbf.toml`, `installer.toml`), accounts and state database, and the
//! same exit codes. Without `--apply` it prints each step and changes nothing; with it, the
//! steps are carried out. It is root's: run by anyone else it exits 2 and says so.
//!
//! What it does not take is the proxy's side: the library is linked without its `proxy` feature,
//! so there are no `--proxy` flags (clap refuses them as flags it does not know), a host behind
//! the proxy (an all-in-one host always is) is `kbf-installer setup --proxy`'s, and the `kbf`
//! binary links none of the proxy's code.
//!
//! Exit status: 0 done, 1 failed, 2 a command line or a prerequisite that is not right (not
//! root, a flag, a missing user), 5 only a person can go on. The text goes as the installer's
//! does: the steps on standard output, an error on standard error as `kbf setup: <why>`, and with
//! `--json` the error as one object on standard output (`verb`, `error`, `message`).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use kbf_setup::cli::SetupArgs;
use kbf_setup::exit::Exit;
use kbf_setup::setup::SetupError;

/// The word after `kbf` that runs the setup instead of a mode. It is not in
/// [`crate::names::MODES`], so the mode list stays exact.
pub const SETUP: &str = "setup";

/// A parsed `kbf setup` command line.
#[derive(Parser)]
#[command(
    name = "setup",
    bin_name = "kbf setup",
    about = "First configuration of a host from the packages: the accounts, directories, \
             installer.toml, kbf.toml and the units of the mode, and the state database's schema",
    after_help = "Without --apply, setup prints each step as DRY-RUN and changes nothing. A \
                  frontend behind the proxy and an all-in-one host are set up by `kbf-installer \
                  setup --proxy`: this command takes none of the proxy's flags."
)]
pub struct Args {
    /// Print an error as one JSON object on standard output.
    #[arg(long)]
    pub json: bool,
    /// Do what setup says. Without it setup is a dry run.
    #[arg(long)]
    pub apply: bool,
    /// What the host is set up as.
    #[command(flatten)]
    pub setup: SetupArgs,
}

/// Parses `kbf setup ...`; `args` is the whole command line, its first word the program's name
/// and its second [`SETUP`].
///
/// # Errors
/// What `clap` refuses: no `--mode`, a flag `setup` does not take (the proxy's among them), a
/// word that is not a flag.
pub fn parse(args: &[OsString]) -> Result<Args, clap::Error> {
    let argv = args.iter().take(1).chain(args.iter().skip(2));
    Args::try_parse_from(argv)
}

/// Runs the setup on this host: the steps on standard output, an error on standard error (or
/// with `--json` on standard output), and the exit status the installer's table gives it.
#[must_use]
pub fn run(args: &Args) -> ExitCode {
    let mut steps = Vec::new();
    let result = kbf_setup::setup::cli(PathBuf::from("/"), &args.setup, args.apply, &mut steps);
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    finish(args.json, result, &steps, &mut out, &mut err).into()
}

/// How a run ends, as `kbf-installer setup` ends it. The steps are for a person: standard output,
/// or with `--json` (where standard output is one object) standard error. A failure prints
/// them ahead of its own line, as text, or as the one object alone.
fn finish(
    json: bool,
    result: Result<(), SetupError>,
    steps: &[u8],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Exit {
    let shown = String::from_utf8_lossy(steps);
    let Err(e) = result else {
        // A closed pipe is the reader's business; the exit status still tells.
        let _ = if json {
            write!(err, "{shown}")
        } else {
            write!(out, "{shown}")
        };
        return Exit::Ok;
    };
    let _ = if json {
        let object =
            serde_json::json!({"verb": "setup", "error": e.code(), "message": e.to_string()});
        writeln!(out, "{object}")
    } else {
        writeln!(err, "{shown}kbf setup: {e}")
    };
    e.exit()
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
