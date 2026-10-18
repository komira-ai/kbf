//! The order ids a controller draws (`docs/design/split-mode-updates.md` section 6.1: an
//! order carries the store's epoch and a monotonic id, so that a restored store cannot replay
//! one). `Controller::new` is given the state store's epoch and every id it draws is under
//! it; the rows a controller stops on when the epoch is below theirs are
//! `update_controller.rs`'s.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::MemberId;
use kbf_server::update::{Alert, Controller, OrderId, Policy, Release, Transport, TransportError};
use kbf_types::WorkerId;
use support::harness::{Backing, Rig, finished, row_of};
use support::world::Spec;
use support::{NEW, OLD};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

/// An order that reached the helper: the host, which order, and its id.
type Given = (String, &'static str, OrderId);

/// A transport that notes the id of every order that carries one.
struct Recorder {
    inner: Box<dyn Transport>,
    given: Arc<Mutex<Vec<Given>>>,
}

impl Recorder {
    fn note(&self, host: &WorkerId, kind: &'static str, order: OrderId) {
        let mut given = self.given.lock().unwrap_or_else(PoisonError::into_inner);
        given.push((host.as_str().to_owned(), kind, order));
    }
}

impl Transport for Recorder {
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
    ) -> Result<(), TransportError> {
        self.note(host, "activate", order);
        self.inner.activate(host, order, release)
    }

    fn confirm(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.note(host, "confirm", order);
        self.inner.confirm(host, order, release)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        self.inner.alert(host, alert)
    }
}

/// Offers `target` and runs it to its end with a new controller built at `epoch`. Returns
/// every order that reached the helpers and every claim the slot held at any look.
fn run_under(rig: &mut Rig, epoch: u64, target: &str) -> (Vec<Given>, Vec<OrderId>) {
    let given = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder {
        inner: rig.world.transport(),
        given: Arc::clone(&given),
    };
    rig.epoch = epoch;
    rig.controller = Controller::new(
        rig.cell.store(),
        epoch,
        Box::new(recorder),
        rig.world.source(),
    );
    rig.offer(target);
    rig.world.lock().new_run();
    let want = Release::new(target);
    let mut claims = Vec::new();
    rig.until("the run to end", 30_000, |r| {
        let slot = r.rows.slot().unwrap().value;
        claims.extend(slot.holders().map(|(_, holder)| holder.order));
        let generation = r.rows.desired().unwrap().value.unwrap().generation;
        finished(&r.world.lock(), &r.rows, generation, &want)
    });
    let given = given.lock().unwrap_or_else(PoisonError::into_inner).clone();
    (given, claims)
}

/// Catches: an id drawn under an epoch other than the one the controller was built with (a
/// constant, the epoch of an earlier controller, or none): a helper that has seen the epoch
/// of the store as it is would refuse the order, or, after a restore, would accept an id it
/// had already acted on. A controller built at epoch 7 runs a farm forward: every activate
/// order, the confirm that names the same id, every claim in the slot and every host's row
/// carry 7. A controller built at epoch 9 over the same store (a restore moved the epoch)
/// walks it back: everything it draws carries 9, above every id of the first run.
#[test]
fn every_order_id_the_controller_draws_carries_its_construction_epoch() {
    for backing in BACKINGS {
        let spec = Spec {
            storage: 2,
            linux: 1,
            macs: 1,
            ..Spec::default()
        };
        let hosts = ["s1", "s2", "d1", "m1"];
        let mut rig = Rig::new(backing, "epochs", &spec, Policy::default());

        let (forward, forward_claims) = run_under(&mut rig, 7, NEW);
        assert_eq!(
            forward.len(),
            2 * hosts.len(),
            "{backing:?}: an activate and a confirm for each host: {forward:?}"
        );
        assert!(
            forward.iter().all(|(_, _, order)| order.epoch == 7),
            "{backing:?}: {forward:?}"
        );
        assert!(!forward_claims.is_empty(), "{backing:?}: no claim was seen");
        assert!(
            forward_claims.iter().all(|order| order.epoch == 7),
            "{backing:?}: {forward_claims:?}"
        );
        for host in hosts {
            let row = row_of(&rig.rows, host);
            assert_eq!(row.record.order.epoch, 7, "{backing:?}: {host}'s row");
            let mine: Vec<(&str, OrderId)> = forward
                .iter()
                .filter(|(to, _, _)| to == host)
                .map(|(_, kind, order)| (*kind, *order))
                .collect();
            let order = row.record.order;
            assert_eq!(
                mine,
                [("activate", order), ("confirm", order)],
                "{backing:?}: {host}: the confirm names the id of its activate order"
            );
        }

        let (back, back_claims) = run_under(&mut rig, 9, OLD);
        assert_eq!(back.len(), 2 * hosts.len(), "{backing:?}: {back:?}");
        assert!(
            back.iter().all(|(_, _, order)| order.epoch == 9),
            "{backing:?}: {back:?}"
        );
        assert!(!back_claims.is_empty(), "{backing:?}: no claim was seen");
        assert!(
            back_claims.iter().all(|order| order.epoch == 9),
            "{backing:?}: {back_claims:?}"
        );
        let last = forward.iter().map(|(_, _, order)| *order).max().unwrap();
        assert!(
            back.iter().all(|(_, _, order)| *order > last),
            "{backing:?}: the walk back drew an id the first run could have seen"
        );
        for host in hosts {
            let epoch = row_of(&rig.rows, host).record.order.epoch;
            assert_eq!(epoch, 9, "{backing:?}: {host}'s row after the walk back");
        }
    }
}
