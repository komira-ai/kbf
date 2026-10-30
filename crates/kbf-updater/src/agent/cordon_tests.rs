//! Which cordons are the updater's own: kept on disk, so that a node it drained reads as in
//! placement on a later attempt and after a restart, and a node an operator drained does not.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::cordon::OwnCordon;
use super::flags::Timing;
use super::local::Trial;
use super::status::{Shared, Status};
use super::testing::{
    COMMIT_B, Events, FakeHealth, FakeLocal, ManualClock, ServingNode, VA, VB, host, version,
};
use super::update::{Ports, Update};
use crate::inventory::Role;
use crate::ports::{Drain, Health, Placed, PortError};
use crate::testutil::scratch;
use crate::wire::{Outcome, UpdateResult};
use kbf_types::ReleasePlatform;

const NOW: u64 = 10_000;

fn cordon(node: &ServingNode, dir: &Path) -> OwnCordon {
    OwnCordon::new(Arc::new(node.clone()), dir)
}

fn marks(dir: &Path) -> usize {
    std::fs::read_dir(dir).expect("dir").count()
}

fn result(target: &str, outcome: Outcome) -> UpdateResult {
    UpdateResult {
        target: version(target),
        outcome,
    }
}

/// A node the updater cordoned reads as in placement afterwards, to this process and to one
/// started later on the same directory; a node an operator cordoned reads as out, and a
/// drain of it by the updater leaves it so, with nothing remembered.
///
/// Catches: a node the updater drained being taken for an operator's on the next attempt or
/// after a restart, and an operator's node being taken for the updater's.
#[test]
fn the_updaters_own_cordon_reads_in_and_an_operators_reads_out() {
    let dir = scratch("cordon-own");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    let first = cordon(&node, &dir);
    assert_eq!(first.placed(&h).unwrap(), Placed::In);
    first.drain(&h, None).unwrap();
    assert!(!node.is_serving());
    assert_eq!(first.placed(&h).unwrap(), Placed::In, "a later attempt");
    assert_eq!(
        cordon(&node, &dir).placed(&h).unwrap(),
        Placed::In,
        "after a restart"
    );

    let dir = scratch("cordon-operator");
    let node = ServingNode::new();
    node.operator_drains();
    let w = cordon(&node, &dir);
    assert_eq!(w.placed(&h).unwrap(), Placed::Out);
    w.drain(&h, None).unwrap();
    assert_eq!(
        marks(&dir),
        0,
        "nothing is remembered of an operator's cordon"
    );
    assert_eq!(cordon(&node, &dir).placed(&h).unwrap(), Placed::Out);
}

/// The mark is on disk before the drain call reaches the frontend, and a drain call that
/// fails keeps it (the frontend may have applied it).
///
/// Catches: the mark written after the drain, where a crash between the two loses it.
#[test]
fn the_mark_is_written_before_the_drain_call_and_kept_when_it_fails() {
    let dir = scratch("cordon-order");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    *node.watch.lock().unwrap() = Some(dir.clone());
    *node.drain_error.lock().unwrap() = Some(PortError::other("503"));
    assert!(cordon(&node, &dir).drain(&h, None).is_err());
    assert_eq!(*node.seen_at_drain.lock().unwrap(), Some(true));
    assert_eq!(marks(&dir), 1);
}

/// A mark that cannot be written stops the drain: nothing is cordoned that could not be given
/// back later.
///
/// Catches: a drain made with no way to tell the cordon is ours.
#[test]
fn a_mark_that_cannot_be_written_stops_the_drain() {
    let dir = scratch("cordon-nodir").join("missing");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    assert!(cordon(&node, &dir).drain(&h, None).is_err());
    assert_eq!(*node.drain_calls.lock().unwrap(), 0);
    assert!(node.is_serving());
}

/// A good undrain clears the mark, so an operator's later drain reads as theirs; a failed
/// undrain keeps it, so the retry still gives the node back.
///
/// Catches: a mark that outlives the cordon (an operator's later drain uncordoned by the
/// next roll), and one cleared although the node is still drained.
#[test]
fn an_undrain_clears_the_mark_only_when_it_worked() {
    let dir = scratch("cordon-clear");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    let w = cordon(&node, &dir);
    w.drain(&h, None).unwrap();
    *node.undrain_error.lock().unwrap() = Some(PortError::other("503"));
    assert!(w.undrain(&h).is_err());
    assert_eq!(marks(&dir), 1);
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    *node.undrain_error.lock().unwrap() = None;
    w.undrain(&h).unwrap();
    assert!(node.is_serving());
    assert_eq!(marks(&dir), 0);
    node.operator_drains();
    assert_eq!(w.placed(&h).unwrap(), Placed::Out);
}

/// A host with no usable node id is the inner drain's to refuse: nothing is marked or read on
/// disk for it, and the calls pass through.
///
/// Catches: a mark path built from a missing or free-text node id.
#[test]
fn a_host_without_a_node_id_passes_through_with_no_mark() {
    let dir = scratch("cordon-noid");
    let mut h = host(Role::Daemon);
    h.node_id = None;
    let node = ServingNode::new();
    let w = cordon(&node, &dir);
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    w.drain(&h, None).unwrap();
    assert_eq!(marks(&dir), 0);
    assert_eq!(w.placed(&h).unwrap(), Placed::Out);
    assert_eq!(w.leases(&h).unwrap(), 0);
    w.undrain(&h).unwrap();
    assert!(node.is_serving());
}

/// An undrain with no mark to clear is fine (the node was never marked), and a mark that cannot
/// be read or cleared is an error, not a guess: a directory that is a file reads as an
/// error from every call that looks, and a mark that is a directory cannot be removed.
///
/// Catches: a missing mark failing the undrain, and an unreadable one read as "none", which
/// would take the updater's own cordon for an operator's.
#[test]
fn a_mark_that_cannot_be_read_or_cleared_is_an_error() {
    let h = host(Role::Daemon);
    let dir = scratch("cordon-nomark");
    let node = ServingNode::new();
    cordon(&node, &dir).undrain(&h).unwrap();

    let file = crate::testutil::file(&scratch("cordon-file"), "not-a-dir", "x", 0o600);
    let node = ServingNode::new();
    let w = cordon(&node, &file);
    assert!(w.placed(&h).is_err());
    assert!(w.drain(&h, None).is_err());
    assert_eq!(*node.drain_calls.lock().unwrap(), 0);
    assert!(w.undrain(&h).is_err());

    let dir = scratch("cordon-dirmark");
    std::fs::create_dir(dir.join("cordon-n1")).unwrap();
    let node = ServingNode::new();
    let w = cordon(&node, &dir);
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    assert!(w.undrain(&h).is_err());
}

struct Rig {
    events: Events,
    local: FakeLocal,
    health: FakeHealth,
    clock: Arc<ManualClock>,
    status: Shared,
    logs: Arc<Mutex<Vec<String>>>,
    node: ServingNode,
    dir: std::path::PathBuf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        Self {
            local: FakeLocal::new(&events),
            health,
            clock: Arc::new(ManualClock::default()),
            status: Shared::new(Status::unknown(ReleasePlatform::LinuxX86_64, version(VA))),
            logs: Arc::default(),
            node: ServingNode::new(),
            dir: scratch(name),
            events,
        }
    }

    /// One attempt by a freshly started updater on the same directory.
    fn run(&self, target: &str, deadline: Option<Duration>) -> UpdateResult {
        let h = host(Role::Daemon);
        let logs = Arc::clone(&self.logs);
        let log = move |line: &str| logs.lock().unwrap().push(line.to_owned());
        let unix_now = || NOW;
        let drain = cordon(&self.node, &self.dir);
        let update = Update {
            ports: Ports {
                local: &self.local,
                drain: &drain,
                health: &self.health,
                clock: &*self.clock,
                unix_now: &unix_now,
                log: &log,
            },
            host: &h,
            channel: "stable",
            timing: Timing {
                drain_deadline: deadline,
                ..Timing::DEFAULT
            },
            status: &self.status,
        };
        update.run(&version(target))
    }

    fn pending(&self) {
        let mut world = self.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(Trial {
            version: Some(version(VB)),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(NOW + 400),
        });
        world.before = Some(version(VA));
    }
}

/// The drain deadline passes with the node's leases still running: the update fails with the
/// node left drained. The next attempt, a new process, succeeds and gives the node back,
/// because the cordon was the updater's own.
///
/// Catches: a node the updater drained and failed on reading as an operator's on the retry,
/// which left it out of placement for good.
#[test]
fn a_node_the_updater_drained_and_failed_on_is_given_back_by_the_retry() {
    let rig = Rig::new("cordon-retry");
    *rig.node.leases.lock().unwrap() = 1;
    assert_eq!(
        rig.run(VB, Some(Duration::from_secs(10))),
        result(VB, Outcome::Failed)
    );
    assert!(!rig.node.is_serving(), "left drained");
    *rig.node.leases.lock().unwrap() = 0;
    assert_eq!(rig.run(VB, None), result(VB, Outcome::Succeeded));
    assert!(rig.node.is_serving(), "given back");
    assert_eq!(marks(&rig.dir), 0);
}

/// A node an operator drained is left out by a roll, and by its retry after a failure.
///
/// Catches: the retry or the roll uncordoning an operator's drain.
#[test]
fn a_node_an_operator_drained_stays_out_across_a_failed_attempt_and_its_retry() {
    let rig = Rig::new("cordon-op-retry");
    rig.node.operator_drains();
    *rig.node.leases.lock().unwrap() = 1;
    assert_eq!(
        rig.run(VB, Some(Duration::from_secs(10))),
        result(VB, Outcome::Failed)
    );
    *rig.node.leases.lock().unwrap() = 0;
    assert_eq!(rig.run(VB, None), result(VB, Outcome::Succeeded));
    assert!(!rig.node.is_serving());
}

/// An updater restarted with B on trial finishes the trial and gives back a node it drained
/// itself, and leaves out one an operator drained.
///
/// Catches: the resume taking every node for the updater's own (an operator's uncordoned),
/// and the resume taking every node for an operator's (the updater's left out for good).
#[test]
fn a_restart_in_the_middle_of_a_trial_tells_its_own_cordon_from_an_operators() {
    let rig = Rig::new("cordon-resume-own");
    cordon(&rig.node, &rig.dir)
        .drain(&host(Role::Daemon), None)
        .unwrap();
    rig.pending();
    assert_eq!(rig.run(VB, None), result(VB, Outcome::Succeeded));
    assert!(rig.node.is_serving());

    let rig = Rig::new("cordon-resume-operator");
    rig.node.operator_drains();
    rig.pending();
    assert_eq!(rig.run(VB, None), result(VB, Outcome::Succeeded));
    assert!(!rig.node.is_serving());
    assert_eq!(rig.events.count("local:install"), 0);
}
