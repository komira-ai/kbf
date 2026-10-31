//! Which cordons are the updater's own: kept on disk, so that a node it drained reads as in
//! placement on a later attempt and after a restart, and a node an operator drained does not.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::cordon::OwnCordon;
use super::flags::Timing;
use super::local::Trial;
use super::run::{Agent, World};
use super::status::{Shared, Status};
use super::testing::{
    COMMIT_B, Events, FakeCoordinator, FakeHealth, FakeLocal, ManualClock, ServingNode, VA, VB,
    host, version,
};
use super::update::{Ports, Update};
use crate::inventory::Role;
use crate::ports::{Drain, Health, Placed, PortError};
use crate::testutil::scratch;
use crate::wire::{Outcome, UpdateResult};
use kbf_types::ReleasePlatform;

const NOW: u64 = 10_000;

fn cordon(node: &ServingNode, dir: &Path) -> OwnCordon {
    unit_cordon(node, dir, "kbf")
}

fn unit_cordon(node: &ServingNode, dir: &Path, unit: &str) -> OwnCordon {
    OwnCordon::new(Arc::new(node.clone()), dir, unit)
}

fn marks(dir: &Path) -> usize {
    std::fs::read_dir(dir).expect("dir").count()
}

fn result(target: &str, outcome: Outcome) -> UpdateResult {
    UpdateResult::new(version(target), outcome)
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
    std::fs::create_dir(dir.join("cordon-n1.kbf")).unwrap();
    let node = ServingNode::new();
    node.operator_drains();
    let w = cordon(&node, &dir);
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    assert!(w.undrain(&h).is_err());
    // A mark that cannot be removed cannot be dropped for a serving node either.
    node.operator_uncordons();
    assert!(w.placed(&h).is_err());
}

/// An agent whose refresh cannot look at its marks says so in its log and goes on.
///
/// Catches: a failed look hidden from the operator, or one that stops the refresh.
#[test]
fn a_refresh_that_cannot_look_at_the_marks_logs_it() {
    let events = Events::default();
    let health = FakeHealth::new(&events);
    health.say(VA, vec![Ok(Health::Healthy)]);
    let file = crate::testutil::file(&scratch("cordon-refresh-file"), "not-a-dir", "x", 0o600);
    let logs: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = Arc::clone(&logs);
    let agent = Agent::new(
        host(Role::Daemon),
        "stable".to_owned(),
        Timing::DEFAULT,
        World {
            coordinator: Arc::new(FakeCoordinator::default()),
            local: Arc::new(FakeLocal::new(&events)),
            drain: Arc::new(cordon(&ServingNode::new(), &file)),
            health: Arc::new(health),
            clock: Arc::new(ManualClock::default()),
            unix_now: Arc::new(|| NOW),
            renewer: None,
            log: Arc::new(move |line| sink.lock().unwrap().push(line.to_owned())),
        },
        Status::unknown(ReleasePlatform::LinuxX86_64, version(VA)),
    );
    agent.refresh();
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("could not be looked at")),
        "{:?}",
        logs.lock().unwrap()
    );
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

/// A cordon given back by hand (the documented way, `:uncordon` or `kbf cluster`) leaves the
/// updater's mark behind; the next time the node is read serving the mark is dropped, so an
/// operator's later drain reads as theirs. A node still cordoned keeps its mark.
///
/// Catches: a mark that outlives its cordon, which made the next roll uncordon a maintenance
/// drain of an operator's.
#[test]
fn a_mark_is_dropped_when_the_node_is_read_serving_and_kept_while_it_is_cordoned() {
    let dir = scratch("cordon-stale");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    let w = cordon(&node, &dir);
    w.drain(&h, None).unwrap();
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    assert_eq!(marks(&dir), 1, "cordoned: still ours");

    node.operator_uncordons();
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    assert_eq!(marks(&dir), 0, "given back by hand: the mark is gone");
    node.operator_drains();
    assert_eq!(
        w.placed(&h).unwrap(),
        Placed::Out,
        "a later maintenance drain"
    );
    assert_eq!(
        cordon(&node, &dir).placed(&h).unwrap(),
        Placed::Out,
        "also to a restarted agent"
    );
}

/// `observe` (what the agent calls when it looks at its node outside an update) drops the mark
/// of a node read serving, keeps that of one still cordoned, and does nothing with no mark.
///
/// Catches: a stale mark that only an update's own read could clear.
#[test]
fn observing_a_node_drops_the_mark_only_once_the_node_serves() {
    let dir = scratch("cordon-observe");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    let w = cordon(&node, &dir);
    w.observe(&h).unwrap();
    w.drain(&h, None).unwrap();
    w.observe(&h).unwrap();
    assert_eq!(marks(&dir), 1);
    node.operator_uncordons();
    w.observe(&h).unwrap();
    assert_eq!(marks(&dir), 0);
}

/// The agent's idle refresh looks at the node: the mark of a cordon given back by hand is gone
/// after it, with no update and no order.
///
/// Catches: the observation not being wired into the agent's loop.
#[test]
fn the_agents_refresh_drops_the_mark_of_a_cordon_given_back_by_hand() {
    let events = Events::default();
    let health = FakeHealth::new(&events);
    health.say(VA, vec![Ok(Health::Healthy)]);
    let local = FakeLocal::new(&events);
    let dir = scratch("cordon-refresh");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    let own = Arc::new(cordon(&node, &dir));
    own.drain(&h, None).unwrap();
    let agent = Agent::new(
        h,
        "stable".to_owned(),
        Timing::DEFAULT,
        World {
            coordinator: Arc::new(FakeCoordinator::default()),
            local: Arc::new(local),
            drain: own,
            health: Arc::new(health),
            clock: Arc::new(ManualClock::default()),
            unix_now: Arc::new(|| NOW),
            renewer: None,
            log: Arc::new(|_| {}),
        },
        Status::unknown(ReleasePlatform::LinuxX86_64, version(VA)),
    );
    agent.refresh();
    assert_eq!(marks(&dir), 1, "still cordoned");
    node.operator_uncordons();
    agent.refresh();
    assert_eq!(marks(&dir), 0);
}

/// A directory the agent cannot write is found out at start, by a name that says which flag to
/// set, and the probe leaves nothing behind.
///
/// Catches: every drain refused at the first update because the default directory is read-only.
#[test]
fn an_unwritable_cordon_directory_is_refused_at_start_naming_the_flag() {
    let node = ServingNode::new();
    let dir = scratch("cordon-probe");
    cordon(&node, &dir).check_writable().unwrap();
    assert_eq!(marks(&dir), 0);

    let missing = dir.join("missing");
    let why = cordon(&node, &missing).check_writable().unwrap_err();
    assert!(why.contains("--cordon-dir"), "{why}");
    assert!(why.contains(&missing.display().to_string()), "{why}");

    let file = crate::testutil::file(&dir, "not-a-dir", "x", 0o600);
    assert!(cordon(&node, &file).check_writable().is_err());
}

/// A host the frontend knows no node for is read in placement and drained as nothing, and no
/// mark is made for the node that does not exist; once a node appears and an operator drains
/// it, it is the operator's.
///
/// Catches: a mark for an absent node, which a daemon installed later and drained by an
/// operator would then be taken to be the updater's cordon for.
#[test]
fn a_host_with_no_node_gets_no_mark() {
    let dir = scratch("cordon-absent");
    let h = host(Role::Storage);
    let node = ServingNode::new();
    node.no_node();
    let w = cordon(&node, &dir);
    assert_eq!(w.placed(&h).unwrap(), Placed::In);
    w.drain(&h, None).unwrap();
    assert_eq!(*node.drain_calls.lock().unwrap(), 1);
    assert_eq!(marks(&dir), 0);
    w.undrain(&h).unwrap();

    *node.absent.lock().unwrap() = false;
    node.operator_drains();
    assert_eq!(cordon(&node, &dir).placed(&h).unwrap(), Placed::Out);
}

/// The kbf update and a service's update each keep a mark of their own. The node is the
/// updater's while either is there; one unit's undrain clears only its own mark and gives the
/// node back only when no other mark remains.
///
/// Catches: one unit's undrain clearing the other's mark and uncordoning a node the other is
/// still rolling on.
#[test]
fn each_unit_has_its_own_mark_and_the_node_is_given_back_with_the_last() {
    let dir = scratch("cordon-units");
    let h = host(Role::Daemon);
    let node = ServingNode::new();
    let kbf = unit_cordon(&node, &dir, "kbf");
    let db = unit_cordon(&node, &dir, "db");
    kbf.drain(&h, None).unwrap();
    db.drain(&h, None).unwrap();
    assert_eq!(marks(&dir), 2, "one mark each");
    assert_eq!(
        db.placed(&h).unwrap(),
        Placed::In,
        "ours, whoever marked it"
    );

    kbf.undrain(&h).unwrap();
    assert!(!node.is_serving(), "the other unit's mark remains");
    assert_eq!(marks(&dir), 1);
    assert_eq!(kbf.placed(&h).unwrap(), Placed::In);

    db.undrain(&h).unwrap();
    assert!(node.is_serving());
    assert_eq!(marks(&dir), 0);
}
