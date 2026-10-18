//! The controller where the farm or another controller lets it down: a place taken between
//! its look and its claim, a host whose storage member the farm does not show when the order
//! to mark or unmark it comes, and the one line a log prints of it.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_server::update::{
    Command, Controller, FactsSource, FarmView, Generation, HostPhase, HostRecord, HostRow, Policy,
    Release, Rows, SlotClass, SourceError, Stored,
};
use kbf_types::FarmTime;
use support::harness::{Backing, Rig};
use support::world::{Gave, Spec};
use support::{BOOT, NEW, id};

fn spec(storage: u64, linux: u64) -> Spec {
    Spec {
        storage,
        linux,
        macs: 0,
        ..Spec::default()
    }
}

fn absent() -> Stored<HostRow> {
    Stored {
        value: HostRow::unstarted(),
        version: None,
    }
}

/// A source that, at its first look, does what a second controller does between the first
/// one's reads and its writes: it puts `host` in a step and gives it the one place for hosts.
/// (A controller reads the slot and the rows before it asks the farm, so the look is the
/// moment after its reads.)
struct Intruder {
    inner: Box<dyn FactsSource>,
    rows: Rows,
    host: &'static str,
    epoch: u64,
    generation: Generation,
    done: bool,
}

impl FactsSource for Intruder {
    fn look(&mut self, now: FarmTime) -> Result<FarmView, SourceError> {
        if !self.done {
            self.done = true;
            let order = self.rows.mint_order(self.epoch).expect("an order id");
            let row = HostRow {
                record: HostRecord {
                    phase: HostPhase::Cordoned,
                    since: now,
                    generation: self.generation,
                    order,
                    boot_before: None,
                },
                commit: Release::new(NEW),
            };
            self.rows
                .put_host(&id(self.host), &absent(), &row)
                .expect("the rival's row");
            self.rows
                .claim(SlotClass::Host, &id(self.host), order, 1)
                .expect("a claim")
                .expect("a free place");
        }
        self.inner.look(now)
    }
}

/// Catches: a claim that was lost to another controller being taken for a claim won. The look
/// shows the one place for hosts free; before the claim is made another controller takes it
/// for `d2`. The claim reads the slot again and is refused: `d1`, whose row says `Selected`,
/// holds no place, no order is given for it, and `d2` keeps its place; the next look sees the
/// slot full of another and puts `d1` back to `Pending` with no order and no alert. A
/// controller that believed its claim would cordon `d1` beside `d2`, two hosts out for a limit
/// of one.
#[test]
fn a_claim_another_controller_beat_to_the_place_leaves_the_slot_theirs() {
    let mut rig = Rig::new(Backing::Fake, "lost-claim", &spec(0, 2), Policy::default());
    let generation = rig.offer(NEW);
    let intruder = Intruder {
        inner: rig.world.source(),
        rows: Rows::new(rig.cell.store()),
        host: "d2",
        epoch: rig.epoch,
        generation,
        done: false,
    };
    rig.controller = Controller::new(
        rig.cell.store(),
        rig.epoch,
        rig.world.transport(),
        Box::new(intruder),
    );

    rig.world.advance();
    let tick = rig.controller.tick(rig.world.time()).expect("a tick");
    assert_eq!(tick.started, [id("d1")], "{tick:?}");
    assert!(tick.errors.is_empty() && !tick.conflict, "{tick:?}");
    let slot = rig.rows.slot().unwrap().value;
    assert!(slot.holds(&id("d2")), "the rival lost its place: {slot:?}");
    assert!(!slot.holds(&id("d1")), "a lost claim was kept: {slot:?}");
    assert_eq!(slot.held(SlotClass::Host), 1, "{slot:?}");
    assert_eq!(rig.phase("d1"), HostPhase::Selected);
    assert!(
        rig.world.lock().given_to("d1").is_empty(),
        "an order for d1"
    );

    rig.world.advance();
    let tick = rig.controller.tick(rig.world.time()).expect("a tick");
    assert!(tick.errors.is_empty() && !tick.conflict, "{tick:?}");
    assert_eq!(rig.phase("d1"), HostPhase::Pending, "given back");
    let w = rig.world.lock();
    assert!(w.given_to("d1").is_empty(), "an order for d1");
    assert!(
        w.alerts.iter().all(|(host, _)| host != "d1"),
        "a lost place is not an alert: {:?}",
        w.alerts
    );
    drop(w);
    let slot = rig.rows.slot().unwrap().value;
    assert!(slot.holds(&id("d2")) && !slot.holds(&id("d1")), "{slot:?}");
}

/// Hides the storage member of one host from the facts while `unlinked` is set.
struct Unlinked {
    inner: Box<dyn FactsSource>,
    host: &'static str,
    unlinked: Arc<AtomicBool>,
}

impl FactsSource for Unlinked {
    fn look(&mut self, now: FarmTime) -> Result<FarmView, SourceError> {
        let mut view = self.inner.look(now)?;
        if self.unlinked.load(Ordering::SeqCst) {
            for host in &mut view.hosts {
                if host.id.as_str() == self.host {
                    host.member = None;
                }
            }
        }
        Ok(view)
    }
}

/// Catches: an order to mark or unmark a member that the farm does not show being given for
/// a member nobody named, or counted as given. `s1` is found `PlannedDown` by a controller
/// that has just started (its mark was never made), and the farm shows `s1` with no storage
/// member: the replay of the row cannot mark one, the step gives the host back, and its undo
/// order cannot unmark one either. Both are reported by name, nothing reaches the farm, and
/// the row is not written `Pending` (nothing would read it again to unmark the node, which
/// may be marked): `s1` stays `PlannedDown` with its place. When the farm shows the member
/// again the mark is given.
#[test]
fn a_mark_or_unmark_for_a_member_the_farm_does_not_show_is_reported_not_given() {
    let mut rig = Rig::new(Backing::Fake, "unlinked", &spec(2, 0), Policy::default());
    let generation = rig.offer(NEW);
    let order = rig.rows.mint_order(rig.epoch).unwrap();
    let row = HostRow {
        record: HostRecord {
            phase: HostPhase::PlannedDown,
            since: rig.world.time(),
            generation,
            order,
            boot_before: Some(BOOT),
        },
        commit: Release::new(NEW),
    };
    rig.rows.put_host(&id("s1"), &absent(), &row).unwrap();
    rig.rows
        .claim(SlotClass::Storage, &id("s1"), order, 1)
        .unwrap()
        .expect("the storage place");
    {
        let mut world = rig.world.lock();
        let at = world.host_index("s1");
        world.hosts[at].cordoned = true;
    }
    let unlinked = Arc::new(AtomicBool::new(true));
    let source = Unlinked {
        inner: rig.world.source(),
        host: "s1",
        unlinked: Arc::clone(&unlinked),
    };
    rig.controller = Controller::new(
        rig.cell.store(),
        rig.epoch,
        rig.world.transport(),
        Box::new(source),
    );

    rig.world.advance();
    let tick = rig.controller.tick(rig.world.time()).expect("a tick");
    let failed: Vec<_> = tick
        .errors
        .iter()
        .map(|e| (e.host.clone(), e.command.clone()))
        .collect();
    assert_eq!(
        failed,
        [(id("s1"), Command::MarkDown), (id("s1"), Command::Undown)],
        "{tick:?}"
    );
    for error in &tick.errors {
        assert!(
            error.error.0.contains("s1") && error.error.0.contains("no storage member"),
            "{error:?}"
        );
    }
    assert!(tick.moved.is_empty(), "{tick:?}");
    assert_eq!(rig.phase("s1"), HostPhase::PlannedDown, "the row moved");
    assert!(rig.rows.slot().unwrap().value.holds(&id("s1")));
    assert!(rig.world.lock().given_to("s1").is_empty(), "an order");

    unlinked.store(false, Ordering::SeqCst);
    rig.world.advance();
    let tick = rig.controller.tick(rig.world.time()).expect("a tick");
    assert!(tick.errors.is_empty(), "{tick:?}");
    let w = rig.world.lock();
    assert!(
        w.given_to("s1").contains(&&Gave::MarkDown),
        "the mark was never given: {:?}",
        w.given_to("s1")
    );
}

/// Catches: a `Debug` of the controller that prints the store, the transport or the rows it
/// holds (none is `Debug`, and a log line must not carry them): it names the epoch, which is
/// what an operator looking at a log wants to know, and nothing else.
#[test]
fn a_controller_prints_its_epoch_and_nothing_it_holds() {
    let rig = Rig::new(Backing::Fake, "debug", &spec(1, 0), Policy::default());
    let controller = Controller::new(
        rig.cell.store(),
        9,
        rig.world.transport(),
        rig.world.source(),
    );
    assert_eq!(format!("{controller:?}"), "Controller { epoch: 9, .. }");
}
