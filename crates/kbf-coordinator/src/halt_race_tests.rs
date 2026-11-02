//! A halt, a halt report and a clear are each decided on the state they change, and a halt is
//! raised only under the lock every decision to order is made under.
//!
//! Each race is made to happen, not waited for. A store's `interleave` puts another writer's
//! change in at the start of the next change (under the writers' lock, after the caller read
//! the state it acts on): what a clear or a heartbeat on another thread would have done in
//! that window. Every time is explicit; nothing sleeps.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use kbf_types::HostRole;
use kbf_updater::wire::{Answer, Heartbeat, UpdateResult};
use serde_json::json;

use crate::desired::{DesiredStore, KBF};
use crate::identity::Identity;
use crate::ops::{Halt, HaltAck, OpsError, OpsState, OpsStore, ServiceOps};
use crate::service::{Coordinator, FEATURES};
use crate::testutil::{OPERATOR, Rig, beat, clock_of, id, ops_of};

/// The update to 2.0 that `node`'s hook halted, for the reason `check`.
fn halted_update(check: &str) -> UpdateResult {
    serde_json::from_value(json!({
        "target": "2.0", "outcome": "failed", "phase": "halt", "reason": check
    }))
    .expect("a report")
}

/// The heartbeat of a storage host at 1.0 whose update to 2.0 halted.
fn halting() -> Heartbeat {
    let mut b = beat("1.0");
    b.last_update = Some(halted_update("disk_tainted"));
    b
}

fn storage(name: &str) -> Identity {
    Identity {
        node_id: id(name),
        role: HostRole::STORAGE,
    }
}

/// The halt `node` raised with [`halting`].
fn halt_of(node: &str) -> Halt {
    Halt {
        node_id: node.to_owned(),
        check: "disk_tainted".to_owned(),
        target: crate::testutil::version("2.0"),
        report: Some(halted_update("disk_tainted")),
    }
}

fn kbf(rig: &Rig) -> ServiceOps {
    rig.coordinator.ops().marks(KBF)
}

fn clear(rig: &Rig) -> Result<crate::ops::AuditEntry, crate::ops::OperateError> {
    rig.coordinator.operate(
        Some(OPERATOR),
        br#"{"action":"clear_halt","service":"kbf","reason":"disk replaced"}"#,
    )
}

/// A rig past the quiet start.
fn rig(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.at(1_100);
    rig
}

/// Catches: a halt noted before the fleet's lock is taken. Every decision to order and the
/// order itself are made under that lock; a halt raised outside it lands between another
/// heartbeat's decision and its order, and that host is ordered an update of a service that is
/// halted already. Every write of a halt, and of a report kept while it stands, is made with the
/// lock held.
#[test]
fn a_halt_is_raised_only_under_the_lock_every_order_is_decided_under() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "halt-race-lock");
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(1_100));
    let me: Arc<OnceLock<Weak<Coordinator>>> = Arc::new(OnceLock::new());
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::default();
    let (probe, log) = (Arc::clone(&me), Arc::clone(&seen));
    let writer = Box::new(move |state: &OpsState| {
        let action = state
            .audit
            .last()
            .map(|e| e.action.clone())
            .unwrap_or_default();
        if action.starts_with("halt") {
            let coordinator = probe
                .get()
                .and_then(Weak::upgrade)
                .expect("the coordinator");
            log.lock()
                .expect("log")
                .push((action, coordinator.fleet_held()));
        }
        Ok::<(), OpsError>(())
    });
    let store = DesiredStore::open(&dir.path().join("desired.json")).expect("a store");
    let (verifier, _) = kbf_oidc::testkit::static_verifier();
    let coordinator = Arc::new(
        Coordinator::new(store, verifier, OPERATOR.to_owned(), clock_of(&clock))
            .with_ops(OpsStore::with_writer(OpsState::default(), writer)),
    );
    me.set(Arc::downgrade(&coordinator)).expect("set once");
    coordinator.heartbeat(storage("s1"), halting());
    coordinator.heartbeat(storage("s2"), halting());
    assert_eq!(
        *seen.lock().expect("log"),
        [("halt".to_owned(), true), ("halt_report".to_owned(), true)]
    );
}

/// Catches: a halt report decided on a state read before another host's halt was raised. Two
/// hosts report their first halt at once; both read no halt standing, the first raises its
/// own, and the second, still taking itself for the first, is dropped instead of kept for the
/// clear. The clear then acknowledges the first host alone, and the second host's next report
/// halts the service again though the operator cleared it.
#[test]
fn a_report_that_finds_another_hosts_halt_raised_under_it_is_kept_for_the_clear() {
    let rig = rig("halt-race-report");
    rig.coordinator.ops().interleave(|state| {
        state.services.entry(KBF.to_owned()).or_default().halt = Some(halt_of("s1"));
    });
    rig.coordinator.heartbeat(storage("s2"), halting());
    let marks = kbf(&rig);
    assert_eq!(marks.halt.map(|h| h.node_id), Some("s1".to_owned()));
    assert_eq!(marks.halt_seen, [HaltAck::from(halt_of("s2"))]);
    clear(&rig).expect("cleared");
    rig.coordinator.heartbeat(storage("s1"), halting());
    rig.coordinator.heartbeat(storage("s2"), halting());
    assert_eq!(kbf(&rig).halt, None, "a cleared report halted again");
}

/// Catches: a clear that takes the halt it clears from a read made before its change. Two
/// operators clear the halt of `s1` at once; the first clears it, and `s2` halts the service
/// again before the second clear is written. The second clear then lifts `s2`'s halt with an
/// acknowledgement of `s1`'s old report, and `s2`, which keeps reporting, halts it once more.
/// The clear is of the halt standing when it is written: it names `s2`, acknowledges `s2`'s
/// report and counts one more clear.
#[test]
fn a_clear_clears_the_halt_standing_when_it_is_written_not_one_read_before() {
    let rig = rig("halt-race-clear-new");
    rig.coordinator.heartbeat(storage("s1"), halting());
    rig.coordinator.ops().interleave(|state| {
        let marks = state.services.entry(KBF.to_owned()).or_default();
        marks.halt_acked = vec![HaltAck::from(halt_of("s1"))];
        marks.halt_generation = 1;
        marks.halt = Some(halt_of("s2"));
    });
    let entry = clear(&rig).expect("cleared");
    assert_eq!(entry.node_id.as_deref(), Some("s2"));
    let marks = kbf(&rig);
    assert_eq!(marks.halt, None);
    assert_eq!(marks.halt_generation, 2);
    assert!(
        marks.halt_acked.contains(&HaltAck::from(halt_of("s2"))),
        "{:?}",
        marks.halt_acked
    );
    rig.coordinator.heartbeat(storage("s2"), halting());
    assert_eq!(kbf(&rig).halt, None, "the cleared report halted again");
}

/// Catches: the same race when nothing halted the service again: the second clear finds no
/// halt and is refused, as a clear of a service that is not halted is, instead of counting a
/// clear that cleared nothing (agents would be told a generation no halt had).
#[test]
fn a_clear_that_finds_the_halt_cleared_under_it_is_refused_and_counts_nothing() {
    let rig = rig("halt-race-clear-gone");
    rig.coordinator.heartbeat(storage("s1"), halting());
    rig.coordinator.ops().interleave(|state| {
        let marks = state.services.entry(KBF.to_owned()).or_default();
        marks.halt = None;
        marks.halt_acked = vec![HaltAck::from(halt_of("s1"))];
        marks.halt_generation = 1;
    });
    let refused = clear(&rig).expect_err("nothing to clear");
    assert_eq!(refused.to_string(), "the service is not halted");
    assert_eq!(kbf(&rig).halt_generation, 1);
    let clears = rig.coordinator.ops().read(|state| {
        state
            .audit
            .iter()
            .filter(|entry| entry.action == "clear_halt")
            .count()
    });
    assert_eq!(clears, 0);
}

/// Catches: the generations of every service the ops file ever counted being told. A service
/// that is no longer defined keeps its generation in the file, and an agent refuses an answer
/// with more generations than a services file holds (the whole answer, its order with it), so a
/// farm whose services came and went would stop every agent that announced `halt-v1`. The
/// answer names the generations of the defined services only, and reads; the file keeps the
/// rest.
#[test]
fn only_the_defined_services_generations_are_told_and_the_answer_reads() {
    let mut rig = rig("halt-race-generations");
    ops_of(&rig.dir)
        .change(
            crate::ops::Write::BeforeMemory,
            crate::ops::AuditEntry::new("operator", "seed"),
            |state| {
                for n in 0..kbf_updater::wire::MAX_SERVICES + 2 {
                    state
                        .services
                        .entry(format!("gone-{n}"))
                        .or_default()
                        .halt_generation = 5;
                }
                state
                    .services
                    .entry(KBF.to_owned())
                    .or_default()
                    .halt_generation = 1;
                state
                    .services
                    .entry("other".to_owned())
                    .or_default()
                    .halt_generation = 3;
            },
        )
        .expect("seeded");
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_services(
        crate::services::Services::parse(
            "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n",
        )
        .expect("services"),
    );
    let mut announce = beat("1.0");
    announce.agent_features = FEATURES.iter().map(|f| (*f).to_owned()).collect();
    let answer = rig.coordinator.heartbeat(storage("s1"), announce);
    let bytes = serde_json::to_vec(&answer).expect("bytes");
    let read: Answer = serde_json::from_slice(&bytes).expect("an agent reads the answer");
    assert_eq!(
        read.halts,
        BTreeMap::from([(KBF.to_owned(), 1), ("other".to_owned(), 3)])
    );
    let kept = rig
        .coordinator
        .ops()
        .read(|state| state.services["gone-0"].halt_generation);
    assert_eq!(kept, 5);
}

/// Catches: the end of an acknowledgement decided on a read made before another writer ended
/// it: the host reports something else, and while its change waits the acknowledgement is gone
/// already. Nothing is ended twice: no `halt_ack_ended` is audited for it.
#[test]
fn an_acknowledgement_another_writer_ended_first_is_not_ended_again() {
    let rig = rig("halt-race-ack-gone");
    rig.coordinator.heartbeat(storage("s1"), halting());
    clear(&rig).expect("cleared");
    assert_eq!(kbf(&rig).halt_acked, [HaltAck::from(halt_of("s1"))]);
    rig.coordinator.ops().interleave(|state| {
        let marks = state.services.entry(KBF.to_owned()).or_default();
        marks.halt_acked.clear();
    });
    rig.coordinator.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(kbf(&rig).halt_acked, []);
    let ended = rig.coordinator.ops().read(|state| {
        state
            .audit
            .iter()
            .filter(|entry| entry.action == "halt_ack_ended")
            .count()
    });
    assert_eq!(ended, 0);
}
