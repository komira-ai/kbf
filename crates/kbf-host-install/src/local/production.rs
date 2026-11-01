//! `kbf-host-install --serve` as a process: the flags, the account, the socket, the backend
//! of the platform this program runs on, and the loop.
//!
//! The paths this program reads and writes as root are constants here and in the backends;
//! the serving itself takes them as arguments ([`Place`], [`LinuxEnv`]) so that a test serves
//! a scratch directory with the same code.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use super::backend::Backend;
use super::core::Ab;
use super::serve::{self, Layout, ServeConfig, Serving};
use crate::mac::layout::MacHost;
use crate::pace::Pace;
use crate::runner::{Runner, SystemRunner};
use crate::trust::Trust;

/// Root's directory for this program's state on a Linux host.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LINUX_STATE_DIR: &str = "/var/lib/kbf-host-install";

/// The audit log (shared with the ssh path).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

/// The platform's serving. (Selected by the target, not by an `if`: the other is compiled and
/// tested on every platform all the same.)
#[cfg(target_os = "macos")]
pub(super) fn run(config: &ServeConfig) -> Result<(), String> {
    serve_mac(config)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn run(config: &ServeConfig) -> Result<(), String> {
    serve_linux(config)
}

pub(super) fn log(line: &str) {
    eprintln!("kbf-host-install: {line}");
}

/// Make root's private directory `dir` (0700) if it is not there, and check it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn private_dir(trust: &Trust, dir: &Path) -> Result<(), String> {
    let say = |e: std::io::Error| format!("{}: {e}", dir.display());
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(say(e)),
        _ => {}
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(say)?;
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
    let ab = Ab::new(
        backend,
        place.state_dir.join(place.state_name),
        place.trust.clone(),
        config.window_secs,
        Box::new(unix_now),
    )
    .keeping(config.keep_releases, Box::new(log));
    ab.seed_versions();
    let listener = serve::bind(place.trust, &config.socket, place.ids)
        .map_err(|e| format!("{}: {e}", config.socket.display()))?;
    let pace = Pace::real();
    let serving = Serving {
        ab: &ab,
        trust: place.trust,
        audit: place.audit,
        log: &log,
        pace: &pace,
    };
    serving
        .run(&listener, stop)
        .map_err(|e| format!("the socket: {e}"))
}

/// Where the state and the audit log are, who is trusted to own them, and who may connect.
pub(super) struct Place<'a> {
    pub(super) state_dir: &'a Path,
    pub(super) state_name: &'a str,
    pub(super) trust: &'a Trust,
    pub(super) ids: (u32, u32),
    pub(super) audit: &'a Path,
}

/// What a Linux host's serving reads: the passwd file the account is looked up in, and the
/// private state directory and audit log it writes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) struct LinuxEnv {
    pub(super) passwd: PathBuf,
    pub(super) state_dir: PathBuf,
    /// The file of the A/B state, in `state_dir`.
    pub(super) state_name: String,
    pub(super) audit: PathBuf,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl LinuxEnv {
    pub(super) fn production() -> Self {
        Self {
            passwd: PathBuf::from("/etc/passwd"),
            state_dir: PathBuf::from(LINUX_STATE_DIR),
            state_name: "state.json".to_owned(),
            audit: PathBuf::from(AUDIT),
        }
    }
}

/// A Linux host: the account of the socket from the passwd file (never root), the state directory
/// made private, then the serving.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn serve_linux_with<B: Backend>(
    config: &ServeConfig,
    env: &LinuxEnv,
    trust: &Trust,
    backend: B,
    stop: &AtomicBool,
) -> Result<(), String> {
    if config.units.is_empty() && !config.frontend_container {
        return Err(
            "--restart-unit is required on a Linux host (or --frontend-container)".to_owned(),
        );
    }
    let passwd = trust
        .read(&env.passwd)
        .map_err(|e| format!("{}: {e}", env.passwd.display()))?;
    let ids = serve::passwd_ids(&passwd, &config.user)
        .filter(|(uid, gid)| *uid != 0 && *gid != 0)
        .ok_or_else(|| format!("the account {} has no usable ids", config.user))?;
    private_dir(trust, &env.state_dir)?;
    let place = Place {
        state_dir: &env.state_dir,
        state_name: &env.state_name,
        trust,
        ids,
        audit: &env.audit,
    };
    serve_with(config, backend, &place, stop)
}

/// The Linux side as the program runs it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn serve_linux(config: &ServeConfig) -> Result<(), String> {
    serve_linux_for(config)
}

/// The container a helper is started for, when its configuration is not the default file
/// (`None` for the default, whose files keep their names).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn bound_container(
    trust: &Trust,
    config: &ServeConfig,
) -> Result<Option<String>, String> {
    if config.frontend_config == Path::new(super::frontend::config::CONFIG) {
        return Ok(None);
    }
    super::frontend::config::FrontendConfig::load_for_start(trust, &config.frontend_config)
        .map(|loaded| Some(loaded.run.name))
}

#[cfg(target_os = "linux")]
fn serve_linux_for(config: &ServeConfig) -> Result<(), String> {
    let mut env = LinuxEnv::production();
    if config.frontend_container {
        let trust = Trust::root();
        let bound = bound_container(&trust, config)?;
        env.state_name = super::frontend::Files::of(bound.as_deref()).state;
        let backend = super::frontend::production(config.frontend_config.clone(), bound);
        // A crash in the middle of a drain left the policy off: put it back before serving.
        match backend.recover_restart_policy() {
            Ok(Some(line)) => log(&line),
            Ok(None) => {}
            Err(why) => log(&format!("the restart policy was not checked: {why}")),
        }
        return serve_linux_with(config, &env, &trust, backend, &AtomicBool::new(false));
    }
    let format = match config.layout {
        Layout::Installer => {
            return serve_linux_with(
                config,
                &env,
                &Trust::root(),
                super::linux::production(config.units.clone()),
                &AtomicBool::new(false),
            );
        }
        Layout::Deb => super::package_format::Format::Deb,
        Layout::Rpm => super::package_format::Format::Rpm,
    };
    let backend = super::package::production(config.units.clone(), format, &env.state_dir);
    serve_linux_with(
        config,
        &env,
        &Trust::root(),
        backend,
        &AtomicBool::new(false),
    )
}

#[cfg(not(target_os = "linux"))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn serve_linux_for(_config: &ServeConfig) -> Result<(), String> {
    Err("this program serves a Linux host on Linux only".to_owned())
}

/// A Mac: the account of the socket from `dscl` (through `runner`, never root), the work
/// directory made, then the serving of the kit by `backend_runner`'s tools.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn serve_mac_with(
    config: &ServeConfig,
    host: MacHost,
    runner: &dyn Runner,
    backend_runner: Box<dyn Runner>,
    stop: &AtomicBool,
) -> Result<(), String> {
    use crate::mac::tools;
    if config.layout != Layout::Installer {
        return Err("--layout is for a Linux host: a Mac keeps its releases in the kit".to_owned());
    }
    if !config.units.is_empty() || config.frontend_container {
        return Err(
            "--restart-unit and --frontend-container are for a Linux host: a Mac restarts the \
             daemon's job"
                .to_owned(),
        );
    }
    let exec = runner
        .run(&tools::dscl_ids(&config.user, host.tuning.tool))
        .map_err(|e| format!("dscl: {e}"))?;
    let who = tools::parse_ids(&exec.stdout_text())
        .filter(|id| id.uid != 0 && id.gid != 0)
        .ok_or_else(|| format!("the account {} has no usable ids", config.user))?;
    crate::mac::ensure_work(&host).map_err(|_| "the work directory is not usable".to_owned())?;
    let (trust, work, audit) = (host.trust.clone(), host.work.clone(), host.audit.clone());
    let backend = super::mac_backend::MacBackend::new(host, backend_runner);
    let place = Place {
        state_dir: &work,
        state_name: "state.json",
        trust: &trust,
        ids: (who.uid, who.gid),
        audit: &audit,
    };
    serve_with(config, backend, &place, stop)
}

/// The Mac side as the program runs it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn serve_mac(config: &ServeConfig) -> Result<(), String> {
    serve_mac_with(
        config,
        MacHost::production(),
        &SystemRunner,
        Box::new(SystemRunner),
        &AtomicBool::new(false),
    )
}
