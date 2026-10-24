//! `kbf-host-install --serve` as a process: the flags, the account, the socket, the backend
//! of the platform this program runs on, and the loop.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use super::backend::Backend;
use super::core::Ab;
use super::serve::{self, ServeConfig, Serving};
use crate::mac::layout::MacHost;
use crate::runner::SystemRunner;
use crate::trust::Trust;

/// Root's directory for this program's state on a Linux host.
pub const LINUX_STATE_DIR: &str = "/var/lib/kbf-host-install";

/// The audit log (shared with the ssh path).
const AUDIT: &str = "/var/log/kbf-host-install.log";

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The program: serve until killed. The exit status is 2 for flags, 1 for a start that
/// failed or a loop that ended.
pub fn main(args: &[String]) -> ExitCode {
    main_with(args, run)
}

/// [`main`] with the serving put in by the caller.
pub(super) fn main_with(
    args: &[String],
    serve: impl FnOnce(&ServeConfig) -> Result<(), String>,
) -> ExitCode {
    let config = match ServeConfig::parse(args) {
        Ok(config) => config,
        Err(why) => {
            eprintln!("kbf-host-install: {why}\n{}", serve::USAGE);
            return ExitCode::from(2);
        }
    };
    match serve(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("kbf-host-install: {why}");
            ExitCode::from(1)
        }
    }
}

fn run(config: &ServeConfig) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        serve_mac(config)
    } else {
        serve_linux(config)
    }
}

fn log(line: &str) {
    eprintln!("kbf-host-install: {line}");
}

/// Make root's private directory `dir` (0700) if it is not there, and check it.
pub(super) fn private_dir(trust: &Trust, dir: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    trust
        .check_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Bind the socket and serve `backend` on it until `stop`.
pub(super) fn serve_with<B: Backend>(
    config: &ServeConfig,
    backend: B,
    place: &Place<'_>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (state_dir, trust, ids, audit) = (place.state_dir, place.trust, place.ids, place.audit);
    let ab = Ab::new(
        backend,
        state_dir.join("state.json"),
        trust.clone(),
        config.window_secs,
        Box::new(unix_now),
    );
    let listener = serve::bind(trust, &config.socket, ids)
        .map_err(|e| format!("{}: {e}", config.socket.display()))?;
    let serving = Serving {
        ab: &ab,
        trust,
        audit,
        log: &log,
    };
    serving
        .run(&listener, stop)
        .map_err(|e| format!("the socket: {e}"))
}

/// Where the state and the audit log are, who is trusted to own them, and who may connect.
pub(super) struct Place<'a> {
    pub(super) state_dir: &'a Path,
    pub(super) trust: &'a Trust,
    pub(super) ids: (u32, u32),
    pub(super) audit: &'a Path,
}

#[cfg(target_os = "linux")]
fn serve_linux(config: &ServeConfig) -> Result<(), String> {
    if config.units.is_empty() {
        return Err("--restart-unit is required on a Linux host".to_owned());
    }
    let trust = Trust::root();
    let passwd = trust
        .read(Path::new("/etc/passwd"))
        .map_err(|e| format!("/etc/passwd: {e}"))?;
    let ids = serve::passwd_ids(&passwd, &config.user)
        .filter(|(uid, gid)| *uid != 0 && *gid != 0)
        .ok_or_else(|| format!("the account {} has no usable ids", config.user))?;
    let state_dir = Path::new(LINUX_STATE_DIR);
    private_dir(&trust, state_dir)?;
    serve_with(
        config,
        super::linux::production(config.units.clone()),
        &Place {
            state_dir,
            trust: &trust,
            ids,
            audit: Path::new(AUDIT),
        },
        &AtomicBool::new(false),
    )
}

#[cfg(not(target_os = "linux"))]
fn serve_linux(_config: &ServeConfig) -> Result<(), String> {
    Err("this program serves a Linux host on Linux only".to_owned())
}

fn serve_mac(config: &ServeConfig) -> Result<(), String> {
    use crate::mac::tools;
    if !config.units.is_empty() {
        return Err(
            "--restart-unit is for a Linux host: a Mac restarts the daemon's job".to_owned(),
        );
    }
    let host = MacHost::production();
    let exec = crate::runner::Runner::run(
        &SystemRunner,
        &tools::dscl_ids(&config.user, host.tuning.tool),
    )
    .map_err(|e| format!("dscl: {e}"))?;
    let who = tools::parse_ids(&exec.stdout_text())
        .filter(|id| id.uid != 0 && id.gid != 0)
        .ok_or_else(|| format!("the account {} has no usable ids", config.user))?;
    crate::mac::ensure_work(&host).map_err(|_| "the work directory is not usable".to_owned())?;
    let (trust, work) = (host.trust.clone(), host.work.clone());
    let backend = super::mac_backend::MacBackend::new(host, Box::new(SystemRunner));
    serve_with(
        config,
        backend,
        &Place {
            state_dir: &work,
            trust: &trust,
            ids: (who.uid, who.gid),
            audit: Path::new(AUDIT),
        },
        &AtomicBool::new(false),
    )
}
