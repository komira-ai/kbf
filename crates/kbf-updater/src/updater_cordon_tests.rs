//! Which cordon the SSH-driven roll gives back: only a node the roll drained. An operator's
//! cordon is left out after the roll, on this attempt and on a retry; the roll's own is given
//! back after a failed attempt and a retry and after a restart.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::cordon::OwnCordon;
use crate::agent::testing::ServingNode;
use crate::api::{ErrorCode, Hosts, Phase, Roll, RollState};
use crate::fakes::Event;
use crate::inventory::Host;
use crate::ports::{Drain, Health, Placed, PortError};
use crate::updater_restart_tests::record;
use crate::updater_tests::{Rig, inventory, request};
use crate::{Timing, Updater};

fn roll_dm(u: &Updater, key: &str) -> Roll {
    let hosts = Hosts::Aliases(vec!["dm".into(), "st2".into()]);
    let started = u.start(&request("2.0", hosts, key)).expect("start");
    u.drive(&started.roll.id).expect("drive")
}

fn dm_words(rig: &Rig) -> Vec<String> {
    rig.log
        .touching("dm")
        .into_iter()
        .map(|e| match e {
            Event::Install(..) => "install".to_owned(),
            other => other.word(),
        })
        .collect()
}

/// An updater whose drain port is `drain`, over the rig's disk and hosts.
fn open_with(rig: &Rig, drain: Box<dyn Drain>) -> Updater {
    let mut ports = rig.ports();
    ports.drain = drain;
    Updater::open(inventory(), rig.store(), ports, Timing::default()).expect("open")
}

/// The roll's drain port as the daemon builds it: marks in the rig's state directory, over a
/// node that remembers whether it serves. A new one is a process that started again.
fn own(rig: &Rig, node: &ServingNode) -> Box<dyn Drain> {
    Box::new(OwnCordon::new(Arc::new(node.clone()), &rig.dir, "roll"))
}

fn marks(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("cordon-"))
        })
        .count()
}

/// A node the roll left out is never serving: the host is healthy only when it is not asked to
/// be serving (the held check), and a check that asks for it never says so.
fn serving_never_answers(rig: &Rig) {
    rig.health.script_held("dm", vec![], Ok(Health::Healthy));
    rig.health.script(
        "dm",
        vec![],
        Ok(Health::Waiting("dm is cordoned, not serving".to_owned())),
    );
}

/// A node an operator had drained before the roll is not undrained when the host runs the
/// target, and the host is then not asked to be serving.
///
/// Catches: the roll uncordoning an operator's drain, or waiting for a node it left out to
/// serve.
#[test]
fn an_operator_drained_node_is_left_out_after_the_roll_confirms() {
    let rig = Rig::new("cordon-op-confirm");
    rig.drain.operator_drained("dm");
    serving_never_answers(&rig);
    let roll = roll_dm(&rig.open(), "k");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    let words = dm_words(&rig);
    assert!(words.contains(&"drain none".to_owned()), "{words:?}");
    assert!(!words.contains(&"undrain".to_owned()), "{words:?}");
}

/// The same with a node that keeps its state and the daemon's drain port: the operator's
/// cordon is still on it afterwards, and nothing is marked.
///
/// Catches: a node an operator drained being given back by the real port.
#[test]
fn an_operator_drained_node_stays_cordoned_through_the_daemons_drain_port() {
    let rig = Rig::new("cordon-op-real");
    let node = ServingNode::new();
    node.operator_drains();
    serving_never_answers(&rig);
    let roll = roll_dm(&open_with(&rig, own(&rig, &node)), "k");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(!node.is_serving());
    assert_eq!(marks(&rig.dir), 0);
}

/// A roll that fails after the drain and a retry of it both leave an operator's cordon out:
/// the failed attempt names no node as left drained by it, and the retry that succeeds does
/// not give the node back.
///
/// Catches: a retry mistaking the node's cordon for its own.
#[test]
fn an_operator_drained_node_is_left_out_after_a_failed_attempt_and_a_retry() {
    let rig = Rig::new("cordon-op-retry");
    rig.drain.operator_drained("dm");
    serving_never_answers(&rig);
    rig.inst
        .script_install("dm", vec![Err(PortError("dpkg: lock held".into()))], Ok(()));
    let u = rig.open();
    let failed = roll_dm(&u, "k1");
    assert_eq!(failed.state, RollState::Failed);
    let retry = roll_dm(&u, "k2");
    assert_eq!(retry.state, RollState::Done, "{:?}", retry.failure);
    assert!(!dm_words(&rig).contains(&"undrain".to_owned()));
}

/// A node the roll drained is given back, with its mark cleared.
///
/// Catches: the give-back lost for the node the roll itself cordoned.
#[test]
fn a_node_the_roll_drained_is_given_back() {
    let rig = Rig::new("cordon-own");
    let node = ServingNode::new();
    let roll = roll_dm(&open_with(&rig, own(&rig, &node)), "k");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(node.is_serving());
    assert_eq!(marks(&rig.dir), 0);
}

/// An attempt that fails after the drain leaves the node drained and marked, and the retry
/// reads the node as its own, gives it back and clears the mark.
///
/// Catches: a retry that takes its own earlier cordon for an operator's and leaves the node
/// out for good.
#[test]
fn a_node_the_roll_drained_is_given_back_after_a_failed_attempt_and_a_retry() {
    let rig = Rig::new("cordon-own-retry");
    let node = ServingNode::new();
    rig.inst
        .script_install("dm", vec![Err(PortError("dpkg: lock held".into()))], Ok(()));
    let u = open_with(&rig, own(&rig, &node));
    let failed = roll_dm(&u, "k1");
    assert_eq!(failed.state, RollState::Failed);
    assert!(!node.is_serving());
    assert_eq!(marks(&rig.dir), 1);
    // The updater is started again before the retry.
    let u = open_with(&rig, own(&rig, &node));
    let retry = roll_dm(&u, "k2");
    assert_eq!(retry.state, RollState::Done, "{:?}", retry.failure);
    assert!(node.is_serving());
    assert_eq!(marks(&rig.dir), 0);
}

/// A roll found waiting for its host after a restart reads its own mark and gives the node
/// back; the same restart over a node with no mark leaves it out.
///
/// Catches: a resume that decides from nothing, giving back an operator's cordon or keeping
/// the roll's own.
#[test]
fn a_resumed_roll_reads_its_own_record() {
    use Phase::{Done, Pending, WaitingHealthy};
    let phases = [(Done, 1), (Done, 1), (WaitingHealthy, 1), (Pending, 0)];

    let rig = Rig::new("cordon-resume-own");
    let node = ServingNode::new();
    // The process before the restart drained the node and marked it.
    let before = own(&rig, &node);
    before.drain(&host(&rig, "dm"), None).expect("drain");
    assert!(!node.is_serving());
    rig.store()
        .save(&record(phases, RollState::Running))
        .expect("save");
    let u = open_with(&rig, own(&rig, &node));
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(node.is_serving());
    assert_eq!(marks(&rig.dir), 0);

    let rig = Rig::new("cordon-resume-operator");
    let node = ServingNode::new();
    node.operator_drains();
    serving_never_answers(&rig);
    rig.store()
        .save(&record(phases, RollState::Running))
        .expect("save");
    let u = open_with(&rig, own(&rig, &node));
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(!node.is_serving());
}

fn host(_rig: &Rig, alias: &str) -> Host {
    inventory().host(alias).expect("host").clone()
}

/// A drain that has an operator cordon the node just before the call reaches the port: what a
/// cordon made between the roll's read and its drain looks like.
struct OperatorFirst {
    node: ServingNode,
    port: Box<dyn Drain>,
}

impl Drain for OperatorFirst {
    fn placed(&self, host: &Host) -> Result<Placed, PortError> {
        self.port.placed(host)
    }

    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError> {
        self.node.operator_drains();
        self.port.drain(host, deadline)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.port.leases(host)
    }

    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        self.port.undrain(host)
    }

    fn placed_if_known(&self, host: &Host) -> Result<Option<Placed>, PortError> {
        self.port.placed_if_known(host)
    }
}

/// A mark whose node is serving again (given back by hand after a failed roll) means nothing:
/// the next roll drops it when it reads the node, so an operator's cordon made after it is not
/// taken for the roll's.
///
/// Catches: a mark that outlives its cordon, so that a later operator drain is undone by the
/// next roll.
#[test]
fn a_stale_mark_is_dropped_when_the_node_is_seen_serving() {
    let rig = Rig::new("cordon-stale");
    let node = ServingNode::new();
    rig.inst
        .script_install("dm", vec![Err(PortError("dpkg: lock held".into()))], Ok(()));
    let failed = roll_dm(&open_with(&rig, own(&rig, &node)), "k1");
    assert_eq!(failed.state, RollState::Failed);
    assert_eq!(marks(&rig.dir), 1);
    // An operator gives the node back by hand.
    node.operator_uncordons();
    // The next roll reads it serving; an operator cordons it before the roll's drain call.
    serving_never_answers(&rig);
    let racing = Box::new(OperatorFirst {
        node: node.clone(),
        port: own(&rig, &node),
    });
    let retry = roll_dm(&open_with(&rig, racing), "k2");
    assert_eq!(retry.state, RollState::Done, "{:?}", retry.failure);
    assert!(!node.is_serving(), "the operator's cordon was undone");
    assert_eq!(marks(&rig.dir), 0);
}

/// A node whose placement cannot be read is not drained: the roll stops with `drain_failed`
/// before any cordon is made, and the failure does not say the node is left drained.
///
/// Catches: a drain made with no way to tell afterwards whether the node was the roll's.
#[test]
fn a_node_whose_placement_cannot_be_read_is_not_drained() {
    let rig = Rig::new("cordon-unreadable");
    rig.drain.script_placed(
        "dm",
        vec![],
        Err(PortError::unreachable("the operator API")),
    );
    let roll = roll_dm(&rig.open(), "k");
    assert_eq!(roll.state, RollState::Failed);
    let f = roll.failure.expect("failure");
    assert_eq!(f.code, ErrorCode::DrainFailed);
    assert!(
        f.detail.contains("placement could not be read"),
        "{}",
        f.detail
    );
    assert!(!f.detail.contains("left drained"), "{}", f.detail);
    let words = dm_words(&rig);
    assert!(!words.iter().any(|w| w.starts_with("drain")), "{words:?}");
}

/// A placement that cannot be read when the roll is to give the node back leaves it drained,
/// named, and the roll fails with `undrain_failed`; nothing is undrained on a guess.
///
/// Catches: an undrain made without knowing whose cordon it is.
#[test]
fn a_placement_that_cannot_be_read_at_the_give_back_leaves_the_node_drained() {
    let rig = Rig::new("cordon-unreadable-back");
    rig.drain.script_placed(
        "dm",
        vec![Ok(Placed::In)],
        Err(PortError::unreachable("the operator API")),
    );
    let roll = roll_dm(&rig.open(), "k");
    assert_eq!(roll.state, RollState::Failed);
    let f = roll.failure.expect("failure");
    assert_eq!(f.code, ErrorCode::UndrainFailed);
    assert!(f.detail.contains("is left drained"), "{}", f.detail);
    assert!(!dm_words(&rig).contains(&"undrain".to_owned()));
}

/// The roll's log lines, as an updater with `with_log` wrote them.
fn lines(u: Updater) -> (Updater, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let u = u.with_log(Arc::new(move |line| {
        sink.lock().expect("lines").push(line.to_owned());
    }));
    (u, seen)
}

fn left_out_lines(seen: &Mutex<Vec<String>>, node: &str) -> usize {
    let want = format!(
        "{node}: left out after the roll: its cordon is not marked as this roll's (an operator's, \
         or placement was unknown at the drain); uncordon it by hand if it should serve"
    );
    seen.lock()
        .expect("lines")
        .iter()
        .filter(|l| l.contains(&want))
        .count()
}

/// A node an operator drained before the roll is left out, and the roll says so once, at warn,
/// naming the node.
///
/// Catches: a node left out silently, with nothing for the operator to act on.
#[test]
fn a_node_left_out_for_an_operators_cordon_is_logged() {
    let rig = Rig::new("cordon-log-op");
    rig.drain.operator_drained("dm");
    serving_never_answers(&rig);
    let (u, seen) = lines(rig.open());
    let roll = roll_dm(&u, "k");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert_eq!(left_out_lines(&seen, "dm-node"), 1, "{:?}", seen.lock());
    assert!(
        seen.lock()
            .expect("lines")
            .iter()
            .all(|l| l.starts_with("warn: ")),
        "{:?}",
        seen.lock()
    );
}

/// A node the roll drained and gave back says nothing.
///
/// Catches: the line logged for every give-back.
#[test]
fn a_node_the_roll_gave_back_is_not_logged() {
    let rig = Rig::new("cordon-log-own");
    let node = ServingNode::new();
    let (u, seen) = lines(open_with(&rig, own(&rig, &node)));
    let roll = roll_dm(&u, "k");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(node.is_serving());
    assert!(seen.lock().expect("lines").is_empty(), "{:?}", seen.lock());
}

/// A node the frontend does not know at the drain (so nothing is marked) and knows, cordoned,
/// at the give-back.
struct LateNode(ServingNode);

impl Drain for LateNode {
    fn placed(&self, host: &Host) -> Result<Placed, PortError> {
        self.0.placed(host)
    }
    fn placed_if_known(&self, host: &Host) -> Result<Option<Placed>, PortError> {
        self.0.placed_if_known(host)
    }
    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError> {
        self.0.drain(host, deadline)?;
        // The node appears at the frontend, cordoned, once the drain call is over.
        *self.0.absent.lock().expect("absent") = false;
        self.0.operator_drains();
        Ok(())
    }
    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.0.leases(host)
    }
    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        self.0.undrain(host)
    }
}

/// Catches: an unmarked drain (placement unknown) that leaves the node out with no word.
#[test]
fn a_node_drained_unmarked_because_placement_was_unknown_is_logged() {
    let rig = Rig::new("cordon-log-unknown");
    let node = ServingNode::new();
    node.no_node();
    serving_never_answers(&rig);
    let drain = Box::new(OwnCordon::new(
        Arc::new(LateNode(node.clone())),
        &rig.dir,
        "roll",
    ));
    let (u, seen) = lines(open_with(&rig, drain));
    let roll = roll_dm(&u, "k");
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(!node.is_serving());
    assert_eq!(marks(&rig.dir), 0);
    assert_eq!(left_out_lines(&seen, "dm-node"), 1, "{:?}", seen.lock());
}
