//! iOS device leases (`docs/design/ios-devices.md`, section 5.4): a request with
//! `ios.device=1` books one device that satisfies every device attribute, the `Start`
//! names it, and the booking lasts until the lease ends.

use kbf_caps::NodeCaps;
use kbf_sched::{DEVICE_LEASES_PER_WORKER, DaemonInstance, Event, Input, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind,
    Outcome, Qos, Resources, StartLease, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

/// An iPhone17,3 on iOS 18.6.
const PHONE_18: &str =
    "class=iPhone,id=dev-a,os_build=22G86,os_version=18.6,product_type=iPhone17,3";
/// An iPad on iOS 26.0.
const PAD_26: &str = "class=iPad,id=dev-b,os_build=23A341,os_version=26.0,product_type=iPad16,3";
/// An iPhone17,3 on iOS 18.6, sorting before [`PHONE_26`].
const PHONE_18_C: &str =
    "class=iPhone,id=dev-c,os_build=22G86,os_version=18.6,product_type=iPhone17,3";
/// An iPhone17,3 on iOS 26.0.
const PHONE_26: &str =
    "class=iPhone,id=dev-d,os_build=23A341,os_version=26.0,product_type=iPhone17,3";

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// Action `n`, one core and 1 GiB, needing what the REAPI `platform` asks for.
fn request(n: u64, platform: &[(&str, &str)]) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform(platform.iter().copied()).unwrap(),
    }
}

/// `ios.device=1` for an iPhone17,3 on iOS 26.0.
fn phone_26(n: u64) -> Request {
    request(
        n,
        &[
            ("ios.device", "1"),
            ("ios.device.os_version", "26.0"),
            ("ios.device.product_type", "iPhone17,3"),
        ],
    )
}

/// Any device.
fn any_device(n: u64) -> Request {
    request(n, &[("ios.device", "1")])
}

/// A Mac running the native driver whose report lists `devices`.
fn mac(devices: &[&str]) -> NodeCaps {
    let entries = [("arch", "arm64"), ("os", "macos")]
        .into_iter()
        .chain(devices.iter().map(|d| ("ios.device", *d)));
    NodeCaps::from_report(entries)
        .unwrap()
        .with_drivers(["native"])
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn completed(n: u64) -> Outcome {
    Outcome::Completed {
        action_result: digest(100 + n),
    }
}

struct Harness {
    s: Scheduler,
    now: FarmTime,
}

impl Harness {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1),
            now: FarmTime::default(),
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` (daemon process `name`) with 8 cores, 16 GiB and `caps`: a new
    /// session of that process if it registered before.
    fn worker(&mut self, name: &str, caps: NodeCaps) {
        let event = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(8_000, 16 * GIB),
            caps,
        };
        assert!(self.feed(event).is_empty());
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        self.feed(Event::Submit {
            waiter: WaiterId(waiter),
            request,
        });
    }

    /// Ticks and returns the grants it proposed.
    fn grants(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Commit(ControlRecord::Lease(g)) => Some(g),
                _ => None,
            })
            .collect()
    }

    /// Ticks, expects one grant, commits it and returns its Start.
    fn one_start(&mut self) -> (LeaseGrant, StartLease) {
        let grants = self.grants();
        let [grant] = grants.as_slice() else {
            panic!("one grant expected: {grants:?}");
        };
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        let [Effect::Start(start)] = effects.as_slice() else {
            panic!("committing {grant:?} gave {effects:?}");
        };
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
        (grant.clone(), start.clone())
    }

    /// Reports `grant` completed and commits the result.
    fn finish(&mut self, grant: &LeaseGrant) {
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome: completed(grant.operation.0),
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        let answered = self.feed(Event::Committed(record.clone()));
        assert!(
            matches!(answered.as_slice(), [Effect::Answer(_)]),
            "{answered:?}"
        );
    }
}

/// Catches: device attributes matched across a node's devices rather than within one,
/// a booking of the first device by id whatever its attributes, and a `Start` that does
/// not name the booked device. Worker `a-split` (first by name) has an iPhone17,3 on
/// 18.6 and an iPad on 26.0: each key is met by one of them, neither meets both, so
/// a per-key match would place the lease there. Worker `b-both` has an iPhone17,3 on
/// 18.6 (first by id) and one on 26.0: only the second may be booked.
#[test]
fn only_a_device_satisfying_every_key_is_booked_and_start_names_it() {
    let mut h = Harness::new();
    h.worker("a-split", mac(&[PHONE_18, PAD_26]));
    h.worker("b-both", mac(&[PHONE_18_C, PHONE_26]));
    h.submit(1, phone_26(1));
    let (grant, start) = h.one_start();
    assert_eq!(grant.worker, w("b-both"));
    assert_eq!(start.device.as_deref(), Some("dev-d"));
    assert_eq!(start.resources, Resources::new(1_000, GIB));

    // An action that asks for no device names none.
    h.submit(2, request(2, &[]));
    let (_, plain) = h.one_start();
    assert_eq!(plain.device, None);
}

/// Catches: a device lease placed on a node whose report has no `ios.device` entry. A
/// daemon that predates devices never reports one and drops `Start.device_id`, so it
/// would run the device test with no device: the Linux node (first by name) and the
/// device-less Mac must never be offered it, and it waits naming what it lacks until a
/// node with a device registers.
#[test]
fn a_daemon_that_reports_no_device_is_never_placed_a_device_lease() {
    let mut h = Harness::new();
    let linux = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container", "native"]);
    h.worker("a-linux", linux);
    h.worker("b-old-mac", mac(&[]));
    h.submit(1, any_device(1));
    assert!(h.grants().is_empty());
    let why = h.s.waiting(kbf_types::OperationId(0)).expect("a reason");
    assert!(why.contains("lacks ios.device=1"), "{why}");

    // A node with a device registers, and the lease goes there.
    h.worker("c-mac", mac(&[PHONE_26]));
    let grants = h.grants();
    assert_eq!(grants.len(), 1, "{grants:?}");
    assert_eq!(grants[0].worker, w("c-mac"));
}

/// Catches: a device request no node can serve waiting without naming the closest
/// device and the attribute it lacks. Both devices of the node lack one attribute, so
/// the first by id is named.
#[test]
fn the_reason_names_the_closest_device() {
    let mut h = Harness::new();
    h.worker("split", mac(&[PHONE_18, PAD_26]));
    h.submit(1, phone_26(1));
    assert!(h.grants().is_empty());
    let why = h.s.waiting(kbf_types::OperationId(0)).expect("a reason");
    assert!(
        why.ends_with(
            "the closest, split, lacks ios.device.os_version=26.0 (device dev-a has 18.6)"
        ),
        "{why}"
    );
}

/// Catches: a device booking ended before its lease is. While the lease runs, the
/// worker registers again (a new stream of the same daemon process), resends a report
/// without the device and then with it; none of that frees the device, so a second
/// device request must wait. Also catches a booking never freed: once the result is
/// accepted, the second request books the device.
#[test]
fn a_booking_lasts_until_its_lease_ends() {
    let mut h = Harness::new();
    h.worker("mac", mac(&[PHONE_26]));
    h.submit(1, any_device(1));
    let (first, start) = h.one_start();
    assert_eq!(start.device.as_deref(), Some("dev-d"));
    h.submit(2, any_device(2));
    assert!(h.grants().is_empty());

    h.worker("mac", mac(&[PHONE_26]));
    h.feed(Event::Heartbeat {
        worker: w("mac"),
        running: vec![first.lease],
    });
    assert!(h.grants().is_empty(), "a new session freed the device");
    for caps in [mac(&[]), mac(&[PHONE_26])] {
        h.feed(Event::Capacity {
            worker: w("mac"),
            capacity: Resources::new(8_000, 16 * GIB),
            caps,
        });
        assert!(h.grants().is_empty(), "a resent report freed the device");
    }

    h.finish(&first);
    let (_, second) = h.one_start();
    assert_eq!(second.device.as_deref(), Some("dev-d"));
}

/// Catches: a lease given up (a new session's heartbeat leaves it out) that keeps its
/// device booked, so the requeued action can never run on the node again.
#[test]
fn a_given_up_lease_frees_its_device() {
    let mut h = Harness::new();
    h.worker("mac", mac(&[PHONE_26]));
    h.submit(1, any_device(1));
    let (first, _) = h.one_start();
    h.worker("mac", mac(&[PHONE_26]));
    h.feed(Event::Heartbeat {
        worker: w("mac"),
        running: Vec::new(),
    });
    let (again, start) = h.one_start();
    assert_eq!(again.operation, first.operation);
    assert_ne!(again.lease, first.lease);
    assert_eq!(start.device.as_deref(), Some("dev-d"));
}

/// Catches: more than [`DEVICE_LEASES_PER_WORKER`] device leases on one Mac (until the
/// sandbox keeps a lease from another lease's device, two would each reach both), and
/// that limit applied to every lease rather than to device leases: an action that asks
/// for no device is still placed beside the device lease.
#[test]
fn one_device_lease_per_worker() {
    assert_eq!(DEVICE_LEASES_PER_WORKER, 1);
    let mut h = Harness::new();
    h.worker("mac", mac(&[PHONE_18, PHONE_26]));
    h.submit(1, any_device(1));
    h.submit(2, any_device(2));
    h.submit(3, request(3, &[]));
    let grants = h.grants();
    let placed: Vec<u64> = grants.iter().map(|g| g.operation.0).collect();
    assert_eq!(placed, [0, 2]);
}
