//! The two adapters that join the step runner to the declarations
//! (docs/design/host-updates.md, 3.4): [`HookLauncher`] runs a step the runner chose as the
//! transient systemd unit `update_rights` builds from the loaded declaration of its service, and
//! [`Grants`] answers the registration socket from the same declarations. Neither library knows
//! the other; this module is the only place they meet.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use kbf_step_runner::register::{self, Grant};
use kbf_step_runner::{LaunchError, StepCall, StepExit, StepLauncher};
use update_hook::Step;
use update_rights::{
    Declared, Launch, Loaded, Spawner, SystemdRun, check_state_dir, command, unit_name,
};

/// What `systemd-run --wait` exits with when the unit's program could not be executed
/// (systemd's `EXIT_EXEC`): the hook did not start.
pub const EXIT_EXEC: i32 = 203;
/// What `systemctl stop` exits with for a unit that is not loaded: nothing to stop.
pub const EXIT_NOT_LOADED: i32 = 5;

/// Where `systemctl` is.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// The copy of a step's params file in the hook's record directory, the one the hook is given.
pub const PARAMS_FILE: &str = "params";

/// Copies the params file `from` (in the root part's own directory, which only root may enter)
/// into `record_dir`, the hook's record directory, as `params`: root-owned, mode 0644, written to a
/// file made new without following a link and renamed into place, so a hook of any account reads
/// it and none can redirect the write. The copy's path.
fn expose_params(from: &Path, record_dir: &Path) -> io::Result<PathBuf> {
    let bytes = fs::read(from)?;
    let target = record_dir.join(PARAMS_FILE);
    let tmp = record_dir.join(format!(".{PARAMS_FILE}.new"));
    // A stale one goes; one that cannot go makes the create below fail.
    let _ = fs::remove_file(&tmp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, &target)?;
    Ok(target)
}

/// `command` with its unit's `RuntimeMaxSec=` no longer than `timeout_secs`, the bound the step
/// runner gives this run (shorter than the declared one when the verify window has less left), so
/// the unit ends by it even when the root part and its dead-man are gone.
fn bounded(mut command: SystemdRun, timeout_secs: u64) -> SystemdRun {
    for arg in &mut command.args {
        let declared = arg
            .to_str()
            .and_then(|text| text.strip_prefix("--property=RuntimeMaxSec="))
            .and_then(|secs| secs.parse::<u64>().ok());
        if let Some(declared) = declared {
            *arg = OsString::from(format!(
                "--property=RuntimeMaxSec={}",
                declared.min(timeout_secs)
            ));
        }
    }
    command
}

/// The declarations of the host, as the root part loaded them last.
pub type Shared = Arc<RwLock<Loaded>>;

/// The loaded declaration of `service`, `None` when it is not declared or was refused.
pub fn declared(loaded: &Shared, service: &str) -> Option<Declared> {
    let loaded = loaded.read().unwrap_or_else(PoisonError::into_inner);
    loaded.services.get(service).cloned()
}

/// Runs the steps of the step runner under the rights of their service's declaration.
pub struct HookLauncher {
    loaded: Shared,
    systemd_run: Arc<dyn Spawner + Send + Sync>,
    systemctl: Arc<dyn Spawner + Send + Sync>,
}

impl HookLauncher {
    /// A launcher that starts units with `systemd_run` (`systemd-run`) and stops them with
    /// `systemctl`.
    pub fn new(
        loaded: Shared,
        systemd_run: Arc<dyn Spawner + Send + Sync>,
        systemctl: Arc<dyn Spawner + Send + Sync>,
    ) -> HookLauncher {
        HookLauncher {
            loaded,
            systemd_run,
            systemctl,
        }
    }
}

fn failed(why: impl std::fmt::Display) -> LaunchError {
    LaunchError::Launcher(why.to_string())
}

/// The generation in the runner's unit name `updater-<service>-<step>-<generation>`, which is
/// the name `update_rights` gives the unit of that generation.
fn generation(unit: &str, service: &str, step: Step) -> Result<u64, LaunchError> {
    unit.strip_prefix(&format!("updater-{service}-{step}-"))
        .and_then(|number| number.parse::<u64>().ok())
        .filter(|number| unit_name(service, step, *number) == unit)
        .ok_or_else(|| failed(format!("{unit:?} is not a unit of {step} of {service}")))
}

/// The release the runner checked the hook in: `executable` is `<release>/<binary>`.
fn release_of(executable: &Path, binary: &str) -> Result<PathBuf, LaunchError> {
    let depth = Path::new(binary).components().count();
    executable
        .ancestors()
        .nth(depth)
        .map(Path::to_path_buf)
        .filter(|release| release.join(binary) == executable)
        .ok_or_else(|| {
            failed(format!(
                "{executable:?} is not the hook {binary:?} of a release"
            ))
        })
}

/// The last line of a step's standard output that is not blank.
fn last_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_owned)
}

impl StepLauncher for HookLauncher {
    /// Runs the step as `update_rights` builds it from the declaration: the user, the
    /// capabilities of that step, the paths, the units and the bound (never longer than the
    /// runner's bound for this run). Before the launch the record directory is checked again (the
    /// service's account may not write it) and so is every path of the declaration, on what it
    /// points to now; the params file is given to the hook as a copy in that directory
    /// ([`PARAMS_FILE`]).
    fn run(&self, call: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        let declared = declared(&self.loaded, call.service)
            .ok_or_else(|| failed(format!("{} is not declared", call.service)))?;
        let declaration = declared.declaration();
        let step = call.invocation.step;
        let generation = generation(call.unit, call.service, step)?;
        let release = release_of(call.executable, &declaration.binary)?;
        check_state_dir(
            &declaration.state_dir,
            declared.service().uid,
            declared.service_gids(),
        )
        .map_err(failed)?;
        declared.recheck().map_err(failed)?;
        let params = call
            .invocation
            .params
            .as_deref()
            .map(|from| expose_params(from, Path::new(&declaration.state_dir)))
            .transpose()
            .map_err(failed)?;
        let launch = Launch::of(&declared, step, generation, &release, params.as_deref());
        let command = bounded(command(&launch).map_err(failed)?, call.timeout_secs);
        let finished = self.systemd_run.run(&command)?;
        if finished.code == Some(EXIT_EXEC) {
            return Err(LaunchError::NoStart(format!(
                "{} could not be executed",
                call.executable.display()
            )));
        }
        Ok(StepExit {
            code: finished.code,
            last_line: last_line(&finished.stdout),
        })
    }

    /// `systemctl stop <unit>`, which waits until the unit has ended; a unit that is not loaded
    /// has nothing to stop.
    fn stop(&self, unit: &str) -> Result<(), LaunchError> {
        let done = self.systemctl.run(&SystemdRun {
            args: vec!["stop".into(), unit.into()],
        })?;
        match done.code {
            Some(0 | EXIT_NOT_LOADED) => Ok(()),
            code => Err(failed(format!(
                "systemctl stop {unit} exited {code:?}: {}",
                String::from_utf8_lossy(&done.stderr).trim()
            ))),
        }
    }
}

/// The registration socket's view of the declarations: who a service runs as, the health ports
/// it may name, and how long a registration loaded from disk is believed.
pub struct Grants(pub Shared);

impl register::Declared for Grants {
    fn grant(&self, name: &str) -> Option<Grant> {
        declared(&self.0, name).map(|declared| {
            let declaration = declared.declaration();
            Grant {
                uid: declared.service().uid,
                health_ports: declaration.health_ports.iter().copied().collect(),
                registration_ttl_secs: declaration.registration_ttl_secs,
            }
        })
    }
}
