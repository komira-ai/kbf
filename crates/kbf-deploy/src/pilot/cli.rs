//! The `pilot` subcommands of the `kbf-deploy` command line ([`crate::cli`]).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::{Args, Subcommand};

use super::PilotError;
use super::buckconfig::{self, Overlay};
use super::config::{KEYS, OverlaySettings, ServerSettings};
use super::server::{self, Home, Install, Platform, Uninstall};
use crate::config::Config;
use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::plan::Mode;

#[derive(Subcommand)]
pub enum Pilot {
    /// Run kbf-server as a capped systemd user service, as yourself (not root).
    ServerInstall(ServerInstallArgs),
    /// Point one linked worktree's buck2 at the pilot kbf-server (.buckconfig.local).
    Buckconfig(BuckconfigArgs),
}

#[derive(Args)]
pub struct ServerInstallArgs {
    /// The kit's settings, KEY=VALUE lines.
    #[arg(long)]
    config: PathBuf,
    /// The kbf-server binary to install.
    #[arg(long, value_parser = HostPath::new, required_unless_present = "uninstall")]
    bin: Option<HostPath>,
    /// The address to bind, instead of KBF_SERVER_BIND.
    #[arg(long)]
    bind: Option<String>,
    /// The directory holding ca.pem, server.pem and server.key (default
    /// ~/.config/kbf-pilot/pki).
    #[arg(long, value_parser = HostPath::new)]
    pki: Option<HostPath>,
    /// The file holding the S3 key pair as AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY=
    /// lines, mode 0600 (default ~/.config/kbf-pilot/s3.env).
    #[arg(long, value_parser = HostPath::new)]
    s3_env_file: Option<HostPath>,
    /// Roll back: stop the service and move both units aside.
    #[arg(long, conflicts_with_all = ["bin", "bind", "pki", "s3_env_file"])]
    uninstall: bool,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

#[derive(Args)]
pub struct BuckconfigArgs {
    /// The kit's settings, KEY=VALUE lines.
    #[arg(long)]
    config: PathBuf,
    /// The linked worktree, by absolute path.
    #[arg(long, value_parser = HostPath::new)]
    worktree: HostPath,
    /// Delete the overlay (only one this verb wrote).
    #[arg(long)]
    remove: bool,
    /// Carry the plan out.
    #[arg(long)]
    apply: bool,
}

/// Run a `pilot` subcommand: 0 done, 1 a step failed or kbf-server never reported
/// ready, 2 refused.
pub fn run<E: Exec>(
    p: Pilot,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    now: SystemTime,
    out: &mut dyn Write,
    err: &mut dyn Write,
    pause: &dyn Fn(),
) -> ExitCode {
    let done = match p {
        Pilot::ServerInstall(a) => server_install(a, exec, env, now, out, pause),
        Pilot::Buckconfig(a) => Config::load(&a.config, KEYS)
            .map_err(PilotError::from)
            .and_then(|c| {
                if a.remove {
                    buckconfig::run_remove(exec, &a.worktree, mode(a.apply), out)
                } else {
                    let o = Overlay {
                        settings: OverlaySettings::from_config(&c)?,
                        worktree: a.worktree,
                    };
                    buckconfig::run(exec, &o, mode(a.apply), out)
                }
            }),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let code = e.exit_code();
            let _ = if code == 1 {
                writeln!(err, "kbf-deploy: {e}")
            } else {
                writeln!(err, "REFUSED: {e}")
            };
            ExitCode::from(code)
        }
    }
}

/// How long `server-install` pauses between looks for kbf-server's start line.
pub const PAUSE: Duration = Duration::from_secs(1);

/// The pause on a host: [`PAUSE`].
pub fn sleep_a_second() {
    std::thread::sleep(PAUSE);
}

fn server_install<E: Exec>(
    a: ServerInstallArgs,
    exec: &mut E,
    env: &dyn Fn(&str) -> Option<OsString>,
    now: SystemTime,
    out: &mut dyn Write,
    pause: &dyn Fn(),
) -> Result<(), PilotError> {
    let c = Config::load(&a.config, KEYS)?;
    let home = Home {
        home: env_path(env, "HOME")?.ok_or(PilotError::NoEnv("HOME"))?,
        xdg_config: env_path(env, "XDG_CONFIG_HOME")?,
    };
    if a.uninstall {
        let u = Uninstall {
            state: ServerSettings::state_of(&c)?,
            home,
        };
        return server::run_uninstall(
            exec,
            &u,
            Platform::HOST,
            &crate::cli::stamp(now),
            mode(a.apply),
            out,
        );
    }
    let i = Install {
        settings: ServerSettings::from_config(&c, a.bind.as_deref())?,
        bin: a.bin.ok_or(PilotError::NoBin)?,
        pki: a.pki,
        s3_env: a.s3_env_file,
        home,
    };
    server::run(exec, &i, Platform::HOST, mode(a.apply), out, pause)
}

/// An absolute path from the environment; unset or empty is `None`.
fn env_path(
    env: &dyn Fn(&str) -> Option<OsString>,
    key: &'static str,
) -> Result<Option<HostPath>, PilotError> {
    match env(key).map(|v| v.to_string_lossy().into_owned()) {
        Some(v) if !v.is_empty() => Ok(Some(
            HostPath::new(&v).map_err(|e| PilotError::path(key, e))?,
        )),
        _ => Ok(None),
    }
}

fn mode(apply: bool) -> Mode {
    if apply { Mode::Apply } else { Mode::DryRun }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
