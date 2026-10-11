//! `kbf-deploy`: the deploy kits (see the library's docs). Exit codes: 0 done, 1 a
//! check found a broken rule, 2 refused (a bad flag, setting or file).

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_deploy::linux;

#[derive(Parser)]
#[command(name = "kbf-deploy", version, about = "Deploy kits for kbf hosts")]
struct Cli {
    #[command(subcommand)]
    kit: Kit,
}

#[derive(Subcommand)]
enum Kit {
    /// A converged Linux storage node: MinIO, kbf-server and kbf-daemon.
    #[command(subcommand)]
    Linux(Linux),
}

#[derive(Subcommand)]
enum Linux {
    /// Render the node's four systemd files into a directory.
    RenderUnits {
        /// The kit's settings, KEY=VALUE lines.
        #[arg(long)]
        config: PathBuf,
        /// Where to write the files, laid out as under /etc/systemd/system.
        #[arg(long)]
        out: PathBuf,
        /// The kbf user's uid (the daemon's XDG_RUNTIME_DIR); without it the daemon's
        /// unit keeps `<kbf uid>`, which `check-units --installable` refuses.
        #[arg(long)]
        kbf_uid: Option<u32>,
    },
    /// Check rendered units against the host's priorities.
    CheckUnits {
        /// The settings the units were rendered with.
        #[arg(long)]
        config: PathBuf,
        /// The directory render-units wrote.
        #[arg(long)]
        dir: PathBuf,
        /// Also refuse a `<...>` placeholder.
        #[arg(long)]
        installable: bool,
    },
}

fn main() -> ExitCode {
    let mut stdout = std::io::stdout().lock();
    let (lines, code) = match Cli::parse().kit {
        Kit::Linux(Linux::RenderUnits {
            config,
            out,
            kbf_uid,
        }) => match linux::render_units(&config, &out, kbf_uid) {
            Ok(line) => (vec![line], 0),
            Err(e) => return refused(&e),
        },
        Kit::Linux(Linux::CheckUnits {
            config,
            dir,
            installable,
        }) => match linux::check_units(&config, &dir, installable) {
            Ok(report) => {
                let code = u8::from(!report.failures.is_empty());
                let mut lines: Vec<String> =
                    report.failures.iter().map(ToString::to_string).collect();
                lines.push(report.summary);
                (lines, code)
            }
            Err(e) => return refused(&e),
        },
    };
    for line in lines {
        // A closed stdout (`| head`) is not worth a panic; the exit code still says it.
        let _ = writeln!(stdout, "{line}");
    }
    ExitCode::from(code)
}

fn refused(e: &linux::Refusal) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "REFUSED: {e}");
    ExitCode::from(2)
}
