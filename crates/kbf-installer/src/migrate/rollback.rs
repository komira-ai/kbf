//! Undoing a migration: the old layout was never touched, so putting a host back is
//! restoring the units' originals and starting what was running. It is refused once the
//! installer has moved the host on (a release activated, staged or confirmed since), since
//! the old tree no longer has what runs.

use kbf_host::{Exec, Mode};

use super::apply::{remove_note, rules};
use super::error::MigrateError;
use super::newtree::StateDir;
use super::schema::Phase;
use super::services::Services;
use super::tree;
use super::units;
use super::{Migration, Outcome, Report};

/// Put the host back on the old layout, or (in a dry run) say what would be done.
pub fn rollback<E: Exec>(m: &Migration, exec: &mut E, mode: Mode) -> Result<Report, MigrateError> {
    let rules = rules(m);
    let Some(state_dir) = StateDir::open(&m.layout, &m.roots, &rules)? else {
        return Err(MigrateError::NoJournal);
    };
    let Some(mut journal) = state_dir.journal()? else {
        return Err(MigrateError::NoJournal);
    };
    if journal.phase == Phase::RolledBack {
        let mut report = Report::new(Outcome::NothingToMigrate);
        report
            .lines
            .push("already rolled back: nothing to do".to_owned());
        return Ok(report);
    }
    if let Some(state) = state_dir.state()? {
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
    let mut report = Report::new(match mode {
        Mode::DryRun => Outcome::Planned,
        Mode::Apply => Outcome::RolledBack,
    });
    report.line(
        mode,
        "restore the units' originals, enable the old helper, start what ran before",
    );
    if mode == Mode::DryRun {
        return Ok(report);
    }
    if exec.euid() != m.owner_uid {
        return Err(MigrateError::NotRoot {
            have: exec.euid(),
            want: m.owner_uid,
        });
    }
    let mut svc = Services {
        exec,
        roots: &m.roots,
    };
    if let Some(installer) = &m.installer_unit {
        if svc
            .is_active(installer)
            .map_err(MigrateError::io(installer.as_str()))?
        {
            svc.stop(installer)
                .map_err(MigrateError::io(installer.as_str()))?;
            svc.disable(installer)
                .map_err(MigrateError::io(installer.as_str()))?;
            report.line(mode, format!("stopped and disabled {installer}"));
        }
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
    remove_note(m)?;
    report.warnings.push(
        "rolled back: the old layout runs again. The new tree is kept; `kbf-installer migrate --apply` \
         moves the host over again."
            .to_owned(),
    );
    Ok(report)
}
