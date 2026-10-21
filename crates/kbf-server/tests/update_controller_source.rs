//! The controller against a model farm, on a staged run (`update/desired.source`): which hosts
//! it starts and what it does when a host's package goes. The picker's gate on its own is in
//! `update_pick_source.rs`, the node step's in `update_node_source.rs`.
//!
//! Each test names the defect it would catch. Each 'Mutants it catches' line names a change
//! that turns the test red.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{Alert, GiveBack, HostPhase, Policy, Skip, Source, WaitReason};
use kbf_server::update_status::ReleaseSource;
use support::harness::{Backing, Rig};
use support::world::Spec;
use support::{NEW, id};

fn spec(linux: u64) -> Spec {
    Spec {
        storage: 0,
        linux,
        macs: 0,
        ..Spec::default()
    }
}

/// Catches: a staged run that starts a host whose installer does not report its package (it
/// would be sent an activate order for a release it may not have), and a tick that does not
/// say why nothing started: both hosts are named as too old to report packages, no order is
/// given and no cordon is ordered.
///
/// Mutants it catches: the picker's gate removed (`d1` is started); the skips dropped from
/// the answer (the tick says nothing is waiting).
#[test]
fn a_staged_run_over_hosts_that_report_no_package_starts_none_and_says_why() {
    let mut rig = Rig::new(Backing::Fake, "bare", &spec(2), Policy::default());
    rig.offer_from(NEW, Source::Staged);
    let tick = rig.tick();
    assert!(tick.started.is_empty(), "{tick:?}");
    let skipped = vec![
        (id("d1"), Skip::InstallerTooOld),
        (id("d2"), Skip::InstallerTooOld),
    ];
    assert_eq!(tick.waiting, Some(WaitReason::NoneEligible(skipped)));
    assert!(rig.world.lock().given.is_empty());
    assert!(!rig.world.lock().host("d1").cordoned, "no cordon");
    assert_eq!(rig.phase("d1"), HostPhase::Pending);
    assert_eq!(rig.phase("d2"), HostPhase::Pending);
}

/// Catches: a staged host that loses its package while it waits for its leases going on
/// to the activate order, or being held when the order is refused: it is given back at the
/// next look, uncordoned, its place freed, a warning sent, and no activate order is ever
/// given. `d1` carries the release, has a lease of 600 s, and is forced after the idle wait;
/// the package goes while it waits. At the look after, it is not started again: it is named
/// as skipped for the package it no longer has.
///
/// Mutants it catches: the recheck missing from the step that waits for the leases (the host
/// waits on, cordoned, for the lease); `GiveBack::NotAvailable` reported as a hold; the
/// picker's gate removed (`d1` is started again at the look after).
#[test]
fn a_staged_host_waiting_for_its_leases_is_given_back_when_its_package_goes() {
    let mut rig = Rig::new(Backing::Fake, "gone", &spec(1), Policy::default());
    rig.world.carry(NEW, ReleaseSource::Bundled);
    rig.world.lease("d1", 600);
    rig.offer_from(NEW, Source::Staged);
    rig.until("d1 to wait for its lease", 400, |r| {
        r.phase("d1") == HostPhase::WaitingLeases
    });

    rig.world.drop_packages();
    let tick = rig.tick();
    assert_eq!(rig.phase("d1"), HostPhase::Pending, "given back");
    assert!(tick.started.is_empty(), "{tick:?}");
    assert!(!rig.world.lock().host("d1").cordoned, "uncordoned");
    assert!(
        rig.rows.slot().unwrap().value.is_empty(),
        "its place is free"
    );
    assert_eq!(
        rig.world.lock().alerts,
        [("d1".to_owned(), Alert::GaveBack(GiveBack::NotAvailable))]
    );
    assert!(rig.world.lock().activations().is_empty());

    let again = rig.tick();
    assert!(again.started.is_empty(), "{again:?}");
    assert_eq!(rig.phase("d1"), HostPhase::Pending, "not started again");
    let skipped = vec![(id("d1"), Skip::NotAvailable { has: None })];
    assert_eq!(again.waiting, Some(WaitReason::NoneEligible(skipped)));
}
