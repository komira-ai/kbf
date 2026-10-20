//! Undoing a migration: the old layout was never touched, so putting a host back is
//! restoring the units' originals and starting what was running. It is refused once the
//! installer has moved the host on (a release activated, staged or confirmed since), since
//! the old tree no longer has what runs. An apply decides, and acts, under the same two locks
//! as the migration (the old tools', then the installer's): a live installer can activate a
//! release up to the moment it holds the lock, so only what is read under it is true.

use kbf_host::{Exec, Mode};

use super::apply::{remove_note, rules};
use super::error::MigrateError;
use super::journal::{Journal, Phase};
use super::newtree::StateDir;
use super::old::OldLock;
use super::services::Services;
use super::tree::{self, Rules};
use super::units;
use super::{Migration, Outcome, Report};
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

/// What the rollback says it does.
fn plan(mode: Mode, unknown_state: Option<String>) -> Report {
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
    report
}

/// Put the host back on the old layout, or (in a dry run) say what would be done.
pub fn rollback<E: Exec>(m: &Migration, exec: &mut E, mode: Mode) -> Result<Report, MigrateError> {
    let rules = rules(m);
    // The first look takes no lock. It is all of a dry run, and an apply's chance to refuse
    // before it waits for anything.
    let first = match look(m, &rules, || state::read_unlocked(&m.layout))? {
        Look::Settled(report) => return Ok(report),
        Look::Go(go) => go,
    };
    if mode == Mode::DryRun {
        return Ok(plan(mode, first.unknown_state));
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
    let mut report = plan(mode, unknown_state);
    let mut svc = Services {
        exec,
        roots: &m.roots,
    };
    if let Some(installer) = &m.installer_unit
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
    journal.phase = Phase::RolledBack;
    journal.stopped.clear();
    state_dir.put_journal(&journal)?;
    if !journal.kit {
        remove_note(m)?;
    }
    report.warnings.push(
        "rolled back: the old layout runs again. The new tree is kept; `kbf-installer migrate --apply` \
         moves the host over again."
            .to_owned(),
    );
    Ok(report)
}
