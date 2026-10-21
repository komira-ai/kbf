//! The `linux meta-backup` and `linux meta-verify` flags, and their exits
//! ([`crate::cli`]).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;

use super::{Backup, Get, MetaError};
use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::plan::Mode;
use kbf_host::step::HttpUrl;

#[derive(Args)]
pub struct BackupArgs {
    /// kbf-server's metadata directory (its --meta-dir, or <hot store>/meta).
    #[arg(long, value_parser = HostPath::new)]
    meta_dir: HostPath,
    /// The hot store's directory (its --hot-store), when the server has a write tier.
    #[arg(long, value_parser = HostPath::new)]
    hot_dir: Option<HostPath>,
    /// The archive to write; it must not exist, nor be inside --meta-dir.
    #[arg(long, value_parser = HostPath::new)]
    out: HostPath,
    /// kbf-server's /metrics on its operator API port; /readyz is read on the same
    /// host and port.
    #[arg(long, default_value = "http://127.0.0.1:8982/metrics", value_parser = |s: &str| HttpUrl::new(s).map_err(|e| e.to_string()))]
    metrics: HttpUrl,
    /// The systemd unit to stop and start.
    #[arg(long, default_value = "kbf-server")]
    unit: String,
    /// Seconds to wait for /readyz after the start.
    #[arg(long, default_value_t = super::READY.as_secs())]
    ready_secs: u64,
    /// Carry the backup out (as root).
    #[arg(long)]
    apply: bool,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// An archive meta-backup wrote.
    #[arg(long)]
    archive: PathBuf,
    /// Where to extract it: an empty or new directory.
    #[arg(long)]
    into: PathBuf,
}

/// `meta-backup`: 0 done, 1 a failure after the stop (the unit restarted), 2 refused.
pub fn backup<E: Exec>(
    a: BackupArgs,
    exec: &mut E,
    get: Get<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let b = Backup {
        meta_dir: a.meta_dir,
        hot_dir: a.hot_dir,
        out: a.out,
        metrics: a.metrics,
        unit: a.unit,
        ready: Duration::from_secs(a.ready_secs),
    };
    let mode = if a.apply { Mode::Apply } else { Mode::DryRun };
    exit(
        super::backup(exec, &b, get, mode, out).map(|()| Vec::new()),
        out,
        err,
    )
}

/// `meta-verify`: 0 every file matches its manifest line, 1 the archive does not
/// verify, 2 refused.
pub fn verify(a: &VerifyArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    exit(super::verify(&a.archive, &a.into), out, err)
}

fn exit(
    done: Result<Vec<String>, MetaError>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match done {
        Ok(lines) => {
            for l in lines {
                let _ = writeln!(out, "{l}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let code = e.exit_code();
            let _ = if code == 1 {
                writeln!(err, "FAIL {e}")
            } else {
                writeln!(err, "REFUSED: {e}")
            };
            ExitCode::from(code)
        }
    }
}
