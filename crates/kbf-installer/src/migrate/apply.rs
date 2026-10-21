//! The migration itself (steps in the module docs).

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use kbf_host::{Exec, Mode};

use super::daemon_dirs;
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
use crate::layout::{self, Platform};
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
/// and memory, and the old tree.
struct Seen {
    journal: Option<Journal>,
    old: Old,
    old_tree: OldTree,
}

/// What a look at the host came to.
enum Look {
    /// There is nothing to migrate (it is done, or never was old): the report says so.
    Settled(Report),
    Old(Box<Seen>),
}

/// Look at the host: read the old state and memory and judge them and the links. Changes
/// nothing, and asks nothing of the network (the floor is seeded from what is read, on the
/// claim made once). An apply looks twice, once to plan and once under its locks, because
/// only what is read under the locks is still true when it is written.
fn look(m: &Migration, rules: &Rules) -> Result<Look, MigrateError> {
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
    if let Some(journal) = &journal {
        super::rollback::refuse_if_installer_moved_aside(&m.layout, journal)?;
    }
    let resume = journal.as_ref().is_some_and(Journal::kit_begun);
    let Some(old) = old::read(&m.layout, &m.roots, rules, resume)? else {
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
    let old_tree = judge(m, &old)?;
    Ok(Look::Old(Box::new(Seen {
        journal,
        old,
        old_tree,
    })))
}

/// Judge what was read of the old layout: no swap on trial, and the links are the ones the
/// state names. The old tree, open, when they are.
fn judge(m: &Migration, old: &Old) -> Result<OldTree, MigrateError> {
    if let Some(pending) = &old.state.pending_confirm {
        return Err(MigrateError::Pending {
            commit: pending.commit.to_string(),
        });
    }
    let old_tree = OldTree::open(&m.layout, &m.roots)?;
    check_links(old, &old_tree)?;
    Ok(old_tree)
}

/// What the plan says of the floor, the staged release and the releases found.
fn report_plan(report: &mut Report, mode: Mode, seeded: &Seeded, old: &Old, found: &[Found]) {
    let staged = old.state.staged.as_ref();
    if let Some(form) = old.kit {
        report.line(
            mode,
            format!(
                "a host the Mac kit installed ({}): the links are the state; there is no old \
                 state or helper memory to carry over",
                form.describe()
            ),
        );
    }
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

/// The old layout once the services are stopped: read and judged like the looks before it
/// (the journal is this run's own by now, and is not read), `current` and `previous` as
/// probed, and the floor raised to what the old helper remembers now (a floor only goes up).
///
/// `kit` is set for a kit host, whose journal this run has written: the plist may be one an
/// earlier run of it repointed.
fn settle(
    m: &Migration,
    rules: &Rules,
    probed: &[Option<CommitId>; 2],
    claim: &floor::Claim,
    seeded: Seeded,
    kit: bool,
) -> Result<(Old, Seeded), MigrateError> {
    let Some(old) = old::read(&m.layout, &m.roots, rules, kit)? else {
        return Err(MigrateError::Changed("the old state is gone".to_owned()));
    };
    judge(m, &old)?;
    unchanged(probed, &old)?;
    let floor = floor::raise(seeded.floor, floor::seed(&old.memory, claim).floor);
    Ok((old, Seeded { floor, ..seeded }))
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
    // (`found`, and the names it probed them under); what it read is not written from. The
    // source of the floor is asked here too, once: the frontend is a call over the network,
    // not one to make with the locks held.
    let (found, probed, claim, kit) = {
        let first = match look(m, &rules)? {
            Look::Settled(report) => return Ok(report),
            Look::Old(seen) => seen,
        };
        // Only the owner of the tree applies. A stranger is told so before anything is run
        // or asked: the probe, as a user it cannot become, would end the run with its own
        // error and hide the reason.
        if mode == Mode::Apply && exec.euid() != m.owner_uid {
            return Err(MigrateError::NotRoot {
                have: exec.euid(),
                want: m.owner_uid,
            });
        }
        let claim = floor::claim(&m.seed, hooks.frontend)?;
        let found = find(m, hooks, &rules, &first.old_tree, &first.old)?;
        if mode == Mode::DryRun {
            let mut report = Report::new(Outcome::Planned);
            report_plan(
                &mut report,
                mode,
                &floor::seed(&first.old.memory, &claim),
                &first.old,
                &found,
            );
            plan(m, exec, first.old.kit.is_some(), &mut report)?;
            return Ok(report);
        }
        // A Mac's kbf.toml is the provisioner's: refuse before anything is stopped or written.
        daemon_dirs::preflight(m, exec)?;
        let probed = [
            first.old.state.current.clone(),
            first.old.state.previous.clone(),
        ];
        (found, probed, claim, first.old.kit.is_some())
    };
    // Both locks: no verb of the old tools is mid-way, and no run of the installer changes
    // its state while this one does. Each is waited for `m.lock_wait` and no longer (busy,
    // exit 4), the old tools' first and let go with the error.
    // A kit host has none: the kit keeps no lock, and none is made for it.
    let _old_lock = (!kit)
        .then(|| OldLock::take(&m.layout, &m.roots, m.lock_wait))
        .transpose()?;
    state::make_lock_dir(&m.layout)?;
    let lock = Locked::acquire_within(&m.layout, m.lock_wait)?;
    // Everything read before the locks may be stale by now: a verb of the old tools (or the
    // old helper) between the look and the locks moves `current`, puts a swap on trial or
    // acts on an order, and another run of this verb may have finished while this one
    // waited. So the old layout is read and judged again, and every write below is made
    // from this look alone. A different `current` or `previous` is not adopted in place of
    // the releases probed: the run is refused, nothing migrated, to be run again.
    let seen = match look(m, &rules)? {
        Look::Old(seen) => seen,
        Look::Settled(report) if report.outcome == Outcome::AlreadyMigrated => return Ok(report),
        Look::Settled(_) => {
            return Err(MigrateError::Changed("the old state is gone".to_owned()));
        }
    };
    unchanged(&probed, &seen.old)?;
    if seen.old.kit.is_some() != kit {
        return Err(MigrateError::Changed(
            "the host is no longer the kind of host it was: a kit host and an old host \
             are not each other"
                .to_owned(),
        ));
    }
    let Seen { journal, old, .. } = *seen;
    let seeded = floor::seed(&old.memory, &claim);
    let mut report = Report::new(Outcome::Migrated);
    report_plan(&mut report, mode, &seeded, &old, &found);
    let tree = NewTree::create(&m.layout, &rules)?;
    tree.sweep_partials()?;
    let mut journal = journal.unwrap_or_else(|| Journal::new(Vec::new(), None));
    journal.phase = Phase::Started;
    journal.aside = None;
    journal.kit = kit;
    journal.units = m.units.clone();
    journal.helper_unit = m.helper_unit.clone();
    tree.state.put_journal(&journal)?;

    // The unit files are read and judged now, before a release is copied and long before a
    // service is stopped: a unit that is a link, that others can write or that is too big
    // would otherwise end the run half way, with the daemon stopped, the helper disabled and
    // the state written.
    let backup = tree.state.unit_backup()?;
    let units_dir = tree::open_root(&m.layout.join(&m.roots.unit_dir))
        .map_err(MigrateError::io(m.roots.unit_dir.to_string()))?;
    let new_bin = m.layout.current().to_string();
    for unit in &m.units {
        units::check(&rules, &units_dir, &backup, unit, &new_bin)?;
    }

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
    // The old helper is stopped now, so what it remembers is final. It acts on orders until
    // its unit stops, which is after the look above: the floor is raised to what it
    // remembers now. A swap it made meanwhile is not the host that was probed: the services
    // run again as they were and the run ends before the installer's state, the links or the
    // units are written (the journal and the copies made are in the new tree, and a run again
    // starts from what is there).
    let (old, seeded) = match settle(m, &rules, &probed, &claim, seeded, kit) {
        Ok(settled) => settled,
        Err(error) => {
            for unit in &journal.stopped {
                // The error to report is the change; a unit that will not start is the
                // operator's to see in the unit manager, and `migrate --rollback` starts
                // what the journal says was stopped.
                let _ = svc.start(unit);
            }
            return Err(error);
        }
    };
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

    for unit in &m.units {
        let outcome = units::rewrite(&rules, &units_dir, &backup, unit, &new_bin)?;
        report.line(mode, format!("{unit}: {}", describe(outcome)));
    }
    // Before the services start again: a node reads its keys when it starts.
    daemon_dirs::add(m, svc.exec, mode, &mut report)?;
    svc.reload().map_err(MigrateError::io("daemon-reload"))?;

    for unit in journal.stopped.clone() {
        if Some(&unit) == m.helper_unit.as_ref() {
            continue;
        }
        svc.start(&unit).map_err(MigrateError::io(unit.as_str()))?;
        report.line(mode, format!("started {unit}"));
    }
    // Last, with the old helper disabled and the new layout in place: the installer's own
    // unit, so the migrated host takes orders without a separate `setup`. Before the journal
    // says done, so a failure here is a run again, not a host that looks finished.
    if let Some(installer) = installer_job(m, kit) {
        enable_installer(&mut svc, &installer, mode, &mut report)?;
    }
    journal.phase = Phase::Done;
    tree.state.put_journal(&journal)?;
    // A kit host has no old state directory to leave a note in, and none is made.
    if !kit {
        note_old_state(m)?;
    }
    Ok(report)
}

/// The package's installer job on a Mac (`packaging/macos`). A kit host's installer is always
/// this job, named or not, so a migration enables it when `--installer-unit` names none. (The
/// same constant, with the same value, is in `rollback.rs`, which stops it; they are to be one
/// definition.)
pub(super) const KIT_INSTALLER_JOB: &str = "com.komira.kbf-installer.plist";

/// The installer's unit this migration enables: the one `--installer-unit` names, else on a kit
/// host the package's job. An old host without the flag has none (its units are the operator's).
fn installer_job(m: &Migration, kit: bool) -> Option<Unit> {
    match &m.installer_unit {
        Some(named) => Some(named.clone()),
        None => kit.then(|| Unit::new(KIT_INSTALLER_JOB).expect("a plist name")),
    }
}

/// Enable the installer's unit and start it unless it is enabled already: the same question
/// and the same command `setup` uses, and nothing run when the answer is yes. The frontend
/// and the proxy are not touched.
fn enable_installer<E: Exec>(
    svc: &mut Services<'_, E>,
    installer: &Unit,
    mode: Mode,
    report: &mut Report,
) -> Result<(), MigrateError> {
    let refused = |e: std::io::Error| MigrateError::InstallerUnit {
        unit: installer.to_string(),
        why: e.to_string(),
    };
    if svc.is_enabled(installer).map_err(refused)? {
        report.line(mode, format!("{installer}: already enabled"));
        return Ok(());
    }
    svc.enable_now(installer).map_err(refused)?;
    report.line(
        mode,
        format!("enabled and started {installer}: the host takes orders"),
    );
    Ok(())
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
        let holds_daemon = m.roots.platform == Platform::MacOs
            && tree::absent(rustix::fs::statat(
                &dir,
                layout::RELEASE_DAEMON,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            ))
            .map_err(MigrateError::io(layout::RELEASE_DAEMON))?
            .is_some();
        let program: PathBuf = m
            .layout
            .join(&m.roots.old_tree)
            .join("releases")
            .join(name.as_str())
            .join(m.roots.binary(old.kit.is_some(), holds_daemon));
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
fn plan<E: Exec>(
    m: &Migration,
    exec: &mut E,
    kit: bool,
    report: &mut Report,
) -> Result<(), MigrateError> {
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
    if let Some(installer) = &installer_job(m, kit) {
        let enabled = svc
            .is_enabled(installer)
            .map_err(MigrateError::io(installer.as_str()))?;
        let what = if enabled {
            "it is enabled already: leave it"
        } else {
            "enable it and start it, last"
        };
        report.line(Mode::DryRun, format!("{installer}: {what}"));
    }
    report.line(
        Mode::DryRun,
        "copy or verify each release, repoint the links, rewrite the units",
    );
    daemon_dirs::add(m, svc.exec, Mode::DryRun, report)?;
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
