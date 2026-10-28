//! The text the scheduler simulations write their traces in.
//!
//! The golden trace hashes (`tests/golden/trace-hashes.txt`) rest on these spellings:
//! a rename of a type, a field or a variant must not change one. Each test here writes
//! the expected text out in full, so a change to a spelling is a change to this file
//! and shows in review.

#[path = "trace_text/mod.rs"]
mod trace_text;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, NodePhase, Request};
use kbf_sim::Trace;
use kbf_types::{
    ControlRecord, Digest, DigestFunction, FarmTime, LeaseGrant, LeaseId, LeaseKind, OperationId,
    Outcome, Qos, Resources, WaiterId, WorkerId,
};

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], 5)
}

fn digest_text(n: u8) -> String {
    format!("sha256:{}:5", format!("{n:02x}").repeat(32))
}

fn worker() -> WorkerId {
    WorkerId::new("w1")
}

fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "arm64"), ("os", "linux")])
        .expect("a valid report")
        .with_drivers(["container"])
}

fn request(cpu_millis: u64) -> Request {
    Request {
        key: kbf_sim::work_key("main", digest(1)),
        qos: Qos::Ci,
        kind: LeaseKind::WholeMachine,
        resources: Resources::new(cpu_millis, 2),
        needs: kbf_caps::Request::parse([("arch", "arm64")]).expect("a valid request"),
        hermetic: true,
        do_not_cache: false,
    }
}

fn lease() -> LeaseId {
    LeaseId { term: 1, seq: 4 }
}

const CAPS_TEXT: &str = "caps(arch=arm64 level=- features=[] exact={\"os\"=\"linux\"} members={} \
                         consumables={} drivers=[\"container\"] devices={})";

/// Catches: a node's capabilities written with a part left out or in another order, or
/// through `Debug`.
#[test]
fn capabilities_are_spelled_part_by_part() {
    assert_eq!(trace_text::caps(&caps()), CAPS_TEXT);
}

/// Catches: a request written with a field left out (two runs that differ only in that
/// field would share a hash).
#[test]
fn a_request_is_spelled_field_by_field() {
    assert_eq!(
        trace_text::request(&request(1_000)),
        format!(
            "request(instance=\"main\" action={} qos=ci/200 kind=whole_machine \
             res(cpu=1000 mem=2 gpus=0 vms=0 leases=-) needs=[\"arch\"=\"arm64\"] \
             hermetic=true do_not_cache=false)",
            digest_text(1)
        )
    );
}

/// Catches: an input event missing from the text, two events sharing a spelling, or a
/// field dropped from a line. Each expected line is written out in full.
#[test]
fn every_event_has_its_fixed_spelling() {
    let grant = ControlRecord::Lease(LeaseGrant {
        lease: lease(),
        operation: OperationId(7),
        worker: worker(),
    });
    let resources = "res(cpu=4000 mem=8 gpus=0 vms=0 leases=-)";
    let cases: Vec<(Event, String)> = vec![
        (
            Event::WorkerUp {
                worker: worker(),
                instance: DaemonInstance::new("d1"),
                predecessor_stopped: true,
                capacity: Resources::new(4_000, 8),
                caps: caps(),
            },
            format!(
                "worker_up(worker=w1 instance=\"d1\" predecessor_stopped=true \
                 capacity={resources} {CAPS_TEXT})"
            ),
        ),
        (
            Event::Capacity {
                worker: worker(),
                capacity: Resources::new(4_000, 8),
                caps: caps(),
            },
            format!("capacity(worker=w1 capacity={resources} {CAPS_TEXT})"),
        ),
        (
            Event::Heartbeat {
                worker: worker(),
                running: vec![lease(), LeaseId { term: 1, seq: 5 }],
            },
            "heartbeat(worker=w1 running=[1.4; 1.5])".to_owned(),
        ),
        (
            Event::Phase {
                worker: worker(),
                phase: NodePhase::Preparing(lease()),
            },
            "phase(worker=w1 preparing(1.4))".to_owned(),
        ),
        (
            Event::Submit {
                waiter: WaiterId(3),
                request: request(1_000),
            },
            format!("submit(waiter=w3 {})", trace_text::request(&request(1_000))),
        ),
        (
            Event::Committed(grant),
            "committed(lease(1.4 op7 w1))".to_owned(),
        ),
        (
            Event::Started {
                operation: OperationId(7),
                lease: lease(),
            },
            "started(op=op7 lease=1.4)".to_owned(),
        ),
        (
            Event::Report {
                operation: OperationId(7),
                lease: lease(),
                outcome: Outcome::Completed {
                    action_result: digest(2),
                },
            },
            format!("report(op=op7 lease=1.4 outcome=done({}))", digest_text(2)),
        ),
        (Event::Tick, "tick".to_owned()),
        (
            Event::Cordon { worker: worker() },
            "cordon(worker=w1)".to_owned(),
        ),
        (
            Event::Drain {
                worker: worker(),
                deadline: FarmTime::from_millis(9_000),
            },
            "drain(worker=w1 deadline=9000)".to_owned(),
        ),
        (
            Event::Uncordon { worker: worker() },
            "uncordon(worker=w1)".to_owned(),
        ),
    ];
    for (event, expected) in &cases {
        assert_eq!(&trace_text::event(event), expected);
    }
}

/// Catches: every phase spelled the same, or a phase's lease or reason dropped.
#[test]
fn every_phase_has_its_fixed_spelling() {
    let phase = |phase| {
        trace_text::event(&Event::Phase {
            worker: worker(),
            phase,
        })
    };
    assert_eq!(phase(NodePhase::Serving), "phase(worker=w1 serving)");
    assert_eq!(
        phase(NodePhase::Restoring(lease())),
        "phase(worker=w1 restoring(1.4))"
    );
    assert_eq!(
        phase(NodePhase::Quarantined("bad disk".to_owned())),
        "phase(worker=w1 quarantined(\"bad disk\"))"
    );
    assert_eq!(
        phase(NodePhase::MemoryPressure),
        "phase(worker=w1 memory_pressure)"
    );
    assert_eq!(phase(NodePhase::Draining), "phase(worker=w1 draining)");
}

/// Catches: a trace hash that does not move when one field of one input moves (a hash
/// that cannot tell two runs apart is no golden): the same events hash alike, and
/// changing one resource of one request changes the hash.
#[test]
fn the_hash_changes_when_one_trace_line_changes() {
    let hash_of = |cpu_millis| {
        let mut trace = Trace::new("test");
        trace.record(&format!("0 in {}", trace_text::event(&Event::Tick)));
        let submit = Event::Submit {
            waiter: WaiterId(1),
            request: request(cpu_millis),
        };
        trace.record(&format!("1 in {}", trace_text::event(&submit)));
        trace.hash()
    };
    assert_eq!(hash_of(1_000), hash_of(1_000));
    assert_ne!(hash_of(1_000), hash_of(1_001));
}
