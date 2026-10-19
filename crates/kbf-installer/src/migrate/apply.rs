//! The migration itself (steps in the module docs).

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use kbf_host::{Exec, Mode};

use super::error::MigrateError;
use super::floor::{self, Seeded};
use super::journal::{Adopted, Journal, Phase};
use super::newtree::{NewTree, StateDir};
use super::old::{self, Old, OldLock, OldTree};
use super::probe::probe;
use super::services::Services;
use super::tree::{self, Rules};
use super::units::{self, Unit, UnitOutcome};
use super::{Hooks, Migration, Outcome, Report};
use crate::CommitId;
use crate::binary::Version;
use crate::helper::memory::{Memory, StagedFacts};
use crate::helper::status;
use crate::layout;
use crate::state::{self, Locked, State};

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

/// What a look at the host found: the journal of a migration begun, the old layout's state
/// and memory, the old tree, and the floor seeded from that memory.
struct Seen {
    journal: Option<Journal>,
    old: Old,
    old_tree: OldTree,
    seeded: Seeded,
}

/// What a look at the host came to.
enum Look {
    /// There is nothing to migrate (it is done, or never was old): the report says so.
    Settled(Report),
    Old(Box<Seen>),
}

/// Look at the host: read the old state and memory, judge them and the links, seed the
/// floor. Changes nothing. An apply looks twice, once to plan and once under its locks,
/// because only what is read under the locks is still true when it is written.
fn look(m: &Migration, hooks: &Hooks<'_>, rules: &Rules) -> Result<Look, MigrateError> {
    let existing = StateDir::open(&m.layout, rules)?;
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
        return Ok(Look::Settled(report));
    }
    let Some(old) = old::read(&m.layout, &m.roots, rules)? else {
        if journal.is_some() {
            return Err(MigrateError::Disagree(
                "a migration was begun, and the old state it read is gone".to_owned(),
            ));
        }
        let mut report = Report::new(Outcome::NothingToMigrate);
        report
            .lines
            .push("no old layout here: nothing to migrate".to_owned());
        return Ok(Look::Settled(report));
    };
    let earlier_state = state::read_unlocked(&m.layout)?;
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
    Ok(Look::Old(Box::new(Seen {
        journal,
        old,
        old_tree,
        seeded,
    })))
}

/// What the plan says of the floor, the staged release and the releases found.
fn report_plan(
    report: &mut Report,
    mode: Mode,
    seeded: &Seeded,
    staged: Option<&CommitId>,
    found: &[Found],
) {
    report.warnings.extend(seeded.warnings.iter().cloned());
    report.min_generation = seeded.min_generation;
    if let Some(generation) = seeded.min_generation {
        report.line(
            mode,
            format!("min_generation = {generation} is the floor setup writes into installer.toml"),
        );
    }
    if let Some(staged) = staged {
        report.warnings.push(format!(
            "release {staged} was staged and is not carried over: stage it again"
        ));
    }
    for f in found {
        report.line(
            mode,
            format!(
                "release {} ({}) is the build {}",
                f.name, f.role, f.version.commit
            ),
        );
    }
}

/// `current` and `previous` as the first look found and probed them must still be what the
/// old state names under the locks: a release probed is not one adopted in its place.
fn unchanged(probed: &[Option<CommitId>; 2], old: &Old) -> Result<(), MigrateError> {
    let shown = |c: &Option<CommitId>| c.as_ref().map_or("none".to_owned(), ToString::to_string);
    for (role, was, now) in [
        ("current", &probed[0], &old.state.current),
        ("previous", &probed[1], &old.state.previous),
    ] {
        if was != now {
            return Err(MigrateError::Changed(format!(
                "{role} was {} and is now {}",
                shown(was),
                shown(now)
            )));
        }
    }
    Ok(())
}

/// Migrate the host, or (in a dry run) say what would be done.
pub fn migrate<E: Exec>(
    m: &Migration,
    exec: &mut E,
    hooks: &Hooks<'_>,
    mode: Mode,
) -> Result<Report, MigrateError> {
    let rules = rules(m);
    // The first look takes no lock. It is all of a dry run, and an apply's chance to refuse
    // and to probe the releases before it waits for anything. Only what it probed goes on
    // (`found`, and the names it probed them under); what it read is not written from.
    let (found, probed) = {
        let first = match look(m, hooks, &rules)? {
            Look::Settled(report) => return Ok(report),
            Look::Old(seen) => seen,
        };
        let found = find(m, hooks, &rules, &first.old_tree, &first.old)?;
        if mode == Mode::DryRun {
            let mut report = Report::new(Outcome::Planned);
            report_plan(
                &mut report,
                mode,
                &first.seeded,
                first.old.state.staged.as_ref(),
                &found,
            );
            plan(m, exec, &mut report)?;
            return Ok(report);
        }
        let probed = [
            first.old.state.current.clone(),
            first.old.state.previous.clone(),
        ];
        (found, probed)
    };
    if exec.euid() != m.owner_uid {
        return Err(MigrateError::NotRoot {
            have: exec.euid(),
            want: m.owner_uid,
        });
    }
    // Both locks: no verb of the old tools is mid-way, and no run of the installer changes
    // its state while this one does.
    let _old_lock = OldLock::take(&m.layout, &m.roots)?;
    let lock = Locked::acquire(&m.layout)?;
    // Everything read before the locks may be stale by now: a verb of the old tools (or the
    // old helper) between the look and the locks moves `current`, puts a swap on trial or
    // acts on an order, and another run of this verb may have finished while this one
    // waited. So the old layout is read and judged again, and every write below is made
    // from this look alone. A different `current` or `previous` is not adopted in place of
    // the releases probed: the run is refused, nothing migrated, to be run again.
    let seen = match look(m, hooks, &rules)? {
        Look::Old(seen) => seen,
        Look::Settled(report) if report.outcome == Outcome::AlreadyMigrated => return Ok(report),
        Look::Settled(_) => {
            return Err(MigrateError::Changed("the old state is gone".to_owned()));
        }
    };
    unchanged(&probed, &seen.old)?;
    let Seen {
        journal,
        old,
        seeded,
        ..
    } = *seen;
    let mut report = Report::new(Outcome::Migrated);
    report_plan(
        &mut report,
        mode,
        &seeded,
        old.state.staged.as_ref(),
        &found,
    );
    let tree = NewTree::create(&m.layout, &rules)?;
    tree.sweep_partials()?;
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

    // The state first: a `current` link without a `state.json` beside it is a host whose
    // state is lost (`state::Files::load`), and a run that stops between the two must be
    // one a re-run can finish.
    let (current, previous) = (old.state.current.clone(), old.state.previous.clone());
    let state = write_state(&lock, current.clone(), previous.clone(), &seeded)?;
    let current_found = found.iter().find(|f| f.role == "current");
    let memory = write_memory(&tree, &old.memory, &seeded, current_found)?;
    tree.set_link(layout::CURRENT, current.as_ref())?;
    tree.set_link(layout::PREVIOUS, previous.as_ref())?;
    report.line(
        mode,
        "releases/current and releases/previous point at the adopted releases",
    );
    tree.state
        .put_status(&status::render_with(&state, &memory, &report.unverified))?;
    report.line(
        mode,
        "state.json (schema 2), helper.json (the replay floor) and status.json written",
    );

    let backup = tree.state.unit_backup()?;
    let units_dir = tree::open_root(&m.layout.join(&m.roots.unit_dir))
        .map_err(MigrateError::io(m.roots.unit_dir.to_string()))?;
    for unit in &m.units {
        let outcome = units::rewrite(
            &rules,
            &units_dir,
            &backup,
            unit,
            &m.layout.current().to_string(),
        )?;
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
    lock: &Locked,
    current: Option<CommitId>,
    previous: Option<CommitId>,
    seeded: &Seeded,
) -> Result<State, MigrateError> {
    let floor = match lock.read()? {
        Some(earlier) => floor::raise(seeded.floor, earlier.floor),
        None => seeded.floor,
    };
    let state = State {
        current,
        previous,
        last_acked_order_id: floor.order_id,
        floor,
        ..State::default()
    };
    lock.write(&state)?;
    Ok(state)
}

/// Write the installer's replay memory (`helper.json`): both kinds of order start at the
/// seeded (epoch, order id), so no order the old helper acted on or refused as old is acted
/// on again; whatever an earlier run (or the installer) already remembered stays if higher.
/// The facts of the current build are what `status.json` shows for it.
fn write_memory(
    tree: &NewTree,
    old: &Memory,
    seeded: &Seeded,
    current: Option<&Found>,
) -> Result<Memory, MigrateError> {
    let mut memory = match tree.state.memory()? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| MigrateError::Corrupt {
            file: layout::MEMORY,
            why: e.to_string(),
        })?,
        None => Memory::default(),
    };
    let floor = (seeded.floor.epoch, seeded.floor.order_id);
    (memory.acked_epoch, memory.acked_order_id) = floor::newer(
        floor::newer(
            (memory.acked_epoch, memory.acked_order_id),
            (old.acked_epoch, old.acked_order_id),
        ),
        floor,
    );
    (memory.frontend_epoch, memory.frontend_order_id) = floor::newer(
        floor::newer(
            (memory.frontend_epoch, memory.frontend_order_id),
            (old.frontend_epoch, old.frontend_order_id),
        ),
        floor,
    );
    // Facts that do not fit the status's width are not shown rather than shown wrong.
    memory.staged = current.and_then(|f| {
        let v = &f.version;
        Some(StagedFacts {
            commit: f.name.clone(),
            protocol: u32::try_from(v.protocol).ok()?,
            min_protocol: u32::try_from(v.min_protocol).ok()?,
            state_schema: u32::try_from(v.state_schema).ok()?,
        })
    });
    tree.state
        .put_memory(&serde_json::to_vec(&memory).expect("plain fields serialize"))?;
    Ok(memory)
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
