//! Undoing a migration: the old layout was never touched, so putting a host back is
//! restoring the units' originals and starting what was running. It is refused once the
//! installer has moved the host on (a release activated, staged or confirmed since), since
//! the old tree no longer has what runs. An apply decides, and acts, under the same two locks
//! as the migration (the old tools', then the installer's): a live installer can activate a
//! release up to the moment it holds the lock, so only what is read under it is true.

use std::os::fd::OwnedFd;
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_host::{Exec, Mode};

use super::apply::{remove_note, rules};
use super::error::MigrateError;
use super::journal::{Journal, Phase};
use super::newtree::StateDir;
use super::old::OldLock;
use super::services::Services;
use super::tree::{self, Rules};
use super::units;
use super::units::Unit;
use super::{Migration, Outcome, Report};
use crate::backup::utc_stamp;
use crate::layout::Layout;
use crate::state::{self, Locked, State, StateError};

/// What a look at the journal and the installer's state came to.
enum Look {
    /// There is nothing to do: the host was rolled back already.
    Settled(Report),
    Go(Box<Go>),
}

/// A rollback that may go ahead.
struct Go {
    state_dir: StateDir,
    journal: Journal,
    /// Why the installer's state could not be read, when it could not: whether it moved the
    /// host on is then unknown, and the report says so.
    unknown_state: Option<String>,
}

/// Look at the journal and at the installer's state (as `read_state` reads it) and judge
/// them: the journal must be there, the host not already rolled back, and, once the
/// migration is done, the installer must not have moved on. Changes nothing. An apply looks
/// twice, once to refuse early and once under its locks, because only what is read under
/// the locks is still true when it is acted on.
fn look(
    m: &Migration,
    rules: &Rules,
    read_state: impl FnOnce() -> Result<Option<State>, StateError>,
) -> Result<Look, MigrateError> {
    let Some(state_dir) = StateDir::open(&m.layout, rules)? else {
        return Err(MigrateError::NoJournal);
    };
    let Some(journal) = state_dir.journal()? else {
        return Err(MigrateError::NoJournal);
    };
    if journal.phase == Phase::RolledBack {
        let mut report = Report::new(Outcome::NothingToMigrate);
        report
            .lines
            .push("already rolled back: nothing to do".to_owned());
        return Ok(Look::Settled(report));
    }
    // A state nobody can read cannot say the installer moved on, and a rollback is how a
    // host gets out of that: it goes ahead and says so.
    let mut unknown_state = None;
    let state = match read_state() {
        Err(e) if e.needs_operator() => {
            unknown_state = Some(e.to_string());
            None
        }
        read => read?,
    };
    if let Some(state) = state {
        let adopted = |role: &str| {
            journal
                .adopted
                .iter()
                .find(|a| a.role == role)
                .map(|a| a.commit.clone())
        };
        let moved = state.current != adopted("current")
            || state.previous != adopted("previous")
            || state.staged.is_some()
            || state.pending_confirm.is_some();
        if journal.phase == Phase::Done && moved {
            return Err(MigrateError::MovedOn(
                "the installer has activated or staged a release since the migration; \
                 go back with `kbf-installer rollback` instead"
                    .to_owned(),
            ));
        }
    }
    Ok(Look::Go(Box::new(Go {
        state_dir,
        journal,
        unknown_state,
    })))
}

/// A kit host that was rolled back has no installer tree: it was moved aside, and the
/// kit manages the Mac. Whatever the installer is asked on such a host (`migrate`, `setup`)
/// is refused with the way back, which is the package. A tree that is there again (the
/// package was reinstalled) is not refused.
pub(super) fn refuse_if_installer_moved_aside(
    layout: &Layout,
    journal: &Journal,
) -> Result<(), MigrateError> {
    if journal.kit && journal.phase == Phase::RolledBack {
        let root = layout.join(&layout.install_root());
        if tree::absent(tree::open_root(&root))
            .map_err(MigrateError::io(root.display().to_string()))?
            .is_none()
        {
            return Err(MigrateError::KitRolledBack);
        }
    }
    Ok(())
}

/// The two things a kit host's rollback moves aside, each as the directory it is in and its
/// name: `installer.toml` (the installer job's `KeepAlive` is gated on it, so it goes first)
/// and the installer's tree.
fn aside_names(layout: &Layout) -> [(String, String); 2] {
    let toml = layout.installer_toml();
    let (config_dir, toml_name) = toml.as_str().rsplit_once('/').expect("an absolute path");
    let tree = layout.install_root();
    let (tree_dir, tree_name) = tree.as_str().rsplit_once('/').expect("an absolute path");
    [
        (config_dir.to_owned(), toml_name.to_owned()),
        (tree_dir.to_owned(), tree_name.to_owned()),
    ]
}

/// Move `installer.toml` and the installer's tree aside under `suffix`. Both targets are
/// looked at before either move, so a name taken already refuses the lot with nothing moved;
/// what is not there to move is taken to have been moved by an earlier run of this rollback.
fn move_aside(layout: &Layout, suffix: &str) -> Result<Vec<String>, MigrateError> {
    let mut plan = Vec::new();
    for (dir, name) in aside_names(layout) {
        let fd: OwnedFd = tree::open_root(&layout.join(&host_dir(&dir)))
            .map_err(MigrateError::io(dir.clone()))?;
        let target = format!("{name}{suffix}");
        let path = format!("{dir}/{name}");
        if tree::holds(&fd, &name).map_err(MigrateError::io(path.clone()))? {
            if tree::holds(&fd, &target).map_err(MigrateError::io(path.clone()))? {
                return Err(MigrateError::Conflict(format!(
                    "{dir}/{target} is there already: the rollback will not replace it; move it \
                     away and run the rollback again"
                )));
            }
            plan.push((fd, path, name, target));
        }
    }
    let mut moved = Vec::new();
    for (fd, path, name, target) in plan {
        tree::rename_within(&fd, &name, &target).map_err(MigrateError::io(path.clone()))?;
        moved.push(format!("{path} is now {path}{suffix}"));
    }
    Ok(moved)
}

fn host_dir(dir: &str) -> kbf_host::path::HostPath {
    kbf_host::path::HostPath::new(dir).expect("a directory of the layout")
}

/// What the rollback says it does.
fn plan(mode: Mode, kit: bool, unknown_state: Option<String>) -> Report {
    let mut report = Report::new(match mode {
        Mode::DryRun => Outcome::Planned,
        Mode::Apply => Outcome::RolledBack,
    });
    if let Some(why) = unknown_state {
        report.warnings.push(format!(
            "the installer's state is unusable, so whether it moved the host on could not be \
             checked: {why}"
        ));
    }
    report.line(
        mode,
        "restore the units' originals, enable the old helper, start what ran before",
    );
    if kit {
        report.line(
            mode,
            "then boot out and disable the installer's own job and move installer.toml and the \
             installer's tree aside, so the kit alone manages this Mac",
        );
    }
    report
}

/// Put the host back on the old layout, or (in a dry run) say what would be done.
/// The package's installer job on a Mac (`packaging/macos`), stopped by a kit-host rollback
/// when `--installer-unit` names none.
const KIT_INSTALLER_JOB: &str = "com.komira.kbf-installer.plist";

pub fn rollback<E: Exec>(m: &Migration, exec: &mut E, mode: Mode) -> Result<Report, MigrateError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rollback_at(m, exec, mode, now)
}

/// [`rollback`] at `now_secs` since the epoch: the time a kit host's moved-aside files are
/// named for.
pub fn rollback_at<E: Exec>(
    m: &Migration,
    exec: &mut E,
    mode: Mode,
    now_secs: u64,
) -> Result<Report, MigrateError> {
    let rules = rules(m);
    // The first look takes no lock. It is all of a dry run, and an apply's chance to refuse
    // before it waits for anything.
    let first = match look(m, &rules, || state::read_unlocked(&m.layout))? {
        Look::Settled(report) => return Ok(report),
        Look::Go(go) => go,
    };
    if mode == Mode::DryRun {
        return Ok(plan(mode, first.journal.kit, first.unknown_state));
    }
    if exec.euid() != m.owner_uid {
        return Err(MigrateError::NotRoot {
            have: exec.euid(),
            want: m.owner_uid,
        });
    }
    // Both locks, in the order the migration takes them, each waited for `m.lock_wait` and
    // no longer (busy, exit 4), and held to the end: the installer is stopped below with
    // its lock held, so it writes no state while the old layout comes back, and no verb of
    // the old tools runs while its units start.
    // A kit host's migration took no old lock, so a rollback takes none.
    let _old_lock = (!first.journal.kit)
        .then(|| OldLock::take(&m.layout, &m.roots, m.lock_wait))
        .transpose()?;
    state::make_lock_dir(&m.layout)?;
    let lock = Locked::acquire_within(&m.layout, m.lock_wait)?;
    // What was read before the locks may be stale: the installer may have activated a
    // release while this waited, or another rollback may have finished. The decision is
    // made again, and everything below is done from this look alone.
    let Go {
        state_dir,
        mut journal,
        unknown_state,
    } = match look(m, &rules, || lock.read())? {
        Look::Settled(report) => return Ok(report),
        Look::Go(go) => *go,
    };
    let mut report = plan(mode, journal.kit, unknown_state);
    let mut svc = Services {
        exec,
        roots: &m.roots,
    };
    // A kit host's installer job is booted out after the kit's own is back (below).
    if let Some(installer) = m.installer_unit.as_ref().filter(|_| !journal.kit)
        && svc
            .is_active(installer)
            .map_err(MigrateError::io(installer.as_str()))?
    {
        svc.stop(installer)
            .map_err(MigrateError::io(installer.as_str()))?;
        svc.disable(installer)
            .map_err(MigrateError::io(installer.as_str()))?;
        report.line(mode, format!("stopped and disabled {installer}"));
    }
    for unit in &journal.units {
        if svc
            .is_active(unit)
            .map_err(MigrateError::io(unit.as_str()))?
        {
            if !journal.stopped.contains(unit) {
                journal.stopped.push(unit.clone());
                state_dir.put_journal(&journal)?;
            }
            svc.stop(unit).map_err(MigrateError::io(unit.as_str()))?;
            report.line(mode, format!("stopped {unit}"));
        }
    }
    let backup = state_dir.unit_backup()?;
    let units_dir = tree::open_root(&m.layout.join(&m.roots.unit_dir))
        .map_err(MigrateError::io(m.roots.unit_dir.to_string()))?;
    for unit in &journal.units {
        if units::restore(&rules, &units_dir, &backup, unit)? {
            report.line(mode, format!("{unit}: the original is back"));
        } else if units::unretire(&rules, &units_dir, &backup, unit)? {
            report.line(
                mode,
                format!("{unit}: the file setup retired is back (it had no original)"),
            );
        }
    }
    svc.reload().map_err(MigrateError::io("daemon-reload"))?;
    if let Some(helper) = &journal.helper_unit {
        svc.enable(helper)
            .map_err(MigrateError::io(helper.as_str()))?;
        report.line(mode, format!("enabled {helper}"));
    }
    for unit in journal.stopped.clone() {
        svc.start(&unit).map_err(MigrateError::io(unit.as_str()))?;
        report.line(mode, format!("started {unit}"));
    }
    if journal.kit {
        // The kit refuses to manage a Mac while the installer's tree is there, so from here
        // nothing of the installer may be able to act. The job goes first, then the file its
        // `KeepAlive` waits on, then the tree it would run; a run that died between them
        // finds the same suffix in the journal and finishes with it.
        // A kit host's installer job is always the package's, named or not: its `KeepAlive`
        // waits on the `installer.toml` moved aside below, so it is stopped first in every
        // case, and the order of the two moves cannot let it start.
        let installer = match &m.installer_unit {
            Some(named) => named.clone(),
            None => Unit::new(KIT_INSTALLER_JOB).expect("a plist name"),
        };
        if svc
            .is_active(&installer)
            .map_err(MigrateError::io(installer.as_str()))?
        {
            svc.stop(&installer)
                .map_err(MigrateError::io(installer.as_str()))?;
            report.line(mode, format!("booted out {installer}"));
        }
        svc.disable(&installer)
            .map_err(MigrateError::io(installer.as_str()))?;
        report.line(mode, format!("disabled {installer}"));
        let suffix = match journal.aside.clone() {
            Some(suffix) => suffix,
            None => {
                let suffix = format!(".rolled-back-{}", utc_stamp(now_secs));
                journal.aside = Some(suffix.clone());
                state_dir.put_journal(&journal)?;
                suffix
            }
        };
        for line in move_aside(&m.layout, &suffix)? {
            report.line(mode, line);
        }
    }
    journal.phase = Phase::RolledBack;
    journal.stopped.clear();
    state_dir.put_journal(&journal)?;
    if journal.kit {
        report.warnings.push(
            "rolled back: the Mac kit runs again and nothing of the installer can act on this \
             Mac. Its job is booted out and disabled, and installer.toml and the installer's \
             tree were moved aside (the `.rolled-back-...` names above); /var/db/kbf-installer, \
             the journal, is kept. To hand the host to the installer again, reinstall the \
             kbf-installer .pkg and run `kbf-installer migrate --apply`."
                .to_owned(),
        );
    } else {
        remove_note(m)?;
        report.warnings.push(
            "rolled back: the old layout runs again. The new tree is kept; `kbf-installer migrate --apply` \
             moves the host over again."
                .to_owned(),
        );
    }
    Ok(report)
}
