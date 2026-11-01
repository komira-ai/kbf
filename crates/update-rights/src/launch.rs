//! Running one step of a hook as a transient systemd unit with exactly the declared rights.
//!
//! The command line is a pure function of the declaration, the step and the generation
//! ([`command`]); starting it sits behind [`Spawner`], so a test runs the whole of
//! [`run_step`] against a fake. The properties are those of docs/design/host-updates.md,
//! section 3.4:
//!
//! | declaration | property |
//! |---|---|
//! | `hook.user`, `hook.group` | `User=`, `Group=` |
//! | the step's capabilities | `AmbientCapabilities=` and `CapabilityBoundingSet=`, exactly that list; `NoNewPrivileges=yes` |
//! | `read_paths`, `write_paths`, `state_dir` | `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `ReadOnlyPaths=`, `ReadWritePaths=` (the writable paths and the record directory) |
//! | `address_families` | `RestrictAddressFamilies=`, an empty value for an empty list (through `systemd-run -p` that is an empty allow list; `none` is refused as an unknown family) |
//! | `environment` | one `Environment=` per variable, and nothing inherited |
//! | `timeout_secs.<step>` | `RuntimeMaxSec=` |
//!
//! `ProtectSystem=strict` makes the whole tree read-only, so what stays writable is exactly
//! `ReadWritePaths=` and the API file systems; it also leaves the tree readable, so
//! `read_paths` says what the hook is declared to read and does not hide the rest.
//!
//! The unit is named `updater-<service>-<step>-<generation>`, so a resumed step can find and
//! stop the unit of the run it replaces. `--wait --collect --pipe` makes `systemd-run` wait
//! for the unit, remove it even when it failed, carry the hook's standard output through and
//! exit with the hook's exit status.
//!
//! The boot entry of a declaration (`boot`, section 3.5) runs through the same path
//! ([`boot_command`], [`run_boot`]): a transient unit `updater-<service>-boot-<generation>` with the
//! same properties, except that its capabilities are the hook-wide `capabilities` and never a
//! step's (it is not one of the five steps and has no `step_capabilities` key), and its bound is the
//! `rollback` bound (the declaration has no bound of its own for it, and what it does at boot is a
//! restore). It is run as `<release>/<binary> <hook_args> <boot>`, with no artifact and no params
//! file.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Display;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use update_hook::{Invocation, Outcome, Report, Step};

use crate::declaration::{Declaration, Refusal};
use crate::load::{Declared, recheck_targets};
use crate::names::Capability;

/// Where `systemd-run` is.
pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// The properties every step has, whatever the declaration says.
pub const FIXED_PROPERTIES: [&str; 10] = [
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectKernelLogs=yes",
    "ProtectControlGroups=yes",
    "RestrictNamespaces=yes",
    "RestrictRealtime=yes",
    "LockPersonality=yes",
    "MemoryDenyWriteExecute=yes",
    "SystemCallFilter=@system-service",
    "SystemCallArchitectures=native",
];

/// The unit that runs `step` of `service` for one order. The generation counts the runs of the
/// order, so a rerun after a crash has a name of its own and the old one can be found.
pub fn unit_name(service: &str, step: Step, generation: u64) -> String {
    format!("updater-{service}-{step}-{generation}")
}

/// The word of the boot entry's unit, where a step's unit has the step.
pub const BOOT_UNIT_WORD: &str = "boot";

/// The unit that runs the boot entry of `service`: `updater-<service>-boot-<generation>`. No step
/// is named `boot`, so it is never a step's unit.
pub fn boot_unit_name(service: &str, generation: u64) -> String {
    format!("updater-{service}-{BOOT_UNIT_WORD}-{generation}")
}

/// One run of one step.
#[derive(Clone, Copy, Debug)]
pub struct Launch<'a> {
    pub declaration: &'a Declaration,
    pub step: Step,
    pub generation: u64,
    /// The staged release tree: the hook is `release/<binary>`, and it is the argument of
    /// `prepare`.
    pub release: &'a Path,
    /// The file of the `[hook.params]` table, if the step is given one.
    pub params: Option<&'a Path>,
    // The uid of the hook account. Whether it is root is derived from this, in `root_hook`, the one
    // place, and is never a flag a caller supplies.
    hook_uid: u32,
    // The declared services of the host, for the check that a root hook stays in its own directory.
    names: &'a BTreeSet<String>,
}

impl<'a> Launch<'a> {
    /// A run of `step` of the hook of `declaration`, whose account has the uid `hook_uid`. Not
    /// public: the one way to build a launch from outside is [`Launch::of`], which takes the uid
    /// and the names from what the loader resolved.
    pub(crate) fn new(
        declaration: &'a Declaration,
        hook_uid: u32,
        names: &'a BTreeSet<String>,
        step: Step,
        generation: u64,
        release: &'a Path,
        params: Option<&'a Path>,
    ) -> Launch<'a> {
        Launch {
            declaration,
            step,
            generation,
            release,
            params,
            hook_uid,
            names,
        }
    }

    /// A run of a loaded declaration, whose hook account the loader resolved.
    pub fn of(
        declared: &'a Declared,
        step: Step,
        generation: u64,
        release: &'a Path,
        params: Option<&'a Path>,
    ) -> Launch<'a> {
        Launch::new(
            &declared.declaration,
            declared.hook.uid,
            &declared.names,
            step,
            generation,
            release,
            params,
        )
    }

    /// Whether the hook runs as uid 0 and is held to the rules of a root hook.
    pub fn root_hook(&self) -> bool {
        self.hook_uid == 0
    }
}

/// Why a step could not be started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    #[error("the staged release {0:?} is not an absolute path")]
    RelativeRelease(PathBuf),
    #[error("the params file {0:?} is not an absolute path")]
    RelativeParams(PathBuf),
}

/// The arguments of `systemd-run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdRun {
    pub args: Vec<OsString>,
}

fn words(items: impl Iterator<Item = impl Display>) -> String {
    items
        .map(|item| item.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `Name=value` properties of a step, in the order they are given.
pub fn properties(declaration: &Declaration, step: Step) -> Vec<String> {
    let hook = &declaration.hook;
    properties_with(
        declaration,
        hook.capabilities_for(step),
        hook.timeout_secs.get(step),
    )
}

/// The `Name=value` properties of the boot entry: those of a step, with the hook-wide
/// `capabilities` (never a step's) and the `rollback` bound.
pub fn boot_properties(declaration: &Declaration) -> Vec<String> {
    let hook = &declaration.hook;
    properties_with(declaration, &hook.capabilities, hook.timeout_secs.rollback)
}

/// The properties of one run that holds `capabilities` and is bounded by `timeout_secs`.
fn properties_with(
    declaration: &Declaration,
    capabilities: &BTreeSet<Capability>,
    timeout_secs: u64,
) -> Vec<String> {
    let hook = &declaration.hook;
    let capabilities = words(capabilities.iter());
    let families = if hook.address_families.is_empty() {
        String::new()
    } else {
        words(hook.address_families.iter())
    };
    let mut writable: BTreeSet<&str> = hook.write_paths.iter().map(String::as_str).collect();
    writable.insert(&declaration.state_dir);
    let read_only = (!hook.read_paths.is_empty())
        .then(|| format!("ReadOnlyPaths={}", words(hook.read_paths.iter())));
    let mut properties = vec![
        format!("User={}", hook.user),
        format!("Group={}", hook.group),
        format!("AmbientCapabilities={capabilities}"),
        format!("CapabilityBoundingSet={capabilities}"),
        "NoNewPrivileges=yes".to_owned(),
        "ProtectSystem=strict".to_owned(),
        "ProtectHome=yes".to_owned(),
        "PrivateTmp=yes".to_owned(),
    ];
    properties.extend(read_only);
    properties.push(format!("ReadWritePaths={}", words(writable.iter())));
    properties.push(format!("RestrictAddressFamilies={families}"));
    properties.extend(
        hook.environment
            .iter()
            .map(|(name, value)| format!("Environment={name}={value}")),
    );
    properties.push(format!("RuntimeMaxSec={timeout_secs}"));
    properties.extend(
        FIXED_PROPERTIES
            .iter()
            .map(|property| (*property).to_owned()),
    );
    properties
}

/// The `systemd-run` command that runs one step.
pub fn command(launch: &Launch<'_>) -> Result<SystemdRun, LaunchError> {
    if !launch.release.is_absolute() {
        return Err(LaunchError::RelativeRelease(launch.release.to_owned()));
    }
    if let Some(params) = launch.params.filter(|params| !params.is_absolute()) {
        return Err(LaunchError::RelativeParams(params.to_owned()));
    }
    let declaration = launch.declaration;
    let invocation = Invocation {
        step: launch.step,
        artifact: (launch.step == Step::Prepare).then(|| launch.release.to_owned()),
        params: launch.params.map(Path::to_owned),
    };
    let unit = unit_name(&declaration.name, launch.step, launch.generation);
    Ok(systemd_run(
        declaration,
        &unit,
        properties(declaration, launch.step),
        launch.release,
        invocation.to_args(),
    ))
}

/// `systemd-run` of the unit `unit` with `properties`, running the hook of `release` with its
/// declared arguments and then `tail`.
fn systemd_run(
    declaration: &Declaration,
    unit: &str,
    properties: Vec<String>,
    release: &Path,
    tail: Vec<OsString>,
) -> SystemdRun {
    let mut args: Vec<OsString> = ["--system", "--quiet", "--wait", "--collect", "--pipe"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(format!("--unit={unit}").into());
    args.extend(
        properties
            .into_iter()
            .map(|property| OsString::from(format!("--property={property}"))),
    );
    args.push("--".into());
    args.push(release.join(&declaration.binary).into());
    args.extend(declaration.hook_args.iter().map(OsString::from));
    args.extend(tail);
    SystemdRun { args }
}

/// The `systemd-run` command of the boot entry of a loaded declaration, run from the verified
/// release `release`: `<release>/<binary> <hook_args> <boot>` in the unit
/// `updater-<service>-boot-<generation>`, with [`boot_properties`]. `None` when the declaration
/// names no boot entry.
pub fn boot_command(
    declared: &Declared,
    generation: u64,
    release: &Path,
) -> Result<Option<SystemdRun>, LaunchError> {
    let declaration = &declared.declaration;
    let Some(boot) = &declaration.boot else {
        return Ok(None);
    };
    if !release.is_absolute() {
        return Err(LaunchError::RelativeRelease(release.to_owned()));
    }
    let unit = boot_unit_name(&declaration.name, generation);
    Ok(Some(systemd_run(
        declaration,
        &unit,
        boot_properties(declaration),
        release,
        vec![OsString::from(boot)],
    )))
}

/// What `systemd-run` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The exit status; `None` if `systemd-run` was killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Starts `systemd-run` and waits for it.
pub trait Spawner {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished>;
}

/// The real thing: runs a program (`systemd-run`) with an empty environment and no input.
#[derive(Clone, Debug)]
pub struct Systemd {
    program: PathBuf,
}

impl Systemd {
    pub fn new(program: &Path) -> Systemd {
        Systemd {
            program: program.to_owned(),
        }
    }

    /// `/usr/bin/systemd-run`.
    pub fn system() -> Systemd {
        Systemd::new(Path::new(SYSTEMD_RUN))
    }
}

impl Spawner for Systemd {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished> {
        let output = Command::new(&self.program)
            .args(&command.args)
            .env_clear()
            .stdin(Stdio::null())
            .output()?;
        Ok(Finished {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRun {
    pub unit: String,
    pub outcome: Outcome,
    /// The last line of the hook's standard output, read as the updater reads it.
    pub report: Report,
    /// The hook's standard error, for the log.
    pub stderr: String,
}

/// Why a step did not run to an end.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error("the targets changed since the load: {0}")]
    Recheck(Refusal),
    #[error("systemd-run could not be started: {0}")]
    Spawn(#[from] io::Error),
}

/// Runs one step and reads how it ended: the exit status as an [`Outcome`] (a status of no
/// contract, a signal and a timeout are failures) and the last line of standard output as a
/// [`Report`] (a missing or malformed line is `{}`).
pub fn run_step(spawner: &dyn Spawner, launch: &Launch<'_>) -> Result<StepRun, RunError> {
    recheck_targets(launch.declaration, launch.hook_uid, launch.names)
        .map_err(RunError::Recheck)?;
    let command = command(launch)?;
    let finished = spawner.run(&command)?;
    Ok(read_run(
        unit_name(&launch.declaration.name, launch.step, launch.generation),
        &finished,
    ))
}

/// Runs the boot entry of `declared` from the verified release `release` and reads how it ended,
/// as [`run_step`] reads a step: the targets are checked again first, and the exit status and
/// the last line are read the same way. `Ok(None)` when the declaration names no boot entry;
/// nothing is spawned then.
pub fn run_boot(
    spawner: &dyn Spawner,
    declared: &Declared,
    generation: u64,
    release: &Path,
) -> Result<Option<StepRun>, RunError> {
    let Some(command) = boot_command(declared, generation, release)? else {
        return Ok(None);
    };
    declared.recheck().map_err(RunError::Recheck)?;
    let finished = spawner.run(&command)?;
    Ok(Some(read_run(
        boot_unit_name(&declared.declaration.name, generation),
        &finished,
    )))
}

/// How a run in `unit` ended, as the updater reads it.
fn read_run(unit: String, finished: &Finished) -> StepRun {
    StepRun {
        unit,
        outcome: Outcome::from_exit(finished.code),
        report: Report::read(&String::from_utf8_lossy(&finished.stdout)),
        stderr: String::from_utf8_lossy(&finished.stderr).into_owned(),
    }
}
