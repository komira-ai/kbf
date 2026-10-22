//! The node step's recheck of a staged run's package (`update/steps.rs`, steps 1 to 5): a host
//! whose package no longer carries the release is given back before the activate order, not
//! held after it. A fetch run reads no package. The picker's gate is in
//! `update_pick_source.rs`, the rest of steps 1 to 5 in `update_node_step.rs`.
//!
//! Each test names the defect it would catch. Each 'Mutants it catches' line names a change
//! that turns the test red.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{Alert, Command, Facts, GiveBack, HostPhase, SlotOp, Source};
use kbf_server::update_status::ReleaseSource;
use support::{
    NEW, NEXT_ORDER, activate, gave_back, host_mut, linux_fleet, member_mut, package, run,
    storage_fleet, waiting_in,
};

/// A release the run is not driving to.
const OTHER: &str = "v3";

/// A fleet whose host `s1` is in step 5: marked, writes quiet, cordoned, the gate open.
fn gated_fleet() -> Facts {
    let mut f = storage_fleet();
    member_mut(&mut f, 1).planned_down = true;
    f
}

/// Runs `phase` for `s1` of `f`, a world in which the host stays, under a staged run. While its
/// package carries the target the host is not given back; with no package, and with a package
/// of another release, it is, and `want` are the commands that do it.
fn is_given_back_when_the_package_goes(f: &Facts, phase: &HostPhase, want: &[Command]) {
    let mut f = f.clone();
    f.source = Source::Staged;
    package(&mut f, "s1", NEW, ReleaseSource::Bundled);
    let stays = run(&f, "s1", phase, 5);
    assert_ne!(stays.next, HostPhase::Pending, "{phase:?}: carried");

    host_mut(&mut f, "s1").available = None;
    let none = run(&f, "s1", phase, 5);
    assert_eq!(none.next, HostPhase::Pending, "{phase:?}: no package");
    assert_eq!(none.commands, want, "{phase:?}: no package");

    package(&mut f, "s1", OTHER, ReleaseSource::Bundled);
    let other = run(&f, "s1", phase, 5);
    assert_eq!(other.next, HostPhase::Pending, "{phase:?}: other");
    assert_eq!(other.commands, want, "{phase:?}: other");
}

/// Runs `phase` for `s1` of `f` under a fetch run: a package of another release changes
/// nothing, and the host is not given back.
fn ignores_the_package_on_a_fetch_run(f: &Facts, phase: &HostPhase) {
    let mut f = f.clone();
    let bare = run(&f, "s1", phase, 5);
    assert_ne!(bare.next, HostPhase::Pending, "{phase:?}");
    package(&mut f, "s1", OTHER, ReleaseSource::Url);
    assert_eq!(run(&f, "s1", phase, 5), bare, "{phase:?}");
}

/// Catches: a staged host whose package went being kept in a step 1 to 5 until it is held,
/// or given back with the wrong undo: in each of the five steps the host is given back with
/// what that step has done undone (nothing is cordoned at step 1; the mark is written at
/// steps 4 and 5), the alert says `NotAvailable`, and the host goes to `Pending`, not
/// `Held`. A package of another commit is the same as none.
///
/// Mutants it catches: the recheck missing from any one of the five steps; a package of
/// another release read as the target; the give-back held instead.
#[test]
fn a_staged_host_whose_package_goes_is_given_back_in_each_of_steps_1_to_5() {
    let why = GiveBack::NotAvailable;
    let unmarked = gave_back(false, why.clone());
    let marked = gave_back(true, why.clone());
    // Nothing is cordoned yet at step 1, so nothing is uncordoned.
    let claim = [
        Command::CasSlot(SlotOp::Release),
        Command::Alert(Alert::GaveBack(why)),
    ];
    let cordoned = waiting_in(&HostPhase::Cordoned);
    let leases = waiting_in(&HostPhase::WaitingLeases);
    let planned = waiting_in(&HostPhase::PlannedDown);
    let gated = waiting_in(&HostPhase::Gated);
    is_given_back_when_the_package_goes(&storage_fleet(), &HostPhase::Selected, &claim);
    is_given_back_when_the_package_goes(&cordoned, &HostPhase::Cordoned, &unmarked);
    is_given_back_when_the_package_goes(&leases, &HostPhase::WaitingLeases, &unmarked);
    is_given_back_when_the_package_goes(&planned, &HostPhase::PlannedDown, &marked);
    is_given_back_when_the_package_goes(&gated, &HostPhase::Gated, &marked);
}

/// Catches: an activate order minted for a host whose package went in the look that was to
/// order it. A daemon-only host whose leases ended, and a storage host whose gate is open,
/// are each ready to enter `Activating`: with the package they do, with an order named; with
/// none they are given back, with no order named and no activate command, the mark undone for
/// the storage host.
///
/// Mutants it catches: the recheck missing from the step that waits for the leases, or from
/// the step that opens the gate (the move enters `Activating`).
#[test]
fn a_package_that_goes_just_before_the_activate_order_stops_it() {
    let mut daemon = linux_fleet();
    daemon.source = Source::Staged;
    package(&mut daemon, "d1", NEW, ReleaseSource::Bundled);
    let ready = run(&daemon, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(ready.next, HostPhase::Activating);
    assert_eq!(ready.commands, vec![activate()]);
    assert_eq!(ready.order, Some(NEXT_ORDER));
    host_mut(&mut daemon, "d1").available = None;
    let stopped = run(&daemon, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(stopped.next, HostPhase::Pending);
    assert_eq!(stopped.order, None);
    assert_eq!(stopped.commands, gave_back(false, GiveBack::NotAvailable));

    let mut storage = gated_fleet();
    storage.source = Source::Staged;
    package(&mut storage, "s1", NEW, ReleaseSource::Bundled);
    let ready = run(&storage, "s1", &HostPhase::Gated, 5);
    assert_eq!(ready.next, HostPhase::Activating);
    assert_eq!(ready.commands, vec![activate()]);
    assert_eq!(ready.order, Some(NEXT_ORDER));
    host_mut(&mut storage, "s1").available = None;
    let stopped = run(&storage, "s1", &HostPhase::Gated, 5);
    assert_eq!(stopped.next, HostPhase::Pending);
    assert_eq!(stopped.order, None);
    assert_eq!(stopped.commands, gave_back(true, GiveBack::NotAvailable));
}

/// Catches: a loss after the last check being given back instead of held. Once the activate
/// order is out the host cannot be put back: a storage host and a daemon-only host in
/// `Activating` whose package is gone stay in `Activating`, with no command and no alert.
///
/// Mutants it catches: the recheck added to the `Activating` step.
#[test]
fn a_package_that_goes_after_the_activate_order_is_not_read() {
    let mut storage = waiting_in(&HostPhase::Activating);
    storage.source = Source::Staged;
    package(&mut storage, "s1", NEW, ReleaseSource::Bundled);
    host_mut(&mut storage, "s1").available = None;
    let mut daemon = linux_fleet();
    daemon.source = Source::Staged;
    package(&mut daemon, "d1", NEW, ReleaseSource::Bundled);
    host_mut(&mut daemon, "d1").available = None;
    for (f, name) in [(&storage, "s1"), (&daemon, "d1")] {
        let moved = run(f, name, &HostPhase::Activating, 5);
        assert_eq!(moved.next, HostPhase::Activating, "{name}");
        assert!(moved.commands.is_empty(), "{name}: {moved:?}");
    }
}

/// Catches: the recheck applied to a fetch run, where a host has no package to lose: in each
/// of steps 1 to 5 a package of another release, or none, makes no difference to the move,
/// and a daemon-only host with leases ended is ordered to activate.
///
/// Mutants it catches: the source not read (a fetch run is given back for a package it never
/// needed).
#[test]
fn a_fetch_run_reads_no_package_in_any_step() {
    ignores_the_package_on_a_fetch_run(&storage_fleet(), &HostPhase::Selected);
    ignores_the_package_on_a_fetch_run(&waiting_in(&HostPhase::Cordoned), &HostPhase::Cordoned);
    let leases = waiting_in(&HostPhase::WaitingLeases);
    ignores_the_package_on_a_fetch_run(&leases, &HostPhase::WaitingLeases);
    let planned = waiting_in(&HostPhase::PlannedDown);
    ignores_the_package_on_a_fetch_run(&planned, &HostPhase::PlannedDown);
    ignores_the_package_on_a_fetch_run(&waiting_in(&HostPhase::Gated), &HostPhase::Gated);

    let mut daemon = linux_fleet();
    package(&mut daemon, "d1", OTHER, ReleaseSource::Url);
    let ready = run(&daemon, "d1", &HostPhase::WaitingLeases, 30);
    assert_eq!(ready.next, HostPhase::Activating);
    assert_eq!(ready.commands, vec![activate()]);
}
