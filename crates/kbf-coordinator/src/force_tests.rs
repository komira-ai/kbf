//! An operator's force over the coordinator's own calls: who gets the forced order and when,
//! what the force writes, and what ends it (`crate::force`).

use kbf_types::HostRole;
use kbf_updater::version::Version;
use kbf_updater::wire::{Answer, Health, Heartbeat, Outcome, Phase, ServiceReply, UpdateResult};
use serde_json::json;

use crate::decide::Hold;
use crate::identity::Identity;
use crate::ops::{AuditEntry, OperateError, Write};
use crate::service::SetError;
use crate::services::Services;
use crate::testutil::{OPERATOR, Rig, beat, ci_token, id, version};

/// `kbf` with every gate a force passes: a canary, a bake, the automatic rollback and a floor;
/// a frontend waits for the storage and daemon hosts.
pub(crate) const GATED: &str = r#"[[service]]
name = "kbf"
bake_beats = 3
canary = { count = 1 }
rollback = "automatic"
min_version = "1.0"

[service.roles.frontend]
never_last = true
after = ["storage", "daemon"]

[service.roles.storage]

[service.roles.daemon]
"#;

/// The features an agent of this release announces.
pub(crate) const FEATURES: [&str; 3] = ["deferred-v1", "halt-v1", "forced-v1"];

/// A rig over `services`, its clock past the quiet start.
pub(crate) fn rig(name: &str, services: &str) -> Rig {
    let mut rig = Rig::new(name);
    restart(&mut rig, services);
    rig
}

/// The rig's coordinator started again over `services`, and the quiet start waited out.
pub(crate) fn restart(rig: &mut Rig, services: &str) {
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_services(Services::parse(services).expect("services"));
    let now = rig.clock.load(std::sync::atomic::Ordering::SeqCst);
    rig.at(now + 31);
}

fn role_of(name: &str) -> HostRole {
    match name.as_bytes()[0] {
        b'f' => HostRole::FRONTEND,
        b's' => HostRole::STORAGE,
        _ => HostRole::DAEMON,
    }
}

/// The heartbeat of an agent of this release running `running`, `health`.
pub(crate) fn agent(running: &str, health: Health) -> Heartbeat {
    let mut b = beat(running);
    b.health = health;
    b.agent_features = FEATURES.iter().map(|f| (*f).to_owned()).collect();
    b
}

/// `name`'s heartbeat, answered: its role is its first letter's (f, s, else daemon).
pub(crate) fn send(rig: &Rig, name: &str, b: Heartbeat) -> Answer {
    let who = Identity {
        node_id: id(name),
        role: role_of(name),
    };
    rig.coordinator.heartbeat(who, b)
}

pub(crate) fn operate(rig: &Rig, body: &str) -> Result<AuditEntry, OperateError> {
    rig.coordinator.operate(Some(OPERATOR), body.as_bytes())
}

pub(crate) fn force_body(version: &str, limits: &str) -> String {
    format!(
        r#"{{"action":"force","service":"kbf","version":"{version}","reason":"2.0 corrupts leases"{limits}}}"#
    )
}

/// Forces `kbf` to `version` with the extra fields `limits` (`,"max_updating":{..}` or none).
pub(crate) fn force(rig: &Rig, version: &str, limits: &str) -> AuditEntry {
    operate(rig, &force_body(version, limits)).expect("forced")
}

pub(crate) fn update(target: &str) -> ServiceReply {
    ServiceReply::Update {
        service: None,
        version: version(target),
    }
}

pub(crate) fn halted(target: &str) -> UpdateResult {
    UpdateResult {
        phase: Some(Phase::Halt),
        reason: kbf_updater::wire::Token::parse("disk_tainted"),
        ..UpdateResult::new(version(target), Outcome::Failed)
    }
}

const DESIRED_2: &str = r#"{"version":"2.0","min_up":{"storage":2,"frontend":1}}"#;

/// The ops audit, as (by, action) pairs, from the entry numbered `from` on.
fn audit_from(rig: &Rig, from: u64) -> Vec<(String, String)> {
    rig.coordinator
        .ops()
        .state()
        .audit
        .into_iter()
        .filter(|e| e.seq >= from)
        .map(|e| (e.by, e.action))
        .collect()
}

pub(crate) fn hold_of(rig: &Rig, name: &str) -> Option<Hold> {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    status.per_service["kbf"]
        .hosts
        .iter()
        .find(|h| h.host.node_id == name)
        .and_then(|h| h.hold)
}

/// A farm of three daemons, two storage hosts and a frontend at 2.0 under CI's desired 2.0 that
/// keeps both storage hosts up, with every gate a force passes closed: 1.5 is marked bad, `s2`
/// has halted the service (its agent holds the halt), and `d1` declined 1.5 a moment ago.
async fn gated_farm(name: &str) -> Rig {
    let rig = rig(name, GATED);
    rig.set(DESIRED_2).await;
    rig.coordinator
        .ops()
        .change(
            Write::BeforeMemory,
            AuditEntry::new("test", "mark_bad"),
            |state| {
                let marks = state.services.entry("kbf".to_owned()).or_default();
                marks.bad.push(version("1.5"));
            },
        )
        .expect("marked");
    for name in ["d1", "d2", "d3", "s1", "f1"] {
        assert_eq!(
            send(&rig, name, agent("2.0", Health::Healthy)),
            ServiceReply::Nothing
        );
    }
    let mut halting = agent("2.0", Health::Healthy);
    halting.last_update = Some(halted("2.0"));
    assert_eq!(send(&rig, "s2", halting), ServiceReply::Nothing);
    let mut declined = agent("2.0", Health::Healthy);
    declined.deferred = Some(
        serde_json::from_value(
            json!({"target":"1.5","reason":"drain_bound","retry_after_secs":3600}),
        )
        .expect("a deferral"),
    );
    assert_eq!(send(&rig, "d1", declined), ServiceReply::Nothing);
    rig
}

/// The beat a host of [`gated_farm`] sends now: `s2` keeps reporting its halt until it updates
/// again, `d1` its deferral.
fn farm_beat(name: &str, running: &str, health: Health) -> Heartbeat {
    let mut b = agent(running, health);
    if name == "s2" && running == "2.0" {
        b.last_update = Some(halted("2.0"));
    }
    if name == "d1" && running == "2.0" {
        b.deferred = Some(
            serde_json::from_value(
                json!({"target":"1.5","reason":"drain_bound","retry_after_secs":3600}),
            )
            .expect("a deferral"),
        );
    }
    b
}

/// Catches: a force held back by any gate it is meant to pass. Under a standing halt, a bad mark
/// on the target, `min_up`, a deferral, a canary and a bake, every live host receives the forced
/// order (`forced: true`), with the clear generation that lifts its agent's halt, the storage
/// and daemon hosts in the first round and the frontend after them; and hosts that run the
/// target unhealthy after the swap hold no slot, so the force reaches every host.
#[tokio::test]
async fn a_force_reaches_every_live_host_past_every_health_gate() {
    let rig = gated_farm("force-reaches").await;
    // The ordinary rules: the halt holds everyone.
    assert_eq!(hold_of(&rig, "d2"), Some(Hold::Halted));
    force(&rig, "1.5", r#","max_updating":{"daemon":3,"storage":2}"#);
    let all = ["d1", "d2", "d3", "s1", "s2", "f1"];
    let mut ordered = Vec::new();
    for name in all {
        let answer = send(&rig, name, farm_beat(name, "2.0", Health::Healthy));
        if answer == update("1.5") {
            assert!(answer.forced, "{name}");
            assert_eq!(answer.halts.get("kbf"), Some(&1), "{name}");
            ordered.push(name);
        } else {
            assert_eq!(answer, ServiceReply::Nothing, "{name}");
        }
    }
    assert_eq!(ordered, ["d1", "d2", "d3", "s1", "s2"]);
    assert_eq!(hold_of(&rig, "f1"), Some(Hold::WaitingForRole));
    // They run 1.5, and none of them is healthy on it.
    for name in &ordered {
        assert_eq!(
            send(&rig, name, agent("1.5", Health::Unhealthy)),
            ServiceReply::Nothing
        );
    }
    let answer = send(&rig, "f1", farm_beat("f1", "2.0", Health::Healthy));
    assert_eq!(answer, update("1.5"));
    assert!(answer.forced);
    assert_eq!(
        send(&rig, "f1", agent("1.5", Health::Unhealthy)),
        ServiceReply::Nothing
    );
    for name in all {
        assert_eq!(hold_of(&rig, name), Some(Hold::UpToDate), "{name}");
    }
}

/// Catches: a force that is not audited as what it did, or that keeps the halt: one force is four
/// entries by the operator, in one write (`force`, then `unmark_bad` of the target, `mark_bad`
/// of the version left, `clear_halt` of the standing halt with its host), the halt is gone, the
/// service's clear generation is 1, the answer is the `force` entry, and the status shows the
/// force with its generation and raised limits.
#[tokio::test]
async fn a_force_clears_the_halt_and_audits_each_of_its_effects() {
    let rig = gated_farm("force-audit").await;
    let before = rig
        .coordinator
        .ops()
        .state()
        .audit
        .last()
        .expect("audit")
        .seq;
    let head = force(&rig, "1.5", r#","max_updating":{"daemon":3}"#);
    assert_eq!(head.action, "force");
    assert_eq!(head.seq, before + 1);
    assert_eq!(
        head.reason.as_deref(),
        Some("1.5, generation 1: 2.0 corrupts leases")
    );
    let entries: Vec<AuditEntry> = rig
        .coordinator
        .ops()
        .state()
        .audit
        .into_iter()
        .filter(|e| e.seq > before)
        .collect();
    let said: Vec<(&str, &str, Option<&str>)> = entries
        .iter()
        .map(|e| (e.by.as_str(), e.action.as_str(), e.reason.as_deref()))
        .collect();
    assert_eq!(
        said,
        [
            (
                "operator",
                "force",
                Some("1.5, generation 1: 2.0 corrupts leases")
            ),
            ("operator", "unmark_bad", Some("1.5 is forced")),
            (
                "operator",
                "mark_bad",
                Some("2.0 was left by the force to 1.5")
            ),
            (
                "operator",
                "clear_halt",
                Some("the force to 1.5: 2.0 corrupts leases")
            ),
        ]
    );
    assert_eq!(entries[3].node_id.as_deref(), Some("s2"));
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.halt, None);
    assert_eq!(marks.halt_generation, 1);
    assert_eq!(marks.bad, [version("2.0")]);
    assert_eq!(marks.bad_by_force, [version("2.0")]);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    let shown = serde_json::to_value(&status.per_service["kbf"].forced).expect("json");
    assert_eq!(
        shown,
        json!({"version":"1.5","generation":1,"reason":"2.0 corrupts leases","max_updating":{"daemon":3}})
    );
    assert_eq!(status.services["kbf"].version, version("1.5"));
}

/// Catches: a second force counted as the first, or a force with no halt that leaves an agent's
/// own hold: the next force is generation 2 and marks the version it leaves (the first forced
/// one); with no halt standing its clear still raises the generation (an entry with no host).
#[tokio::test]
async fn a_second_force_counts_on_and_raises_the_generation_without_a_halt() {
    let rig = gated_farm("force-second").await;
    force(&rig, "1.5", "");
    let head = force(&rig, "1.2", "");
    assert_eq!(
        head.reason.as_deref(),
        Some("1.2, generation 2: 2.0 corrupts leases")
    );
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.halt_generation, 2);
    assert_eq!(marks.bad, [version("2.0"), version("1.5")]);
    assert_eq!(marks.bad_by_force, [version("2.0"), version("1.5")]);
    let last = rig
        .coordinator
        .ops()
        .state()
        .audit
        .last()
        .cloned()
        .expect("last");
    assert_eq!((last.action.as_str(), last.node_id), ("clear_halt", None));
    // The same version again leaves nothing: no mark, no unmark.
    let seq = last.seq;
    force(&rig, "1.2", "");
    assert_eq!(
        audit_from(&rig, seq + 1),
        [
            ("operator".to_owned(), "force".to_owned()),
            ("operator".to_owned(), "clear_halt".to_owned())
        ]
    );
}

/// Catches: the version a force left being rolled out again by CI by accident, or held for good:
/// CI's desired state of it is refused (`HeldBad`) and changes nothing; an operator's
/// `unmark_bad` lifts the mark and its hold, after which CI sets it, and that ends the force.
#[tokio::test]
async fn the_version_left_needs_an_operators_unmark_before_ci_sets_it_again() {
    let rig = gated_farm("force-left").await;
    force(&rig, "1.5", "");
    let refused = rig
        .coordinator
        .set_desired(Some(&ci_token()), DESIRED_2.as_bytes())
        .await;
    assert!(matches!(&refused, Err(SetError::HeldBad(v)) if *v == version("2.0")));
    assert_eq!(
        refused.expect_err("refused").to_string(),
        "2.0 was left by a force and is marked bad: an operator's unmark_bad lifts it"
    );
    assert_eq!(
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .services["kbf"]
            .version,
        version("1.5")
    );
    let unmark = r#"{"action":"unmark_bad","service":"kbf","version":"2.0","reason":"fixed by 2.0.1 config"}"#;
    let entry = operate(&rig, unmark).expect("unmarked");
    assert_eq!(entry.action, "unmark_bad");
    assert_eq!(entry.by, "operator");
    assert_eq!(
        entry.reason.as_deref(),
        Some("fixed by 2.0.1 config; 0 failure reports acked")
    );
    let marks = rig.coordinator.ops().marks("kbf");
    assert!(marks.bad.is_empty() && marks.bad_by_force.is_empty());
    let seq = entry.seq;
    rig.set(DESIRED_2).await;
    assert_eq!(
        audit_from(&rig, seq + 1),
        [("ci".to_owned(), "force_ended".to_owned())]
    );
    assert_eq!(rig.coordinator.ops().marks("kbf").force, None);
}

/// Catches: an unmark that is accepted for what it cannot do, or that is not refused cleanly: a
/// version that is not marked, an unknown service and an empty reason are each `Invalid` and
/// change nothing.
#[tokio::test]
async fn an_unmark_of_nothing_is_refused() {
    let rig = gated_farm("force-unmark-refused").await;
    let before = rig.coordinator.ops().state();
    for (body, why) in [
        (
            r#"{"action":"unmark_bad","service":"kbf","version":"3.0","reason":"x"}"#,
            "the version is not marked bad",
        ),
        (
            r#"{"action":"unmark_bad","service":"nope","version":"1.5","reason":"x"}"#,
            "service is not one this coordinator manages",
        ),
        (
            r#"{"action":"unmark_bad","service":"kbf","version":"1.5","reason":" "}"#,
            "reason must be 1 to 256 bytes with no control character",
        ),
    ] {
        let err = operate(&rig, body).expect_err("refused");
        assert!(matches!(err, OperateError::Invalid(_)), "{body}");
        assert_eq!(err.to_string(), why, "{body}");
    }
    assert_eq!(rig.coordinator.ops().state(), before);
}

/// Catches: a force that outlives the next ordinary desired state, or one that CI ends without a
/// trace: CI setting another version ends it (`force_ended` by CI), and the ordinary rules hold
/// again (a new bad mark of that version holds every host); CI setting the forced version itself
/// ends it too.
#[tokio::test]
async fn the_next_ordinary_desired_state_ends_the_force() {
    let rig = gated_farm("force-ends").await;
    force(&rig, "1.5", "");
    rig.set(r#"{"version":"1.6","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.force, None);
    let last = rig
        .coordinator
        .ops()
        .state()
        .audit
        .last()
        .cloned()
        .expect("last");
    assert_eq!(
        (last.by.as_str(), last.action.as_str()),
        ("ci", "force_ended")
    );
    assert_eq!(
        last.reason.as_deref(),
        Some("the force to 1.5 ends: 1.6 was set")
    );
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.per_service["kbf"].forced.is_none());
    // The ordinary rules: the canary lets one host go, and an answer is not forced.
    let answer = send(&rig, "d1", agent("2.0", Health::Healthy));
    assert_eq!(answer, update("1.6"));
    assert!(!answer.forced);

    force(&rig, "1.5", "");
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    assert_eq!(rig.coordinator.ops().marks("kbf").force, None);
    // Nothing to end: no entry.
    let seq = rig
        .coordinator
        .ops()
        .state()
        .audit
        .last()
        .expect("last")
        .seq;
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    assert!(audit_from(&rig, seq + 1).is_empty());
}

/// Catches: a force that does not pace itself, or ignores the pace it was given: with the
/// service's own limit (2 daemons) the third daemon waits for a slot and gets it when one of the
/// first two runs the target, unhealthy; a raised limit (3) orders all three at once; a role the
/// request does not raise keeps its own limit (1 storage host).
#[tokio::test]
async fn max_updating_is_honoured_and_a_raised_value_too() {
    let rig = rig("force-pace", GATED);
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "d2", "d3", "s1", "s2"] {
        send(&rig, name, agent("2.0", Health::Healthy));
    }
    force(&rig, "1.5", "");
    let round = |names: &[&str]| -> Vec<bool> {
        names
            .iter()
            .map(|n| send(&rig, n, agent("2.0", Health::Healthy)) == update("1.5"))
            .collect()
    };
    assert_eq!(round(&["d1", "d2", "d3"]), [true, true, false]);
    assert_eq!(hold_of(&rig, "d3"), Some(Hold::NoSlot));
    assert_eq!(round(&["s1", "s2"]), [true, false]);
    assert_eq!(
        send(&rig, "d1", agent("1.5", Health::Unhealthy)),
        ServiceReply::Nothing
    );
    assert_eq!(round(&["d3"]), [true]);

    let rig = self::rig("force-pace-raised", GATED);
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "d2", "d3", "s1", "s2"] {
        send(&rig, name, agent("2.0", Health::Healthy));
    }
    force(&rig, "1.5", r#","max_updating":{"daemon":3}"#);
    let round = |names: &[&str]| -> Vec<bool> {
        names
            .iter()
            .map(|n| send(&rig, n, agent("2.0", Health::Healthy)) == update("1.5"))
            .collect()
    };
    assert_eq!(round(&["d1", "d2", "d3"]), [true, true, true]);
    assert_eq!(round(&["s1", "s2"]), [true, false]);
}

/// Catches: an older agent sent `forced` (its strict reader refuses the whole answer), or freed
/// of its slot as if it could not go back: an agent that did not announce `forced-v1` gets the
/// order in the shape it has always read, goes back by its own rule, and its failed, unhealthy
/// host holds its slot the ordinary way, so the next daemon waits.
#[tokio::test]
async fn an_older_agent_gets_an_ordinary_order_and_holds_its_slot_the_ordinary_way() {
    let rig = rig("force-older", GATED);
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    send(&rig, "d1", beat("2.0"));
    send(&rig, "d2", agent("2.0", Health::Healthy));
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    let answer = send(&rig, "d1", beat("2.0"));
    assert_eq!(answer, update("1.5"));
    assert!(!answer.forced);
    assert_eq!(
        serde_json::to_string(&answer).expect("json"),
        r#"{"reply":"update","version":"1.5"}"#
    );
    // It went back: failed, and 2.0 is not healthy either.
    let mut back = beat("2.0");
    back.health = Health::Unhealthy;
    back.last_update = Some(UpdateResult::new(version("1.5"), Outcome::Failed));
    assert_eq!(send(&rig, "d1", back), ServiceReply::Nothing);
    assert_eq!(hold_of(&rig, "d2"), Some(Hold::NoSlot));
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::FailedHere));
}

/// Catches: a force that cannot reach a host which failed the target before it, or one that
/// orders a host again and again after it failed under the force: the failure standing when the
/// force was made is acknowledged, so the host is ordered (while CI's ordinary desired state of
/// the same version holds it at rule 4); a failure after the forced order holds it.
#[tokio::test]
async fn a_host_that_failed_the_target_before_the_force_is_ordered_once_more() {
    let rig = rig("force-failed-before", GATED);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let mut failed = agent("2.0", Health::Healthy);
    failed.last_update = Some(UpdateResult::new(version("1.5"), Outcome::Failed));
    assert_eq!(send(&rig, "d1", failed.clone()), ServiceReply::Nothing);
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::FailedHere));
    force(&rig, "1.5", "");
    let answer = send(&rig, "d1", failed.clone());
    assert_eq!(answer, update("1.5"));
    assert!(answer.forced);
    // It failed the forced order too: held, as rule 4 holds it.
    send(&rig, "d1", updating_of(failed.clone(), "1.5"));
    assert_eq!(send(&rig, "d1", failed), ServiceReply::Nothing);
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::FailedHere));
}

pub(crate) fn updating_of(mut b: Heartbeat, target: &str) -> Heartbeat {
    b.updating = Some(version(target));
    b
}

/// Catches: a force that reaches a host before the coordinator has heard the farm, or that
/// ignores a host busy with its own update: the quiet start holds it, and so does an update the
/// host is running; a host that sends `promotion_pending` at another version holds a slot.
#[tokio::test]
async fn a_force_keeps_the_quiet_start_and_a_hosts_own_update() {
    let mut rig = rig("force-quiet", GATED);
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    restart(&mut rig, GATED);
    rig.at(rig.clock.load(std::sync::atomic::Ordering::SeqCst) - 31);
    assert_eq!(
        send(&rig, "d1", agent("2.0", Health::Healthy)),
        ServiceReply::Nothing
    );
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::Quiet));
    rig.at(rig.clock.load(std::sync::atomic::Ordering::SeqCst) + 31);
    let busy = updating_of(agent("2.0", Health::Healthy), "2.1");
    assert_eq!(send(&rig, "d1", busy), ServiceReply::Nothing);
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::Updating));
    // At another version, promotion pending, and held itself by a failure after the force.
    let mut pending = agent("1.9", Health::Healthy);
    pending.promotion_pending = true;
    pending.last_update = Some(UpdateResult::new(version("1.5"), Outcome::Failed));
    assert_eq!(send(&rig, "d1", pending), ServiceReply::Nothing);
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::FailedHere));
    assert_eq!(
        send(&rig, "d2", agent("2.0", Health::Healthy)),
        ServiceReply::Nothing
    );
    assert_eq!(hold_of(&rig, "d2"), Some(Hold::NoSlot));
    let mut promoted = agent("1.5", Health::Healthy);
    promoted.promotion_pending = true;
    send(&rig, "d1", promoted);
    assert_eq!(
        send(&rig, "d2", agent("2.0", Health::Healthy)),
        update("1.5")
    );
}

/// Catches: the last good version recorded for a forced version that never converged or baked,
/// or the automatic rollback undoing the operator's choice: while the hosts run the forced
/// version unhealthy, or fail it after the commit point on enough hosts, nothing is recorded and
/// nothing is rolled back; once every host has beaten healthy three times at it, it is the last
/// good version.
#[tokio::test]
async fn last_good_moves_only_after_the_forced_version_converged_and_baked() {
    let rig = rig("force-last-good", GATED);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "d2", "d3", "s1", "f1"] {
        for _ in 0..3 {
            send(&rig, name, agent("1.5", Health::Healthy));
        }
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.5"))
    );
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "d2", "d3", "s1", "f1"] {
        for _ in 0..3 {
            send(&rig, name, agent("2.0", Health::Healthy));
        }
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("2.0"))
    );
    force(
        &rig,
        "1.4",
        r#","max_updating":{"daemon":3,"storage":1,"frontend":1}"#,
    );
    // Two hosts fail 1.4 after its commit point, the others run it unhealthy.
    let mut failed = agent("2.0", Health::Healthy);
    failed.last_update = Some(UpdateResult {
        phase: Some(Phase::AfterCommit),
        ..UpdateResult::new(version("1.4"), Outcome::Failed)
    });
    send(&rig, "d1", failed.clone());
    send(&rig, "d2", failed);
    for name in ["d3", "s1", "f1"] {
        send(&rig, name, agent("1.4", Health::Unhealthy));
    }
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.last_good, Some(version("2.0")));
    assert!(!marks.bad.contains(&version("1.4")));
    assert_eq!(
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .services["kbf"]
            .version,
        version("1.4")
    );
    for name in ["d1", "d2", "d3", "s1", "f1"] {
        for _ in 0..2 {
            send(&rig, name, agent("1.4", Health::Healthy));
        }
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("2.0")),
        "two healthy beats are not a bake of three"
    );
    for name in ["d1", "d2", "d3", "s1", "f1"] {
        send(&rig, name, agent("1.4", Health::Healthy));
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.4"))
    );
}

/// Catches: a force below the first release with the host-updates hook taken, or refused with no
/// reason: one release below `min_version` is `BelowMinVersion`, which names both versions and
/// the manual path, and changes nothing; `min_version` itself is accepted.
#[tokio::test]
async fn a_force_below_min_version_is_refused_with_its_reason() {
    let rig = rig("force-floor", GATED);
    rig.set(DESIRED_2).await;
    let before = rig.coordinator.ops().state();
    let err = operate(&rig, &force_body("0.9", "")).expect_err("refused");
    assert!(matches!(
        &err,
        OperateError::BelowMinVersion { version: v, floor } if *v == version("0.9") && *floor == version("1.0")
    ));
    assert_eq!(
        err.to_string(),
        "0.9 is older than 1.0, the service's min_version: the first release with the \
         host-updates hook. A force does not go below it; install the older release on each \
         host by hand (docs/deploy/kbf-updater.md, \"Below min_version\")"
    );
    assert_eq!(rig.coordinator.ops().state(), before);
    force(&rig, "1.0", "");
    assert_eq!(
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .services["kbf"]
            .version,
        Version::parse("1.0").expect("v")
    );
}

/// Catches: a force request that is accepted with limits it cannot honour, or with the parts of
/// another action: an unknown service, a role the service does not run on, a limit of 0 or over
/// 10000, a role given twice, an unknown field and a version that is none are each refused and
/// change nothing.
#[tokio::test]
async fn a_bad_force_request_is_refused_and_changes_nothing() {
    let rig = rig("force-bad", GATED);
    rig.set(DESIRED_2).await;
    let before = rig.coordinator.ops().state();
    for (body, why) in [
        (
            r#"{"action":"force","service":"nope","version":"1.5","reason":"r"}"#.to_owned(),
            "service is not one this coordinator manages",
        ),
        (
            force_body("1.5", r#","max_updating":{"mac-daemon":1}"#),
            "max_updating names a role the service does not run on",
        ),
        (
            force_body("1.5", r#","max_updating":{"daemon":0}"#),
            "max_updating is 1 to 10000 hosts per role",
        ),
        (
            force_body("1.5", r#","max_updating":{"daemon":10001}"#),
            "max_updating is 1 to 10000 hosts per role",
        ),
        (
            force_body("1.5", r#","max_updating":{"daemon":1,"daemon":2}"#),
            "the body is not an operator action",
        ),
        (
            force_body("1.5", r#","node_id":"d1""#),
            "the body is not an operator action",
        ),
        (
            force_body("not a version!", ""),
            "the body is not an operator action",
        ),
        (
            r#"{"action":"force","service":"kbf","version":"1.5","reason":""}"#.to_owned(),
            "reason must be 1 to 256 bytes with no control character",
        ),
    ] {
        let err = operate(&rig, &body).expect_err("refused");
        assert_eq!(err.to_string(), why, "{body}");
    }
    assert_eq!(rig.coordinator.ops().state(), before);
    assert_eq!(
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .services["kbf"]
            .version,
        version("2.0")
    );
}

/// Catches: a force of a service CI never set failing, or writing a desired state an earlier
/// coordinator cannot read: it is the forced version with the default limits and no minimum,
/// and nothing is marked left.
#[tokio::test]
async fn a_force_of_a_service_never_set_writes_a_plain_desired_state() {
    let rig = rig("force-first", GATED);
    force(&rig, "1.5", "");
    let text = std::fs::read_to_string(rig.dir.path().join("desired.json")).expect("desired");
    let state = crate::desired::Desired::parse(text.as_bytes()).expect("an earlier reader's");
    assert_eq!(state.version, version("1.5"));
    assert_eq!(state.min_up.min_up_of("storage"), 0);
    assert_eq!(state.max_updating.max_updating_of("daemon"), 2);
    assert!(rig.coordinator.ops().marks("kbf").bad.is_empty());
}

/// Catches: the automatic rollback going back to the version a force left once the force has
/// ended: with that version the last good one, a later release that fails on enough hosts is
/// marked bad and the desired state stays, rather than going to a version every host is held
/// from (the farm would be stuck on nothing).
#[tokio::test]
async fn the_automatic_rollback_never_goes_back_to_the_version_a_force_left() {
    let rig = rig("force-no-way-back", GATED);
    rig.set(r#"{"version":"1.2","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "d2", "d3", "s1", "f1"] {
        for _ in 0..3 {
            send(&rig, name, agent("1.2", Health::Healthy));
        }
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.2"))
    );
    force(&rig, "1.5", "");
    rig.set(r#"{"version":"1.6","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let mut failed = agent("1.5", Health::Healthy);
    failed.last_update = Some(UpdateResult {
        phase: Some(Phase::AfterCommit),
        ..UpdateResult::new(version("1.6"), Outcome::Failed)
    });
    send(&rig, "d1", failed.clone());
    send(&rig, "d2", failed);
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.last_good, Some(version("1.2")));
    assert!(marks.bad.contains(&version("1.6")));
    assert_eq!(
        rig.coordinator
            .status(Some(OPERATOR))
            .expect("status")
            .services["kbf"]
            .version,
        version("1.6")
    );
}

/// Catches: a halt a host reports while a force stands stopping the force for the other hosts,
/// or going unrecorded: the halt is raised (the convergence says `halted`), and the next host is
/// still given the forced order.
#[tokio::test]
async fn a_halt_raised_while_a_force_stands_is_recorded_and_does_not_stop_it() {
    let rig = rig("force-halt-during", GATED);
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "d2"] {
        send(&rig, name, agent("2.0", Health::Healthy));
    }
    force(&rig, "1.5", r#","max_updating":{"daemon":2}"#);
    let mut halting = agent("2.0", Health::Healthy);
    halting.last_update = Some(halted("1.5"));
    assert_eq!(send(&rig, "d1", halting), ServiceReply::Nothing);
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.halt.map(|h| h.node_id), Some("d1".to_owned()));
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(
        serde_json::to_value(&status.per_service["kbf"].convergence).expect("json")["state"],
        "halted"
    );
    let answer = send(&rig, "d2", agent("2.0", Health::Healthy));
    assert_eq!(answer, update("1.5"));
    assert!(answer.forced);
}
