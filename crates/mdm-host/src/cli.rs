//! The command line: parsing, dispatch and the exit code.

use std::io::Write;

use clap::{Parser, Subcommand};
use host_step::Mode;
use mdm_core::acme::Acme;
use mdm_core::config::MdmConfig;
use mdm_core::pins::{BUILT_IN, Pins, Tool};

use crate::fetch::{self, Fetch};
use crate::install::{self, Install};
use crate::verify::{self, Verify};
use crate::{Env, MdmError, tls};

/// mdm-host: MDM for enrolling Macs as compute hosts, on a Linux host. Every verb prints its
/// plan by default; --apply acts. Exit 0 done, 1 a step or check failed, 2 refused.
#[derive(Parser)]
#[command(name = "mdm-host", version)]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Download the pinned upstream archives (as yourself, not root).
    Fetch {
        /// Where the archives go: absolute, outside every git work tree.
        #[arg(long)]
        dir: String,
        /// Only this tool (repeatable); every pinned tool by default.
        #[arg(long = "only", value_parser = Tool::named)]
        only: Vec<Tool>,
        /// Download; without it the plan is printed.
        #[arg(long)]
        apply: bool,
    },
    /// Install the services (root), or with --uninstall take their units away.
    Install {
        /// The settings file.
        #[arg(long)]
        config: String,
        /// Where `fetch` put the archives.
        #[arg(
            long,
            required_unless_present = "uninstall",
            conflicts_with = "uninstall"
        )]
        bin_dir: Option<String>,
        /// Stop and disable the units and move their files aside; state and secrets stay.
        #[arg(long)]
        uninstall: bool,
        /// Act; without it the plan is printed.
        #[arg(long)]
        apply: bool,
    },
    /// The TLS certificate over ACME DNS-01 and its renewal (root). Only the checks before the
    /// plan refuse without changing anything: the plan runs in two parts, and when the
    /// certificate is not there after the issuing unit ran, the run fails (exit 1) with the
    /// units already written; run it again once the cause is fixed.
    Tls {
        /// The settings file.
        #[arg(long)]
        config: String,
        /// Act; without it the plan is printed.
        #[arg(long)]
        apply: bool,
    },
    /// Read-only health checks, and installed files compared with the render.
    Verify {
        /// The settings file.
        #[arg(long)]
        config: String,
        /// Leave out of both texts of every file the lines equal to this one, whole
        /// (repeatable; every line left out is printed).
        #[arg(long)]
        ignore: Vec<String>,
    },
}

fn mode(apply: bool) -> Mode {
    match apply {
        true => Mode::Apply,
        false => Mode::DryRun,
    }
}

/// The settings file's text.
fn read_config(env: &Env<'_>, path: &str) -> Result<String, MdmError> {
    let bytes = env
        .host
        .read(path)?
        .ok_or_else(|| MdmError::NoFile(path.to_owned()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Runs the verb `args` (the program's name first) names, with the built-in pins. The exit
/// code.
pub fn run(args: &[String], env: &Env<'_>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    run_with_pin_text(args, env, BUILT_IN, out, err)
}

/// [`run_with`] the pins `text` holds; a pin table that does not parse fails the run (exit 1,
/// `FAILED: ...` on `err`) before the arguments are read.
fn run_with_pin_text(
    args: &[String],
    env: &Env<'_>,
    text: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    match Pins::parse(text) {
        Ok(pins) => run_with(args, env, &pins, out, err),
        Err(e) => {
            let _ = writeln!(err, "FAILED: the built-in pins: {e}");
            1
        }
    }
}

/// [`run`] with other pins.
pub fn run_with(
    args: &[String],
    env: &Env<'_>,
    pins: &Pins,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let sink: &mut dyn Write = if e.use_stderr() { err } else { out };
            let _ = write!(sink, "{}", e.render());
            return u8::try_from(e.exit_code()).unwrap_or(2);
        }
    };
    match dispatch(cli.verb, env, pins, out) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            let word = if e.exit_code() == 2 {
                "REFUSED"
            } else {
                "FAILED"
            };
            let _ = writeln!(err, "{word}: {e}");
            e.exit_code()
        }
    }
}

/// Carries the verb out; false when a check failed.
fn dispatch(verb: Verb, env: &Env<'_>, pins: &Pins, out: &mut dyn Write) -> Result<bool, MdmError> {
    match verb {
        Verb::Fetch { dir, only, apply } => {
            fetch::run(env, &Fetch { dir, only }, pins, mode(apply), out)?;
        }
        Verb::Install {
            config,
            bin_dir,
            uninstall,
            apply,
        } => {
            let config = MdmConfig::parse(&read_config(env, &config)?)?;
            match (uninstall, bin_dir) {
                (false, Some(bin_dir)) => {
                    let a = Install {
                        config,
                        bin_dir,
                        max_archive: fetch::MAX_ARCHIVE,
                    };
                    install::run(env, &a, pins, mode(apply), out)?;
                }
                _ => install::run_uninstall(env, &config, mode(apply), out)?,
            }
        }
        Verb::Tls { config, apply } => {
            let text = read_config(env, &config)?;
            let (config, acme) = (MdmConfig::parse(&text)?, Acme::parse(&text)?);
            tls::run(env, &config, &acme, mode(apply), out)?;
        }
        Verb::Verify { config, ignore } => {
            let text = read_config(env, &config)?;
            let config = MdmConfig::parse(&text)?;
            let acme = Acme::parse(&text).ok();
            let v = Verify {
                config: &config,
                acme: acme.as_ref(),
                ignore: &ignore,
            };
            return verify::run(env, &v, out);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use host_step::fake::FakeHost;

    use super::run_with_pin_text;
    use crate::{Env, Net, system_random};

    /// Catches a pin table that does not parse being run with anyway, or failing silently: the
    /// run fails with exit 1 and says why, before it reads the arguments or the host.
    #[test]
    fn a_pin_table_that_does_not_parse_fails_the_run() {
        let host = FakeHost::new(0);
        let net = Net::new(true);
        let env = Env {
            host: &host,
            net: &net,
            random: &system_random,
            now: 0,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = ["mdm-host".to_owned(), "--help".to_owned()];
        let code = run_with_pin_text(&args, &env, "caddy\n", &mut out, &mut err);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "FAILED: the built-in pins: pins line 1: 1 fields, not 4 (tool version url sha256)\n"
        );
    }
}
