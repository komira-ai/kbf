//! `kbf-host-install --migrate-to-package`: move a Linux daemon host from the hand-placed
//! layout (`/var/lib/kbf-installer/releases/<commit>`, a unit in `/etc/systemd/system` that
//! runs `releases/current/kbf` with the node's flags) to the `kbf` package, once, by an
//! operator, as root.
//!
//! ```text
//! kbf-host-install --migrate-to-package --layout deb|rpm --channel NAME --unit UNIT --dir DIR [--apply]
//! ```
//!
//! It plans by default: the plan is printed, `DRY-RUN:` before each step, and nothing on the
//! host changes. `--apply` carries it out. The checks that come first run in both and change
//! nothing: the host's installer state names the running commit and has no release on trial,
//! the unit is one this program can carry over ([`unit::parse`]), and the package for that very
//! commit in `--dir` (the three files of the release: `kbf-<commit>-linux-<cpu>.<deb|rpm>`,
//! its `.sigstore.json` bundle and `SHA256SUMS`) passes every check a package passes before an
//! update installs it ([`PackageHost::check_copies`]), and is the commit the host runs.
//!
//! The steps, in order:
//!
//! 1. the package manager installs the verified file (the units it ships are disabled and sit
//!    under `/usr/lib/systemd/system`, behind the hand-placed unit in `/etc/systemd/system`,
//!    so nothing changes for the running node);
//! 2. a drop-in `/etc/systemd/system/<unit>.d/10-node-flags.conf` is written: the node's flags
//!    on the packaged unit's `ExecStart=` and `--check`, its account, and the few settings
//!    named in [`unit::HandPlaced`], and the packaged unit's `kbf.toml` condition lifted (the
//!    flags are the node's configuration);
//! 3. the hand-placed unit is renamed `<unit>.hand-placed`, so the packaged one takes its
//!    place, and systemd rereads its files;
//! 4. the unit restarts and is waited for;
//! 5. the check: the unit's main process runs `/usr/bin/kbf` and that binary reports the
//!    commit the host ran;
//! 6. only then is the old layout (`releases/`) removed.
//!
//! A failure after step 1 puts the hand-placed unit back (the drop-in is removed, systemd
//! rereads, the unit restarts) and leaves the old layout alone; the package stays installed.

mod unit;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kbf_installer::state;
use serde_json::Value;

use crate::local::backend::Fault;
use crate::local::package::{Copies, PackageHost};
use crate::local::package_format::Format;
use crate::local::units::restart_and_wait;
use crate::mac::release::{self, commit_prefix};
use crate::plan::{self, SYSTEMCTL};
use crate::request::{Channel, Unit};
use crate::runner::{Cmd, Identity, Runner};

pub const USAGE: &str = "usage: kbf-host-install --migrate-to-package --layout deb|rpm \
--channel NAME --unit UNIT --dir DIR [--apply]\n";

/// The units of the modes a daemon host runs. (A frontend is not migrated here.)
const MODES: [&str; 4] = ["daemon", "storage", "storage-and-daemon", "all-in-one"];

/// The name of the drop-in.
const DROP_IN: &str = "10-node-flags.conf";

/// The suffix the hand-placed unit is kept under.
const ASIDE: &str = ".hand-placed";

/// The packaged binary.
const KBF: &str = "/usr/bin/kbf";

/// The flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub format: Format,
    pub channel: Channel,
    pub unit: Unit,
    pub dir: PathBuf,
    pub apply: bool,
}

impl Config {
    /// Read the flags (after `--migrate-to-package`).
    ///
    /// # Errors
    /// What is wrong with them, in a line.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let (mut layout, mut channel, mut unit, mut dir) = (None, None, None, None);
        let mut apply = false;
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            if name == "--apply" && inline.is_none() {
                apply = true;
                continue;
            }
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))?,
            };
            let slot = match name {
                "--layout" => &mut layout,
                "--channel" => &mut channel,
                "--unit" => &mut unit,
                "--dir" => &mut dir,
                _ => return Err(format!("unknown argument {name}")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("{name} is given twice"));
            }
        }
        let format = match layout.as_deref() {
            Some("deb") => Format::Deb,
            Some("rpm") => Format::Rpm,
            _ => return Err("--layout is required and is deb or rpm".to_owned()),
        };
        let channel = channel.ok_or("--channel is required")?;
        let unit = unit.ok_or("--unit is required")?;
        let dir = PathBuf::from(dir.ok_or("--dir is required")?);
        if !dir.is_absolute() {
            return Err("--dir must be an absolute path".to_owned());
        }
        Ok(Self {
            format,
            channel: Channel::new(&channel)
                .map_err(|_| format!("--channel {channel:?} is not a channel"))?,
            unit: Unit::service(&unit).map_err(|_| format!("--unit {unit:?} is not a kbf unit"))?,
            dir,
            apply,
        })
    }
}

/// Where the things the migration touches outside the package manager are.
#[derive(Debug, Clone)]
pub struct Place {
    /// `/etc/systemd/system`.
    pub systemd_dir: PathBuf,
    /// `/proc`.
    pub proc_dir: PathBuf,
    /// `/usr/bin/kbf`, the packaged binary the unit must come to run.
    pub kbf: PathBuf,
}

impl Place {
    pub fn production() -> Self {
        Self {
            systemd_dir: PathBuf::from("/etc/systemd/system"),
            proc_dir: PathBuf::from("/proc"),
            kbf: PathBuf::from(KBF),
        }
    }
}

/// One step of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Install(PathBuf),
    DropIn { path: PathBuf, text: String },
    MoveAside { from: PathBuf, to: PathBuf },
    Reload,
    Restart,
    Check { commit: String },
    RemoveOldLayout(PathBuf),
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Install(file) => write!(f, "install the verified package {}", file.display()),
            Self::DropIn { path, text } => {
                write!(f, "write {}:", path.display())?;
                for line in text.lines() {
                    write!(f, "\n    | {line}")?;
                }
                Ok(())
            }
            Self::MoveAside { from, to } => {
                write!(f, "rename {} to {}", from.display(), to.display())
            }
            Self::Reload => f.write_str("systemctl daemon-reload"),
            Self::Restart => f.write_str("restart the unit and wait until it is active"),
            Self::Check { commit } => write!(
                f,
                "check that the unit's main process runs /usr/bin/kbf and that it reports commit {}",
                &commit[..12]
            ),
            Self::RemoveOldLayout(dir) => write!(
                f,
                "remove the old layout {} (only after the check above)",
                dir.display()
            ),
        }
    }
}

/// A refusal: nothing was changed.
struct Refused(String);

fn refused<T>(why: impl Into<String>) -> Result<T, Refused> {
    Err(Refused(why.into()))
}

impl From<Fault> for Refused {
    fn from(fault: Fault) -> Self {
        Self(fault.detail)
    }
}

/// The drop-in for a unit that runs `old`'s flags as the mode `mode`.
fn drop_in(old: &unit::HandPlaced, mode: &str) -> String {
    let mut flags = old.args.clone();
    if !flags.iter().any(|w| w.starts_with("--mode")) {
        flags.insert(0, format!("--mode={mode}"));
    }
    let flags = flags.join(" ");
    let mut text = String::from(
        "# Written by kbf-host-install --migrate-to-package: the flags this node's hand-placed\n\
         # unit gave kbf, now on the kbf package's unit and its /usr/bin/kbf.\n\
         [Unit]\n\
         ConditionPathExists=\n\n\
         [Service]\n",
    );
    text.push_str(&format!("User={}\n", old.user.as_deref().unwrap_or("root")));
    if let Some(group) = &old.group {
        text.push_str(&format!("Group={group}\n"));
    }
    for (directive, value) in &old.carried {
        text.push_str(&format!("{directive}={value}\n"));
    }
    text.push_str(&format!(
        "ExecStart=\nExecStart={KBF} {flags}\nExecStartPre=\n\
         ExecStartPre=+{KBF} boot-check --mode={mode}\nExecStartPre={KBF} {flags} --check\n"
    ));
    text
}

fn systemctl(args: &[&str]) -> Cmd {
    Cmd {
        program: SYSTEMCTL.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        timeout: Some(std::time::Duration::from_secs(120)),
        ..Cmd::default()
    }
}

/// What the plan is made of, and the copies that must outlive it: the package is installed
/// from them.
struct Planned {
    steps: Vec<Step>,
    _copies: Copies,
}

/// Everything that is checked before anything changes, and the plan.
fn plan_of(config: &Config, host: &PackageHost, place: &Place) -> Result<Planned, Refused> {
    let layout = &host.layout;
    let releases = layout.join(&layout.releases());
    // 1. The unit is the unit of a daemon mode.
    let mode = config
        .unit
        .as_str()
        .strip_prefix("kbf-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .filter(|mode| MODES.contains(mode))
        .ok_or_else(|| {
            Refused(format!(
                "{} is not the unit of a daemon mode ({})",
                config.unit.as_str(),
                MODES.join(", ")
            ))
        })?;
    // 2. The host runs a hand-placed release, and none is on trial.
    let state =
        state::read_unlocked(layout).map_err(|e| Refused(format!("the installer's state: {e}")))?;
    let Some(state) = state else {
        return refused("this host has no installer state: it is not on the hand-placed layout");
    };
    let Some(current) = state.current else {
        return refused("the installer's state names no release that runs");
    };
    if state.pending_confirm.is_some() {
        return refused("a release is on trial: confirm or revert it first");
    }
    let commit = current.to_string();

    // 3. The unit is one the migration carries over: root's alone to write, a plain file.
    let unit_file = place.systemd_dir.join(config.unit.as_str());
    let text = host
        .host
        .trust
        .read(&unit_file)
        .map_err(|e| Refused(format!("{}: {e}", unit_file.display())))?;
    let old = unit::parse(&text, &releases.display().to_string()).map_err(Refused)?;
    if let Some(mode_flag) = old.args.iter().find_map(|w| w.strip_prefix("--mode="))
        && mode_flag != mode
    {
        return refused(format!(
            "the unit runs --mode={mode_flag}, which is not the mode of {}",
            config.unit.as_str()
        ));
    }
    let drop_in_dir = place
        .systemd_dir
        .join(format!("{}.d", config.unit.as_str()));
    let aside = place
        .systemd_dir
        .join(format!("{}{ASIDE}", config.unit.as_str()));
    for taken in [drop_in_dir.join(DROP_IN), aside.clone()] {
        if fs::symlink_metadata(&taken).is_ok() {
            return refused(format!("{} is there already", taken.display()));
        }
    }

    // 3. The package for that very commit passes every check.
    let (settings, _token) = host.prepare(&config.channel)?;
    let who = Identity {
        uid: settings.run_as.uid,
        gid: settings.run_as.gid,
    };
    let platform = format!("linux-{}", settings.arch);
    let file_name = release::asset_name_on(&platform, config.format.asset().extension(), &commit);
    let work = host.layout.join(&host.layout.work_dir());
    let copies = host.copy_in(
        (&work, &commit),
        who,
        [
            &config.dir.join(&file_name),
            &config.dir.join(release::bundle_of(&file_name)),
            &config.dir.join(release::SUMS_NAME),
        ],
        &file_name,
    )?;
    let meta = host.check_copies(&copies, &current, &settings)?;
    if commit_prefix(&meta.version) != Some(&commit[..12]) {
        return refused(format!(
            "the package is version {}, which is not the commit {} the host runs",
            meta.version,
            &commit[..12]
        ));
    }

    let steps = vec![
        Step::Install(copies.package.clone()),
        Step::DropIn {
            path: drop_in_dir.join(DROP_IN),
            text: drop_in(&old, mode),
        },
        Step::MoveAside {
            from: unit_file,
            to: aside,
        },
        Step::Reload,
        Step::Restart,
        Step::Check { commit },
        Step::RemoveOldLayout(releases),
    ];
    Ok(Planned {
        steps,
        _copies: copies,
    })
}

/// The commit `kbf --version --json` reports.
fn reported_commit(runner: &dyn Runner) -> Option<String> {
    let exec = runner
        .run(&plan::running_build())
        .ok()
        .filter(crate::runner::Exec::success)?;
    let build: Value = serde_json::from_str(&exec.stdout_text()).ok()?;
    ["commit_full", "commit"]
        .iter()
        .filter_map(|key| build.get(key)?.as_str())
        .find(|commit| (1..=64).contains(&commit.len()))
        .map(str::to_owned)
}

/// Carry out one step.
fn apply(step: &Step, host: &PackageHost, config: &Config, place: &Place) -> Result<(), String> {
    let runner = &*host.runner;
    let ran = |cmd: &Cmd, what: &str| match runner.run(cmd) {
        Ok(exec) if exec.success() => Ok(exec),
        Ok(exec) => Err(format!(
            "{what}: exit status {:?}: {}",
            exec.code,
            exec.stderr_text().trim()
        )),
        Err(e) => Err(format!("{what}: {e}")),
    };
    match step {
        Step::Install(file) => {
            ran(&config.format.install(file), "installing the package").map(drop)
        }
        Step::DropIn { path, text } => {
            let dir = path.parent().ok_or("the drop-in has no directory")?;
            fs::create_dir_all(dir)
                .and_then(|()| fs::set_permissions(dir, fs::Permissions::from_mode(0o755)))
                .and_then(|()| fs::write(path, text))
                .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o644)))
                .map_err(|e| format!("{}: {e}", path.display()))
        }
        Step::MoveAside { from, to } => {
            fs::rename(from, to).map_err(|e| format!("{}: {e}", from.display()))
        }
        Step::Reload => ran(&systemctl(&["daemon-reload"]), "systemctl daemon-reload").map(drop),
        Step::Restart => restart_and_wait(runner, &config.unit, host.restart_wait, &host.pace),
        Step::Check { commit } => check(host, config, place, commit),
        Step::RemoveOldLayout(dir) => {
            fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
        }
    }
}

/// The unit's main process runs the packaged binary, and the binary is the commit the host ran.
fn check(host: &PackageHost, config: &Config, place: &Place, commit: &str) -> Result<(), String> {
    let runner = &*host.runner;
    let shown = runner
        .run(&systemctl(&[
            "show",
            "--property=MainPID",
            "--value",
            "--",
            config.unit.as_str(),
        ]))
        .map_err(|e| format!("systemctl show: {e}"))?;
    let pid: u32 = shown
        .stdout_text()
        .trim()
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or("the unit has no main process")?;
    let exe = fs::read_link(place.proc_dir.join(pid.to_string()).join("exe"))
        .map_err(|e| format!("the main process {pid}: {e}"))?;
    if exe != place.kbf {
        return Err(format!(
            "the main process runs {}, not {}",
            exe.display(),
            place.kbf.display()
        ));
    }
    match reported_commit(runner) {
        Some(reported) if commit.starts_with(&reported) => Ok(()),
        Some(reported) => Err(format!(
            "{} reports commit {reported}, not {}",
            place.kbf.display(),
            &commit[..12]
        )),
        None => Err(format!("{} reports no commit", place.kbf.display())),
    }
}

/// Put the hand-placed unit back after a failure: the drop-in goes, the unit is renamed back,
/// systemd rereads and the unit restarts. Best effort, and says what it could not do.
fn put_back(host: &PackageHost, config: &Config, place: &Place) -> Result<(), String> {
    let unit_file = place.systemd_dir.join(config.unit.as_str());
    let aside = place
        .systemd_dir
        .join(format!("{}{ASIDE}", config.unit.as_str()));
    let drop_in_dir = place
        .systemd_dir
        .join(format!("{}.d", config.unit.as_str()));
    let mut problems = Vec::new();
    let mut note = |step: &str, result: std::io::Result<()>| match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            problems.push(format!("{step}: {e}"));
        }
        _ => {}
    };
    note(
        "removing the drop-in",
        fs::remove_file(drop_in_dir.join(DROP_IN)),
    );
    note(
        "removing the drop-in directory",
        fs::remove_dir(&drop_in_dir),
    );
    if fs::symlink_metadata(&aside).is_ok() {
        note("renaming the unit back", fs::rename(&aside, &unit_file));
    }
    let reloaded = host.runner.run(&systemctl(&["daemon-reload"]));
    if !matches!(&reloaded, Ok(exec) if exec.success()) {
        problems.push("systemctl daemon-reload did not work".to_owned());
    }
    if let Err(why) = restart_and_wait(&*host.runner, &config.unit, host.restart_wait, &host.pace) {
        problems.push(why);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Run the migration against `host`: print the plan, and carry it out with `--apply`. The
/// exit status: 0 done or planned, 1 a step failed, 2 refused.
pub fn run_with(
    config: &Config,
    host: &PackageHost,
    place: &Place,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let planned = match plan_of(config, host, place) {
        Ok(planned) => planned,
        Err(Refused(why)) => {
            let _ = writeln!(err, "kbf-host-install: refused: {why}");
            return 2;
        }
    };
    if !config.apply {
        for step in &planned.steps {
            let _ = writeln!(out, "DRY-RUN: {step}");
        }
        let _ = writeln!(out, "dry run: nothing changed. Re-run with --apply.");
        return 0;
    }
    for (index, step) in planned.steps.iter().enumerate() {
        let _ = writeln!(out, "+ {step}");
        if let Err(why) = apply(step, host, config, place) {
            let _ = writeln!(err, "kbf-host-install: step {} failed: {why}", index + 1);
            // Up to the removal, the old layout is whole: the old unit comes back. (A removal
            // that fails leaves a node that runs the package and a directory to delete.)
            if index >= 1 && !matches!(step, Step::RemoveOldLayout(_)) {
                match put_back(host, config, place) {
                    Ok(()) => {
                        let _ = writeln!(
                            err,
                            "kbf-host-install: the hand-placed unit is back and running"
                        );
                    }
                    Err(problems) => {
                        let _ =
                            writeln!(err, "kbf-host-install: putting it back failed: {problems}");
                    }
                }
            }
            return 1;
        }
    }
    let _ = writeln!(
        out,
        "migrated: {} runs the kbf package",
        config.unit.as_str()
    );
    0
}

/// The program: the flags, then the migration of this host.
pub fn main(args: &[String]) -> ExitCode {
    let config = match Config::parse(args) {
        Ok(config) => config,
        Err(why) => {
            eprintln!("kbf-host-install: {why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let host = crate::local::package::production(
        Vec::new(),
        config.format,
        Path::new("/var/lib/kbf-host-install"),
    );
    ExitCode::from(run_with(
        &config,
        &host,
        &Place::production(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
