//! VM slots on a Mac (`LeaseKind::Vm`): a Mac holds at most the slots it reports, a VM
//! books its memory like its cores, and a VM and a whole-machine lease never share a Mac.
//! Each test names the defect it catches.

use kbf_caps::NodeCaps;
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind,
    Outcome, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
/// What the server books on a 12-core, 32 GiB Mac that reports two VM slots, after the
/// default host floor of 2 cores and 8 GiB.
const MAC: Resources = Resources::new(10_000, 24 * GIB).with_vms(2);

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// Operation `n` of `kind` with `resources`.
fn request(n: u8, kind: LeaseKind, resources: Resources) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        kind,
        resources,
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

/// A VM of 4 cores and `mem_gib` GiB.
fn vm(n: u8, mem_gib: u64) -> Request {
    request(
        n,
        LeaseKind::Vm,
        Resources::new(4_000, mem_gib * GIB).with_vms(1),
    )
}

/// An action of 1 core and `mem_gib` GiB.
fn action(n: u8, mem_gib: u64) -> Request {
    request(n, LeaseKind::Action, Resources::new(1_000, mem_gib * GIB))
}

fn whole(n: u8) -> Request {
    request(n, LeaseKind::WholeMachine, MAC)
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

struct Harness {
    sched: Scheduler,
    now: u64,
}

impl Harness {
    fn new() -> Self {
        Self {
            sched: Scheduler::new(1),
            now: 1_000,
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.sched
            .apply(Input::new(FarmTime::from_millis(self.now), event))
    }

    /// Registers `name`, a Mac of `capacity` that runs VMs, actions and whole-machine
    /// leases.
    fn mac(&mut self, name: &str, capacity: Resources) {
        let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
            .unwrap()
            .with_drivers(["native", "native-whole-machine", "vm"]);
        let effects = self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity,
            caps,
        });
        assert!(effects.is_empty(), "{effects:?}");
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// Ticks, commits and starts every grant proposed; the grants, and the reasons given
    /// to operations that wait.
    fn tick(&mut self) -> (Vec<LeaseGrant>, Vec<String>) {
        let (mut grants, mut reasons) = (Vec::new(), Vec::new());
        for effect in self.feed(Event::Tick) {
            match effect {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g),
                Effect::Waiting(waiting) => reasons.extend(waiting.reason),
                other => panic!("a tick proposed {other:?}"),
            }
        }
        for grant in &grants {
            let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
            assert!(
                matches!(effects.as_slice(), [Effect::Start(_)]),
                "{effects:?}"
            );
            self.feed(Event::Started {
                operation: grant.operation,
                lease: grant.lease,
            });
        }
        (grants, reasons)
    }

    /// Reports `grant` completed and commits the result, which frees what it booked.
    fn finish(&mut self, grant: &LeaseGrant) {
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome: Outcome::Completed {
                action_result: digest(200),
            },
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        self.feed(Event::Committed(record.clone()));
    }

    fn booked(&self, name: &str) -> Resources {
        self.sched.booked(&w(name)).expect("registered")
    }

    /// Moves farm time to `millis` and ticks, without starting grants.
    fn tick_at(&mut self, millis: u64) -> (Vec<LeaseGrant>, Vec<String>) {
        self.now = millis;
        self.tick()
    }
}

/// Catches: a third VM placed on a Mac that has two slots (Apple allows two guests), and
/// a slot not given back when its VM ends, so that the third VM waits forever. The third
/// VM waits as servable, with no reason: a Mac that has slots will take it.
#[test]
fn a_third_vm_waits_for_a_slot() {
    let mut h = Harness::new();
    h.mac("mac", MAC);
    for n in 1..=3 {
        h.submit(u64::from(n), vm(n, 8));
    }
    let (grants, reasons) = h.tick();
    assert_eq!(grants.len(), 2, "two slots, two VMs: {grants:?}");
    assert_eq!(h.booked("mac").vms, 2);
    assert!(reasons.is_empty(), "the third VM is servable: {reasons:?}");
    assert!(h.tick().0.is_empty(), "a third VM beside two");
    h.finish(&grants[0]);
    assert_eq!(h.booked("mac").vms, 1);
    let (next, _) = h.tick();
    assert_eq!(next.len(), 1, "the freed slot goes to the third VM");
    assert_eq!(h.booked("mac").vms, 2);
}

/// Catches: a VM placed on a Mac that reports no slot, and a VM that waits as servable
/// when no live Mac has a slot, so the caller is never told.
#[test]
fn a_mac_without_slots_takes_no_vm_and_the_caller_is_told() {
    let mut h = Harness::new();
    h.mac("mac", Resources::new(12_000, 32 * GIB));
    h.submit(1, vm(1, 8));
    let (grants, reasons) = h.tick();
    assert!(grants.is_empty(), "{grants:?}");
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("VM slot"), "{reasons:?}");
}

/// Catches: a whole-machine lease placed beside a running VM, a VM placed beside a
/// whole-machine lease, and a Mac left unusable after either ends.
#[test]
fn a_vm_and_a_whole_machine_lease_never_share_a_mac() {
    let mut h = Harness::new();
    h.mac("mac", MAC);
    h.submit(1, whole(1));
    h.submit(2, vm(2, 8));
    let (grants, _) = h.tick();
    let [whole_grant] = grants.as_slice() else {
        panic!("{grants:?}");
    };
    assert_eq!(h.booked("mac"), MAC, "it books the whole Mac");
    assert!(h.tick().0.is_empty(), "a VM beside a whole-machine lease");
    h.finish(whole_grant);
    let (grants, _) = h.tick();
    let [vm_grant] = grants.as_slice() else {
        panic!("{grants:?}");
    };

    h.submit(3, whole(3));
    assert!(h.tick().0.is_empty(), "a whole-machine lease beside a VM");
    h.finish(vm_grant);
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1, "the Mac is free again: {grants:?}");
}

/// Catches: a VM's memory not counted against what other leases hold, so two 14 GiB
/// guests are placed on a Mac that has 24 GiB; and a VM that fits in cores but not in
/// the memory left. Pool actions are unchanged: an action's memory is a limit and not a
/// booking, so an action is placed beside a VM that holds most of the memory.
#[test]
fn vm_memory_is_never_overcommitted() {
    let mut h = Harness::new();
    h.mac("mac", MAC);
    h.submit(1, vm(1, 14));
    h.submit(2, vm(2, 14));
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1, "28 GiB of VMs on 24 GiB: {grants:?}");
    assert_eq!(h.booked("mac").memory_bytes, 14 * GIB);
    h.submit(3, vm(3, 10));
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1, "10 GiB fits in the 10 left: {grants:?}");
    assert_eq!(h.booked("mac").memory_bytes, 24 * GIB);

    // The pool is not held back by a VM's memory.
    h.submit(4, action(4, 8));
    let (actions, _) = h.tick();
    assert_eq!(actions.len(), 1, "an action beside full VMs: {actions:?}");
}

/// Catches: a VM placed while pool actions hold the memory it needs (the memory it books
/// is added to theirs and the Mac is overcommitted), and the check applied to actions too,
/// which would hold a 20 GiB action back until the VM ends.
#[test]
fn a_vm_waits_for_the_memory_actions_hold() {
    let mut h = Harness::new();
    h.mac("mac", MAC);
    h.submit(1, action(1, 20));
    let (grants, _) = h.tick();
    let [held] = grants.as_slice() else {
        panic!("{grants:?}");
    };
    h.submit(2, vm(2, 8));
    assert!(h.tick().0.is_empty(), "8 GiB of VM on the 4 GiB left");
    h.finish(held);
    assert_eq!(h.tick().0.len(), 1, "the memory is free: the VM runs");
}

/// Catches: a VM request refused for good when its only Mac is away, so a Mac that
/// restarts for an update fails every queued VM; a VM lease not given up when its Mac is
/// silent past the grace; and a Mac that comes back with slots that are not offered.
#[test]
fn a_mac_that_goes_away_and_returns_takes_its_vms_again() {
    let mut h = Harness::new();
    h.mac("mac", MAC);
    h.submit(1, vm(1, 8));
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1);

    // Silent past the lease grace: the lease goes back to the queue, and no worker is
    // connected, so the verdict says so.
    let gone = 1_000 + u64::try_from(LEASE_GRACE.as_millis()).unwrap() + 1_000;
    let (grants, reasons) = h.tick_at(gone);
    assert!(grants.is_empty(), "{grants:?}");
    assert_eq!(reasons, ["no worker is connected"]);

    // It returns, and the queued VM runs.
    h.mac("mac", MAC);
    let (grants, _) = h.tick();
    assert_eq!(
        grants.len(),
        1,
        "the VM runs on the returned Mac: {grants:?}"
    );
    assert_eq!(h.booked("mac").vms, 1);
}

/// Catches: a VM that asks for the whole Mac's memory placed beside another lease (it
/// would not be alone as a request at the host's memory must be), and one that is never
/// placed on an empty Mac. The action books no memory, so only the exclusive rule holds
/// the VM back while it runs.
#[test]
fn a_vm_at_the_whole_macs_memory_runs_alone() {
    let mut h = Harness::new();
    h.mac("mac", MAC);
    h.submit(1, action(1, 0));
    let (grants, _) = h.tick();
    let [held] = grants.as_slice() else {
        panic!("{grants:?}");
    };
    h.submit(2, vm(2, 24));
    assert!(
        h.tick().0.is_empty(),
        "a VM at the host's memory beside an action"
    );
    h.finish(held);
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1, "the empty Mac takes it: {grants:?}");
    assert_eq!(h.booked("mac").memory_bytes, 24 * GIB);
}

/// Catches: a VM placed on a capped Mac that holds as many leases as its cap while a VM
/// slot is free (the VM check skipping the lease cap `action_slots` sets), and a freed
/// lease slot not taking the waiting VM. The Mac has two VM slots and one lease slot.
#[test]
fn a_vm_with_a_free_vm_slot_but_no_free_lease_slot_is_refused() {
    let mut h = Harness::new();
    h.mac("mac", MAC.with_lease_cap(1));
    h.submit(1, action(1, 1));
    h.submit(2, vm(2, 8));
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1, "one lease slot, one lease: {grants:?}");
    assert_eq!(h.booked("mac").vms, 0, "a VM slot is free");
    assert!(h.tick().0.is_empty(), "a VM past the lease cap");
    h.finish(&grants[0]);
    let (next, _) = h.tick();
    assert_eq!(next.len(), 1, "the freed lease slot goes to the VM");
    assert_eq!(h.booked("mac").vms, 1);
}

/// Catches: a VM placed on a Mac whose only VM slot is taken because lease slots are
/// free (the lease cap standing in for the VM slot count), and a VM that does not book its
/// lease slot, which lets actions overrun the cap. The Mac has one VM slot and three lease
/// slots.
#[test]
fn a_vm_with_a_free_lease_slot_but_no_free_vm_slot_is_refused() {
    let mut h = Harness::new();
    h.mac("mac", MAC.with_vms(1).with_lease_cap(3));
    h.submit(1, vm(1, 8));
    h.submit(2, vm(2, 8));
    let (grants, _) = h.tick();
    assert_eq!(grants.len(), 1, "one VM slot, one VM: {grants:?}");
    assert_eq!(h.booked("mac").vms, 1);
    assert!(h.tick().0.is_empty(), "a second VM on one VM slot");
    h.submit(3, action(3, 1));
    h.submit(4, action(4, 1));
    h.submit(5, action(5, 1));
    let (actions, _) = h.tick();
    assert_eq!(
        actions.len(),
        2,
        "the VM books one of the three lease slots: {actions:?}"
    );
}
