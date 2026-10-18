//! The migration itself (steps in the module docs).

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use kbf_host::{Exec, Mode};

use super::error::MigrateError;
use super::floor::{self, Seeded};
use super::newtree::{NewTree, StateDir};
use super::old::{self, Old, OldTree};
use super::probe::probe;
use super::schema::{Adopted, Journal, NewState, NewStatus, Phase, ProbeFacts};
use super::services::Services;
use super::tree::{self, Rules};
use super::units::{self, Unit, UnitOutcome};
use super::{Hooks, Migration, Outcome, Report};
use crate::CommitId;
use crate::binary::{Arch, Version};
use crate::layout::Platform;
use crate::state::Locked;

/// The rules every file read and written is judged by.
pub fn rules(m: &Migration) -> Rules {
    Rules::release(m.owner_uid)
}

/// A release of the old layout, named, probed and open.
struct Found {
    role: &'static str,
    name: CommitId,
    version: Version,
    dir: OwnedFd,
}

/// The note left in the old state directory, which the old tools do not read.
const MARKER: &str = "MIGRATED";
const MARKER_TEXT: &str = "This host is managed by kbf-installer. This directory and /usr/local/kbf \
are kept as the rollback path (`kbf-installer migrate --rollback`); nothing here is read any more.\n";

/// Migrate the host, or (in a dry run) say what would be done.
pub fn migrate<E: Exec>(
    m: &Migration,
    exec: &mut E,
    hooks: &Hooks<'_>,
    mode: Mode,
) -> Result<Report, MigrateError> {
    let rules = rules(m);
    let existing = StateDir::open(&m.layout, &m.roots, &rules)?;
    let journal = existing
        .as_ref()
        .map(StateDir::journal)
        .transpose()?
        .flatten();
    if journal.as_ref().is_some_and(|j| j.phase == Phase::Done) {
        let mut report = Report::new(Outcome::AlreadyMigrated);
        report
            .lines
            .push("already migrated: nothing to do".to_owned());
        return Ok(report);
    }
    let Some(old) = old::read(&m.layout, &m.roots, &rules)? else {
        if journal.is_some() {
            return Err(MigrateError::Disagree(
                "a migration was begun, and the old state it read is gone".to_owned(),
            ));
        }
        let mut report = Report::new(Outcome::NothingToMigrate);
        report
            .lines
            .push("no old layout here: nothing to migrate".to_owned());
        return Ok(report);
    };
    let earlier_state = existing
        .as_ref()
        .map(StateDir::state)
        .transpose()?
        .flatten();
    if journal.is_none() && earlier_state.is_some() {
        return Err(MigrateError::Conflict(
            "the installer's state.json exists beside the old state and no migration ran: \
             one of them is stale, and the migration will not choose"
                .to_owned(),
        ));
    }
    if let Some(pending) = &old.state.pending_confirm {
        return Err(MigrateError::Pending {
            commit: pending.commit.to_string(),
        });
    }
    let old_tree = OldTree::open(&m.layout, &m.roots)?;
    check_links(&old, &old_tree)?;
    let seeded = floor::seed(&old.memory, &m.seed, hooks.frontend)?;
    let found = find(m, hooks, &rules, &old_tree, &old)?;

    let mut report = Report::new(match mode {
        Mode::DryRun => Outcome::Planned,
        Mode::Apply => Outcome::Migrated,
    });
    report.warnings.extend(seeded.warnings.iter().cloned());
    report.min_generation = seeded.min_generation;
    if let Some(generation) = seeded.min_generation {
        report.line(
            mode,
            format!("min_generation = {generation} is the floor setup writes into installer.toml"),
        );
    }
    if let Some(staged) = &old.state.staged {
        report.warnings.push(format!(
            "release {staged} was staged and is not carried over: stage it again"
        ));
    }
    for f in &found {
        report.line(
            mode,
            format!(
                "release {} ({}) is the build {}",
                f.name, f.role, f.version.commit
            ),
        );
    }
    if mode == Mode::DryRun {
        plan(m, exec, &mut report)?;
        return Ok(report);
    }
    if exec.euid() != m.owner_uid {
        return Err(MigrateError::NotRoot {
            have: exec.euid(),
            want: m.owner_uid,
        });
    }
    let _old_lock = Locked::acquire(&m.layout)?;
    let tree = NewTree::create(&m.layout, &m.roots, &rules)?;
    tree.sweep_work()?;
    let mut journal = journal.unwrap_or_else(|| Journal::new(Vec::new(), None));
    journal.phase = Phase::Started;
    journal.units = m.units.clone();
    journal.helper_unit = m.helper_unit.clone();
    tree.state.put_journal(&journal)?;

    journal.adopted.clear();
    for f in &found {
        let adopted = adopt(&tree, hooks, &rules, f)?;
        match &adopted.unverified {
            None => report.line(mode, format!("release {} installed and verified", f.name)),
            Some(why) => {
                report.line(mode, format!("release {} copied UNVERIFIED: {why}", f.name));
                report.unverified.push(f.name.clone());
            }
        }
        journal.adopted.push(adopted);
    }
    if !report.unverified.is_empty() {
        report.warnings.push(format!(
            "UNVERIFIED releases adopted ({}): they carry no VERIFIED marker, a rollback to them is refused, \
             and status.json says so until a verified copy replaces them",
            report.unverified.iter().map(|c| &c.as_str()[..12]).collect::<Vec<_>>().join(", ")
        ));
    }

    let mut svc = Services {
        exec,
        roots: &m.roots,
    };
    let managed: Vec<&Unit> = m.helper_unit.iter().chain(&m.units).collect();
    for unit in managed {
        if !journal.stopped.contains(unit)
            && svc
                .is_active(unit)
                .map_err(MigrateError::io(unit.as_str()))?
        {
            journal.stopped.push(unit.clone());
        }
    }
    tree.state.put_journal(&journal)?;
    for unit in journal.stopped.clone() {
        if svc
            .is_active(&unit)
            .map_err(MigrateError::io(unit.as_str()))?
        {
            svc.stop(&unit).map_err(MigrateError::io(unit.as_str()))?;
            report.line(mode, format!("stopped {unit}"));
        }
    }
    if let Some(helper) = &m.helper_unit {
        svc.disable(helper)
            .map_err(MigrateError::io(helper.as_str()))?;
        report.line(
            mode,
            format!("disabled {helper}: the installer manages this host now"),
        );
    }

    let (current, previous) = (old.state.current.clone(), old.state.previous.clone());
    tree.set_link("current", current.as_ref())?;
    tree.set_link("previous", previous.as_ref())?;
    report.line(
        mode,
        "releases/current and releases/previous point at the adopted releases",
    );
    let state = write_state(&tree, current, previous, &seeded)?;
    let probe_facts = found
        .iter()
        .find(|f| f.role == "current")
        .map(|f| facts(m, f));
    tree.state.put_status(&NewStatus::migrated(
        &state,
        probe_facts,
        report.unverified.clone(),
    ))?;
    report.line(mode, "state.json (schema 2) and status.json written");

    let backup = tree.state.unit_backup()?;
    let units_dir = tree::open_root(&m.layout.join(&m.roots.unit_dir))
        .map_err(MigrateError::io(m.roots.unit_dir.to_string()))?;
    for unit in &m.units {
        let outcome = units::rewrite(&rules, &units_dir, &backup, unit, &m.roots.new_bin())?;
        report.line(mode, format!("{unit}: {}", describe(outcome)));
    }
    svc.reload().map_err(MigrateError::io("daemon-reload"))?;

    for unit in journal.stopped.clone() {
        if Some(&unit) == m.helper_unit.as_ref() {
            continue;
        }
        svc.start(&unit).map_err(MigrateError::io(unit.as_str()))?;
        report.line(mode, format!("started {unit}"));
    }
    journal.phase = Phase::Done;
    tree.state.put_journal(&journal)?;
    note_old_state(m)?;
    Ok(report)
}

fn describe(outcome: UnitOutcome) -> &'static str {
    match outcome {
        UnitOutcome::Rewritten => "repointed at the new tree (the original is kept)",
        UnitOutcome::Already => "already runs from the new tree",
        UnitOutcome::NotOurs => "does not run kbf from the old link: left alone",
        UnitOutcome::Missing => "no such unit file: skipped",
    }
}

/// The links of the old tree must be what the old state says they are.
fn check_links(old: &Old, tree: &OldTree) -> Result<(), MigrateError> {
    for (link, want) in [
        ("current", &old.state.current),
        ("previous", &old.state.previous),
    ] {
        let have = tree.linked(link)?;
        if &have != want {
            return Err(MigrateError::Disagree(format!(
                "state.json says {link} is {}, the link is {}",
                want.as_ref().map_or("none".to_owned(), ToString::to_string),
                have.map_or("missing".to_owned(), |c| c.to_string()),
            )));
        }
    }
    Ok(())
}

/// Open, judge and probe the releases to adopt.
fn find(
    m: &Migration,
    hooks: &Hooks<'_>,
    rules: &Rules,
    tree: &OldTree,
    old: &Old,
) -> Result<Vec<Found>, MigrateError> {
    let mut found = Vec::new();
    for (role, name) in [
        ("current", &old.state.current),
        ("previous", &old.state.previous),
    ] {
        let Some(name) = name else { continue };
        let dir = tree.release(name)?;
        tree::digest(rules, &dir)?;
        let program: PathBuf = m
            .layout
            .join(&m.roots.old_tree)
            .join("releases")
            .join(name.as_str())
            .join(m.roots.binary());
        let version = probe(hooks.runner, program, m.run_as, name)?;
        found.push(Found {
            role,
            name: name.clone(),
            version,
            dir,
        });
    }
    Ok(found)
}

/// What a dry run says about the services.
fn plan<E: Exec>(m: &Migration, exec: &mut E, report: &mut Report) -> Result<(), MigrateError> {
    let svc = Services {
        exec,
        roots: &m.roots,
    };
    for unit in m.helper_unit.iter().chain(&m.units) {
        let active = svc
            .is_active(unit)
            .map_err(MigrateError::io(unit.as_str()))?;
        let what = if active {
            "stop it, then start it again"
        } else {
            "it is not running: leave it stopped"
        };
        report.line(Mode::DryRun, format!("{unit}: {what}"));
    }
    report.line(
        Mode::DryRun,
        "copy or verify each release, repoint the links, rewrite the units",
    );
    Ok(())
}

/// Adopt one release: verified if the pipeline can, else copied and flagged.
fn adopt(
    tree: &NewTree,
    hooks: &Hooks<'_>,
    rules: &Rules,
    found: &Found,
) -> Result<Adopted, MigrateError> {
    let adopted = |unverified| Adopted {
        role: found.role.to_owned(),
        commit: found.name.clone(),
        unverified,
    };
    if tree.verified(&found.name)? {
        return Ok(adopted(None));
    }
    let why = match hooks.verifier.install_verified(&found.name, &found.version) {
        Ok(()) if tree.verified(&found.name)? => return Ok(adopted(None)),
        Ok(()) => "the verify pipeline wrote no VERIFIED marker".to_owned(),
        Err(why) => why,
    };
    let source = tree::digest(rules, &found.dir)?;
    match tree.digest(&found.name)? {
        Some(have) if have == source => {}
        Some(_) => {
            tree.remove_release(&found.name)?;
            tree.copy_in(&found.name, &found.dir)?;
        }
        None => tree.copy_in(&found.name, &found.dir)?,
    }
    Ok(adopted(Some(why)))
}

/// Write `state.json`: schema 2, the adopted releases, the seeded floor, raised to what an
/// interrupted earlier run already wrote.
fn write_state(
    tree: &NewTree,
    current: Option<CommitId>,
    previous: Option<CommitId>,
    seeded: &Seeded,
) -> Result<NewState, MigrateError> {
    let floor = match tree.state.state()? {
        Some(earlier) => floor::raise(seeded.floor, earlier.floor),
        None => seeded.floor,
    };
    let state = NewState::migrated(current, previous, floor);
    tree.state.put_state(&state)?;
    Ok(state)
}

fn facts(m: &Migration, found: &Found) -> ProbeFacts {
    let os = match m.roots.platform {
        Platform::Linux => "linux",
        Platform::MacOs => "darwin",
    };
    ProbeFacts {
        commit: found.name.clone(),
        platform: format!("{os}-{}", Arch::current()),
        protocol: found.version.protocol,
        min_protocol: found.version.min_protocol,
        state_schema: found.version.state_schema,
        profile: found.version.profile.clone(),
    }
}

/// Leave a note in the old state directory (or take it away, for a rollback).
pub(super) fn note_old_state(m: &Migration) -> Result<(), MigrateError> {
    let dir = tree::open_root(&m.layout.join(&m.roots.old_state))
        .map_err(MigrateError::io(m.roots.old_state.to_string()))?;
    Ok(tree::replace_file(
        &dir,
        MARKER,
        0o644,
        MARKER_TEXT.as_bytes(),
    )?)
}

pub(super) fn remove_note(m: &Migration) -> Result<(), MigrateError> {
    let dir = tree::open_root(&m.layout.join(&m.roots.old_state))
        .map_err(MigrateError::io(m.roots.old_state.to_string()))?;
    let gone = rustix::fs::unlinkat(&dir, MARKER, rustix::fs::AtFlags::empty());
    tree::except(gone, rustix::io::Errno::NOENT).map_err(MigrateError::io(MARKER))?;
    Ok(())
}
