//! Removing what a package left behind: `kbf-installer uninstall`.
//!
//! A rollback undoes an *adoption*; the package (its receipt, its LaunchDaemon plist, its
//! binary, its logs) belongs to the package manager, and the installer never updates or
//! removes itself. So after a kit host is rolled back a Mac still carries the plist, the log
//! directory, the receipt, the launchd disable override of the installer's job and the
//! `.rolled-back-...` files the rollback moved aside. This verb removes them, once the
//! installer manages nothing:
//!
//! 1. **Refuse** unless the host is rolled back (journal phase `RolledBack`) or was never
//!    adopted (no journal and no `state.json`): a journal that is `Started` or `Done`, or a
//!    state the installer wrote, means the installer runs this host, and the way out is
//!    `migrate --rollback`. (An archived journal, `migration.json.rolled-back-...`, is a host
//!    an earlier uninstall finished: the `state.json` of the adoption it undid is not a reason.) The refusal is [`MigrateError::Adopted`] (exit 5).
//!    Handing a rolled-back host to the installer again ends in a refusal on every path:
//!    `migrate --apply` takes the same journal to `Started` and `Done`; `setup` is refused
//!    (exit 5) while the old layout is there; and when the old layout was removed by hand
//!    `setup` goes ahead with the journal still `RolledBack`, so the gate also refuses a
//!    rolled-back journal whose `state.json` differs from what the migration adopted
//!    (another `current` or `previous`, or a release staged or pending).
//! 2. **Lock**: the installer's own lock, the one a `run` pass holds, waited for
//!    [`Uninstall::lock_wait`] and then busy (exit 4), and held to the end, so the plist
//!    cannot be pulled from under a pass. The refusal is judged again under it.
//! 3. **Act** (a Mac), in this order, each step through the host's [`Exec`]: boot the job
//!    out (only if loaded), remove its plist, clear its disable override (`launchctl
//!    enable`), forget the package receipt (`pkgutil --forget`, only if the receipt is
//!    there), remove the log directory, `installer.toml` and every `installer.toml.rolled-back-*`,
//!    the installer's tree and every `kbf-installer.rolled-back-*`, and last archive the journal
//!    under the suffix the rollback chose (`migration.json.rolled-back-<UTC time>`): the
//!    journal is renamed in effect, never deleted. A Linux host's package files (the unit's
//!    file, the binary, the trust files) belong to the package manager and are left to it: the
//!    verb stops and disables the installer's unit when `--installer-unit` names it (as the
//!    rollback does), and archives the journal. The unit must be of the platform's kind
//!    (`.plist` on a Mac, `.service` on Linux), else the verb ends as a usage error (exit 2)
//!    before anything runs. On a Mac `--installer-unit` replaces the package's job as the one
//!    booted out, removed and enabled; the package's own plist, if it is another file, is left,
//!    while the tree, logs and receipt go as always.
//! 4. **Dry run unless `--apply`**: every step above is printed as the line it would run
//!    (`remove /path`, `remove tree /path`, the receipt id), one per line, and nothing is
//!    changed.
//! 5. **Resume**: `uninstall.json` in the state directory names the suffix and the steps done,
//!    written after each step; a run again skips them. A step that finds its object gone
//!    is not planned at all, so a run on a host already clean says so and does nothing.
//!    The record is removed just before the journal is archived, which is last, so that a
//!    run again always meets the same journal the first one met.
//!
//! The launchd and `pkgutil` forms are UNVERIFIED on a Mac of the farm, like those of
//! [`super::services`]: `launchctl enable system/<label>` is the documented way to clear a
//! disable, `pkgutil --pkg-info <id>` exits non-zero for a receipt that is not there and
//! `pkgutil --forget <id>` drops one.

use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_host::path::HostPath;
use kbf_host::{Cmd, Entry, Exec, Kind, Meta, Mode, Perms, Step};
use serde::{Deserialize, Serialize};

use super::apply::KIT_INSTALLER_JOB;
use super::error::MigrateError;
use super::journal::{Journal, Phase};
use super::newtree::{JOURNAL, StateDir};
use super::rollback::{ASIDE_PREFIX, aside_names, moved_on};
use super::roots::Roots;
use super::services::Services;
use super::tree::Rules;
use super::units::Unit;
use super::{Outcome, Report};
use crate::utc_stamp::utc_stamp;
use crate::layout::{Layout, Platform};
use crate::state::{self, Locked, State, StateError};

/// The package's identifier, which `pkgutil --forget` takes: the same string as the job's
/// label (`MAC_INSTALLER_LABEL`; `crates/xtask/src/pkg.rs` makes the package with it).
const RECEIPT: &str = "com.komira.kbf-installer";
const PKGUTIL: &str = "/usr/sbin/pkgutil";
/// Where the package's job writes (`LOG_DIR` of the package builder, as a host path).
const MAC_LOG_DIR: &str = "/Library/Logs/kbf-installer";
/// The record of an uninstall under way, in the state directory.
const PROGRESS: &str = "uninstall.json";

/// What an uninstall needs to know.
pub struct Uninstall {
    pub layout: Layout,
    pub roots: Roots,
    /// The user that must own every file read: root on a host.
    pub owner_uid: u32,
    /// The installer's own unit. A Mac always has one (the package's job, when none is named);
    /// a Linux host acts on its unit only when it is named.
    pub installer_unit: Option<Unit>,
    /// How long the installer's lock is waited for before the verb ends busy (exit 4).
    pub lock_wait: Duration,
}

/// `uninstall.json`: where an uninstall got to.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    schema: u64,
    /// The suffix the journal is archived under, chosen once.
    suffix: String,
    /// The steps finished, by their ids ([`Act::id`]).
    done: Vec<String>,
}

/// One thing an uninstall does.
#[derive(Debug, PartialEq, Eq)]
enum Act {
    /// Boot out (launchd) or stop (systemd) the installer's unit.
    Stop(Unit),
    /// Keep a systemd unit from starting again.
    Disable(Unit),
    /// Clear the launchd disable override of the unit, so a package installed again is not
    /// left disabled.
    ClearOverride(Unit),
    /// Forget the package receipt.
    Forget,
    /// Remove a file, or a tree with everything below it.
    Remove { path: HostPath, tree: bool },
    /// Keep the journal under another name, then take the original away.
    Archive {
        from: HostPath,
        to: HostPath,
        journal: Box<Journal>,
    },
}

impl Act {
    /// The removal of what is at `path`: a tree if it is a directory, else the one file or link.
    fn removal(path: HostPath, meta: Meta) -> Act {
        Act::Remove {
            tree: meta.kind == Kind::Dir,
            path,
        }
    }

    /// The name the progress record keeps.
    fn id(&self) -> String {
        match self {
            Act::Stop(unit) => format!("stop {unit}"),
            Act::Disable(unit) => format!("disable {unit}"),
            Act::ClearOverride(unit) => format!("enable {unit}"),
            Act::Forget => format!("forget {RECEIPT}"),
            Act::Remove { path, .. } => format!("remove {path}"),
            Act::Archive { to, .. } => format!("archive {to}"),
        }
    }

    /// What the report says, which for a removal is the line the step prints.
    fn describe(&self, platform: Platform) -> String {
        match self {
            Act::Stop(unit) if platform == Platform::MacOs => format!("boot out {unit}"),
            Act::Stop(unit) => format!("stop {unit}"),
            Act::Disable(unit) => format!("disable {unit}"),
            Act::ClearOverride(unit) => {
                format!("clear the disable override of {unit} (launchctl enable)")
            }
            Act::Forget => format!("forget the package receipt {RECEIPT} (pkgutil --forget)"),
            Act::Remove { path, tree: false } => Step::Remove { path: path.clone() }.to_string(),
            Act::Remove { path, tree: true } => Step::RemoveTree { path: path.clone() }.to_string(),
            Act::Archive { from, to, .. } => {
                format!("archive the journal {from} as {to} (kept, never deleted)")
            }
        }
    }
}

/// What a look at the journal came to.
struct Seen {
    state_dir: Option<StateDir>,
    journal: Option<Journal>,
}

/// Judge the host: rolled back, or never adopted, or refuse. Changes nothing. An apply looks
/// twice, once to refuse early and once under its lock, because only what is read under the
/// lock is still true when it is acted on.
fn look(
    u: &Uninstall,
    rules: &Rules,
    read_state: impl FnOnce() -> Result<Option<State>, StateError>,
) -> Result<Seen, MigrateError> {
    let state_dir = StateDir::open(&u.layout, rules)?;
    let journal = state_dir
        .as_ref()
        .map(StateDir::journal)
        .transpose()?
        .flatten();
    match &journal {
        Some(j) if j.phase == Phase::RolledBack => {
            // A rollback keeps the new tree and its state.json as the adoption left them. A
            // state that differs from what the migration adopted was written since: `setup`
            // goes ahead on a rolled-back host whose old layout is gone, and the journal
            // stays `rolled_back` while the installer runs the host.
            if let Some(state) = read_state()?
                && moved_on(j, &state)
            {
                return Err(MigrateError::Adopted(
                    "the installer's state.json has moved on since the migration that was rolled                      back (a release became current or was staged): the installer manages this                      host again, and removing it would leave kbf unmanaged"
                        .to_owned(),
                ));
            }
        }
        Some(j) => {
            return Err(MigrateError::Adopted(format!(
                "the migration is {}, so the installer manages this host: put it back with \
                 `kbf-installer migrate --rollback --apply` first",
                match j.phase {
                    Phase::Started => "begun and not finished",
                    _ => "done",
                }
            )));
        }
        None => {
            // An uninstall that finished archived the journal, and left the state of the
            // adoption it undid: that is a host with the installer removed, not one it runs.
            let archived = !asides(&u.layout, u.layout.state_dir().as_str(), JOURNAL)?.is_empty();
            if !archived && read_state()?.is_some() {
                return Err(MigrateError::Adopted(
                    "the installer's state.json is there and no migration was rolled back: the \
                     installer manages this host (a host `setup` built has no layout to roll \
                     back to), and removing it would leave kbf unmanaged"
                        .to_owned(),
                ));
            }
        }
    }
    Ok(Seen { state_dir, journal })
}

fn host_path(path: &str) -> HostPath {
    HostPath::new(path).expect("a layout path joined to a plain name")
}

fn in_state_dir(layout: &Layout, name: &str) -> HostPath {
    host_path(&format!("{}/{name}", layout.state_dir()))
}

/// Whether `text` is what [`utc_stamp`] writes: `YYYYMMDDTHHMMSSZ`.
fn is_stamp(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 16
        && b[8] == b'T'
        && b[15] == b'Z'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
}

/// The entries `<name>.rolled-back-<stamp>` of `dir`, as host paths, in name order. A name
/// that only starts like one (another suffix, a longer stamp) is not one.
fn asides(layout: &Layout, dir: &str, name: &str) -> Result<Vec<HostPath>, MigrateError> {
    let prefix = format!("{name}{ASIDE_PREFIX}");
    let real = layout.join(&host_path(dir));
    let entries = match fs::read_dir(&real) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MigrateError::io(dir)(e)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(MigrateError::io(dir))?;
        let name = entry.file_name();
        if let Some(rest) = name
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .filter(|rest| is_stamp(rest))
        {
            found.push(host_path(&format!("{dir}/{prefix}{rest}")));
        }
    }
    found.sort();
    Ok(found)
}

/// What is at `path`, without reading it; `None` when nothing is.
fn look_at<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Meta>, MigrateError> {
    exec.stat(path).map_err(MigrateError::io(path.to_string()))
}

/// The record of an uninstall under way; `None` when there is none.
fn read_progress<E: Exec>(exec: &E, layout: &Layout) -> Result<Option<Progress>, MigrateError> {
    let path = in_state_dir(layout, PROGRESS);
    match exec
        .inspect(&path)
        .map_err(MigrateError::io(path.to_string()))?
    {
        None => Ok(None),
        Some(Entry::File { bytes, .. }) => {
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| MigrateError::Corrupt {
                    file: PROGRESS,
                    why: e.to_string(),
                })
        }
        Some(_) => Err(MigrateError::Corrupt {
            file: PROGRESS,
            why: "it is not a regular file".to_owned(),
        }),
    }
}

fn write_progress<E: Exec>(
    exec: &mut E,
    layout: &Layout,
    progress: &Progress,
) -> Result<(), MigrateError> {
    let path = in_state_dir(layout, PROGRESS);
    let mut bytes = serde_json::to_vec_pretty(progress).expect("plain fields serialize");
    bytes.push(b'\n');
    exec.apply(&Step::WriteFile {
        path: path.clone(),
        perms: Perms::new(0o600),
        owner: None,
        bytes,
    })
    .map_err(MigrateError::io(path.to_string()))
}

/// What is asked of the unit manager.
#[derive(Clone, Copy)]
enum Question {
    Running,
    Enabled,
}

/// The answer of the unit manager, asked as `setup` and the rollback ask it.
fn unit_is<E: Exec>(
    exec: &mut E,
    roots: &Roots,
    unit: &Unit,
    question: Question,
) -> Result<bool, MigrateError> {
    let services = Services { exec, roots };
    match question {
        Question::Running => services.is_active(unit),
        Question::Enabled => services.is_enabled(unit),
    }
    .map_err(MigrateError::io(unit.as_str()))
}

/// The steps this host needs, in order: what is there to undo, and nothing else.
fn plan<E: Exec>(
    u: &Uninstall,
    exec: &mut E,
    seen: &Seen,
    suffix: &str,
    resuming: bool,
) -> Result<Vec<Act>, MigrateError> {
    let roots = Roots::new(u.layout.platform());
    let mut acts = Vec::new();
    match u.layout.platform() {
        Platform::MacOs => {
            let unit = match &u.installer_unit {
                Some(named) => named.clone(),
                None => Unit::new(KIT_INSTALLER_JOB).expect("a plist name"),
            };
            if unit_is(exec, &roots, &unit, Question::Running)? {
                acts.push(Act::Stop(unit.clone()));
            }
            let plist = host_path(&format!("{}/{unit}", roots.unit_dir));
            if let Some(meta) = look_at(exec, &plist)? {
                acts.push(Act::removal(plist, meta));
            }
            let enable_at = acts.len();
            let receipt = Cmd::new(host_path(PKGUTIL)).arg("--pkg-info").arg(RECEIPT);
            if exec
                .query(&receipt)
                .map_err(MigrateError::io(PKGUTIL))?
                .status
                .success()
            {
                acts.push(Act::Forget);
            }
            let [(config_dir, toml), (tree_dir, tree_name)] = aside_names(&u.layout);
            let log_dir = host_path(MAC_LOG_DIR);
            let live_toml = host_path(&format!("{config_dir}/{toml}"));
            let live_tree = host_path(&format!("{tree_dir}/{tree_name}"));
            // The log directory, the live files, then what a rollback moved aside.
            let mut removals = vec![log_dir, live_toml];
            removals.extend(asides(&u.layout, &config_dir, &toml)?);
            removals.push(live_tree);
            removals.extend(asides(&u.layout, &tree_dir, &tree_name)?);
            for path in removals {
                if let Some(meta) = look_at(exec, &path)? {
                    acts.push(Act::removal(path, meta));
                }
            }
            // Clearing the override is not a question the host answers cheaply, so it is
            // part of every uninstall that has anything else to do (or is under way), and
            // of none that has not.
            if !acts.is_empty() || seen.journal.is_some() || resuming {
                acts.insert(enable_at, Act::ClearOverride(unit));
            }
        }
        Platform::Linux => {
            if let Some(unit) = &u.installer_unit {
                if unit_is(exec, &roots, unit, Question::Running)? {
                    acts.push(Act::Stop(unit.clone()));
                }
                if unit_is(exec, &roots, unit, Question::Enabled)? {
                    acts.push(Act::Disable(unit.clone()));
                }
            }
        }
    }
    if let Some(journal) = &seen.journal {
        acts.push(Act::Archive {
            from: in_state_dir(&u.layout, JOURNAL),
            to: in_state_dir(&u.layout, &format!("{JOURNAL}{suffix}")),
            journal: Box::new(journal.clone()),
        });
    }
    Ok(acts)
}

/// Carry out one step.
fn run<E: Exec>(u: &Uninstall, exec: &mut E, act: &Act) -> Result<(), MigrateError> {
    let roots = Roots::new(u.layout.platform());
    match act {
        Act::Stop(unit) => Services {
            exec,
            roots: &roots,
        }
        .stop(unit)
        .map_err(MigrateError::io(unit.as_str())),
        Act::Disable(unit) => Services {
            exec,
            roots: &roots,
        }
        .disable(unit)
        .map_err(MigrateError::io(unit.as_str())),
        Act::ClearOverride(unit) => Services {
            exec,
            roots: &roots,
        }
        .enable(unit)
        .map_err(MigrateError::io(unit.as_str())),
        Act::Forget => exec
            .apply(&Step::Run(
                Cmd::new(host_path(PKGUTIL)).arg("--forget").arg(RECEIPT),
            ))
            .map_err(MigrateError::io(PKGUTIL)),
        Act::Remove { path, tree } => {
            let step = if *tree {
                Step::RemoveTree { path: path.clone() }
            } else {
                Step::Remove { path: path.clone() }
            };
            exec.apply(&step)
                .map_err(MigrateError::io(path.to_string()))
        }
        // The copy is written whole before the original goes, so a run that dies between
        // the two leaves both and the next finishes: the journal is never the only copy
        // anywhere but under one of its names.
        Act::Archive { from, to, journal } => {
            exec.apply(&Step::WriteFile {
                path: to.clone(),
                perms: Perms::new(0o600),
                owner: None,
                bytes: journal.render(),
            })
            .map_err(MigrateError::io(to.to_string()))?;
            exec.apply(&Step::Remove { path: from.clone() })
                .map_err(MigrateError::io(from.to_string()))
        }
    }
}

/// What the report says when it is done, or would be.
fn closing(platform: Platform, mode: Mode, archived: bool, report: &mut Report) {
    match platform {
        Platform::MacOs if mode == Mode::Apply => report.warnings.push(format!(
            "uninstalled: nothing of the installer is left on this Mac but /var/db/kbf-installer \
             (its state directory{}). Install the kbf-installer .pkg again to use it.",
            if archived {
                ", with the journal archived in it"
            } else {
                ""
            }
        )),
        Platform::MacOs => {}
        Platform::Linux => report.warnings.push(
            "the installer's package files (its binary, unit file and trust files) belong to the \
             package manager and are left: remove the kbf-installer package with it"
                .to_owned(),
        ),
    }
}

/// Remove what the package left behind, or (in a dry run) say what would go.
pub fn uninstall<E: Exec>(u: &Uninstall, exec: &mut E, mode: Mode) -> Result<Report, MigrateError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    uninstall_at(u, exec, mode, now)
}

/// [`uninstall`] at `now_secs` since the epoch: the time the journal's archive is named for
/// when the rollback chose no suffix.
pub fn uninstall_at<E: Exec>(
    u: &Uninstall,
    exec: &mut E,
    mode: Mode,
    now_secs: u64,
) -> Result<Report, MigrateError> {
    if let Some(unit) = &u.installer_unit {
        let wanted = match u.layout.platform() {
            Platform::MacOs => super::units::Kind::Launchd,
            Platform::Linux => super::units::Kind::Systemd,
        };
        if unit.kind() != wanted {
            return Err(MigrateError::UnitKind {
                unit: unit.to_string(),
                want: match wanted {
                    super::units::Kind::Launchd => ".plist (launchd)",
                    super::units::Kind::Systemd => ".service (systemd)",
                },
            });
        }
    }
    let rules = Rules::release(u.owner_uid);
    // The first look takes no lock. It is all of a dry run, and an apply's chance to refuse
    // before it waits for anything.
    let first = look(u, &rules, || state::read_unlocked(&u.layout))?;
    if mode == Mode::DryRun {
        return carry_out(u, exec, mode, now_secs, &first);
    }
    if exec.euid() != u.owner_uid {
        return Err(MigrateError::NotRootToUninstall {
            have: exec.euid(),
            want: u.owner_uid,
        });
    }
    state::make_lock_dir(&u.layout)?;
    let lock = Locked::acquire_within(&u.layout, u.lock_wait)?;
    let seen = look(u, &rules, || lock.read())?;
    carry_out(u, exec, mode, now_secs, &seen)
}

fn carry_out<E: Exec>(
    u: &Uninstall,
    exec: &mut E,
    mode: Mode,
    now_secs: u64,
    seen: &Seen,
) -> Result<Report, MigrateError> {
    let platform = u.layout.platform();
    let earlier = read_progress(&*exec, &u.layout)?;
    let suffix = earlier
        .as_ref()
        .map(|p| p.suffix.clone())
        .or_else(|| seen.journal.as_ref().and_then(|j| j.aside.clone()))
        .unwrap_or_else(|| format!("{ASIDE_PREFIX}{}", utc_stamp(now_secs)));
    let acts = plan(u, exec, seen, &suffix, earlier.is_some())?;
    let mut report = Report::new(match mode {
        Mode::DryRun => Outcome::Planned,
        Mode::Apply => Outcome::Uninstalled,
    });
    if acts.is_empty() {
        report.outcome = Outcome::NothingToMigrate;
        report
            .lines
            .push("nothing to uninstall: the package left nothing here".to_owned());
        // A record left by a run that finished everything but its removal goes now.
        if mode == Mode::Apply && earlier.is_some() {
            end_progress(u, exec)?;
        }
        return Ok(report);
    }
    let mut done = earlier.as_ref().map(|p| p.done.clone()).unwrap_or_default();
    if !done.is_empty() {
        report.lines.push(format!(
            "resuming an uninstall begun earlier: {} step(s) are done",
            done.len()
        ));
    }
    // The record is there before the first step, so a run that dies in it has a suffix to
    // finish with; a host with no state directory has nowhere to keep one, and its steps
    // are each planned only while their object is there.
    let keep = mode == Mode::Apply && seen.state_dir.is_some();
    let mut record = Progress {
        schema: 1,
        suffix,
        done: Vec::new(),
    };
    if keep && earlier.is_none() {
        write_progress(exec, &u.layout, &record)?;
    }
    for act in &acts {
        let id = act.id();
        if done.contains(&id) {
            continue;
        }
        report.line(mode, act.describe(platform));
        if mode == Mode::DryRun {
            continue;
        }
        // The record goes just before the journal is archived, which is last, so that a run
        // again meets the same journal the first one met.
        if matches!(act, Act::Archive { .. }) {
            end_progress(u, exec)?;
        }
        run(u, exec, act)?;
        if keep && !matches!(act, Act::Archive { .. }) {
            done.push(id);
            record.done.clone_from(&done);
            write_progress(exec, &u.layout, &record)?;
        }
    }
    if mode == Mode::Apply && seen.journal.is_none() {
        end_progress(u, exec)?;
    }
    closing(platform, mode, seen.journal.is_some(), &mut report);
    Ok(report)
}

/// Remove `uninstall.json` if it is there.
fn end_progress<E: Exec>(u: &Uninstall, exec: &mut E) -> Result<(), MigrateError> {
    let path = in_state_dir(&u.layout, PROGRESS);
    if look_at(&*exec, &path)?.is_some() {
        exec.apply(&Step::Remove { path: path.clone() })
            .map_err(MigrateError::io(path.to_string()))?;
    }
    Ok(())
}
