//! The controller against a model farm, on a staged run (`update/desired.source`): which hosts
//! it starts and what it does when a host's package goes. The picker's gate on its own is in
//! `update_pick_source.rs`, the node step's in `update_node_source.rs`; the order that reaches
//! the node is `update_session.rs`'s.
//!
//! Each test names the defect it would catch. Each 'Mutants it catches' line names a change
//! that turns the test red.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::MemberId;
use kbf_server::update::{
    Alert, Controller, GiveBack, HostPhase, OrderId, Policy, Release, Skip, Source, Transport,
    TransportError, WaitReason,
};
use kbf_server::update_status::ReleaseSource;
use kbf_types::WorkerId;
use support::harness::{Backing, Rig, row_of};
use support::world::Spec;
use support::{NEW, OLD, id};

/// The activate orders that reached the farm: the host, the order and the source it was given.
type Given = Arc<Mutex<Vec<(String, OrderId, Source)>>>;

/// A transport that notes the source of every activate order it passes on.
struct SourceSpy {
    inner: Box<dyn Transport>,
    given: Given,
}

impl Transport for SourceSpy {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.inner.cordon(host)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.inner.uncordon(host)
    }

    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.inner.mark_down(host, member)
    }

    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.inner.undown(host, member)
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
        source: Source,
    ) -> Result<(), TransportError> {
        let mut given = self.given.lock().unwrap_or_else(PoisonError::into_inner);
        given.push((host.as_str().to_owned(), order, source));
        drop(given);
        self.inner.activate(host, order, release, source)
    }

    fn confirm(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.inner.confirm(host, order, release)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        self.inner.alert(host, alert)
    }
}

/// Gives the rig a new controller (as a restart does) whose transport notes into `given`.
fn spy(rig: &mut Rig, given: &Given) {
    let transport = SourceSpy {
        inner: rig.world.transport(),
        given: Arc::clone(given),
    };
    rig.controller = Controller::new(
        rig.cell.store(),
        rig.epoch,
        Box::new(transport),
        rig.world.source(),
    );
}

/// The activate orders noted so far.
fn given_by(given: &Given) -> Vec<(String, OrderId, Source)> {
    given.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

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

/// Catches: an activate order that names the source of the run that is desired now instead of
/// the run that began the host, and one that names a fetch whatever the run says. `d1` begins
/// under a staged offer of `v2` and is given its activate order; a rollback is then offered as
/// a fetch, and a new controller (a restart) gives the orders of the rows it finds again. The
/// desired run is a fetch by then, the host's row still says staged, and the order given again
/// says staged, as the first did.
///
/// Mutants it catches: the source of `update/desired` passed in place of the row's (the order
/// given again says fetch); a fetch passed whatever the row says (the first order says fetch).
#[test]
fn an_activate_order_names_the_source_of_the_run_that_began_the_host() {
    let mut rig = Rig::new(Backing::Fake, "again", &spec(1), Policy::default());
    // A staged run starts only hosts whose package carries the release.
    rig.world.carry(NEW, ReleaseSource::Url);
    let given = Given::default();
    spy(&mut rig, &given);
    rig.offer_from(NEW, Source::Staged);
    rig.until("the activate order", 400, |r| {
        r.phase("d1") == HostPhase::Activating
    });
    let order = row_of(&rig.rows, "d1").record.order;
    let ordered = |source| ("d1".to_owned(), order, source);
    assert_eq!(given_by(&given), [ordered(Source::Staged)]);

    rig.offer(OLD);
    let desired = rig.rows.desired().unwrap().value.unwrap();
    assert_eq!(desired.source, Source::Fetch, "the desired run is a fetch");
    spy(&mut rig, &given);
    rig.tick();
    assert_eq!(row_of(&rig.rows, "d1").source, Source::Staged);
    assert_eq!(row_of(&rig.rows, "d1").record.order, order);
    assert_eq!(
        given_by(&given),
        [ordered(Source::Staged), ordered(Source::Staged)]
    );
}

/// Catches: a fetch run's activate order that names a staged source (the installer would use
/// the release its package carries, not the one the order names). `d1` begins under a fetch
/// offer and is given its activate order, which names a fetch.
///
/// Mutants it catches: a staged source passed whatever the row says.
#[test]
fn a_fetch_runs_activate_order_names_the_fetch_source() {
    let mut rig = Rig::new(Backing::Fake, "fetch", &spec(1), Policy::default());
    let given = Given::default();
    spy(&mut rig, &given);
    rig.offer(NEW);
    rig.until("the activate order", 400, |r| {
        r.phase("d1") == HostPhase::Activating
    });
    let order = row_of(&rig.rows, "d1").record.order;
    assert_eq!(given_by(&given), [("d1".to_owned(), order, Source::Fetch)]);
}
