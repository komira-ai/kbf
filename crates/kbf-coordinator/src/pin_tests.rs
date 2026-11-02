//! A pinned host: one whose heartbeat says `pinned` (root on the host holds it on a release with
//! `kbf-host-install activate --pin`). It is not ordered, keeps its role's slot, is counted
//! neither failed nor at another version, gives no vote for a rollback, is no canary, and the
//! status says so. Every time is explicit; the fleets are built beat by beat.

use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{Commit, Heartbeat, Outcome, PinReport, ServiceReply};
use serde_json::{Value, json};

use crate::bake_canary_tests::{beats, go, held, services, started, why};
use crate::converge::{State, converge_kbf};
use crate::decide::Hold;
use crate::desired::KBF;
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::rollback_tests::{AUTO, def, farm, good, plan_for, release, rolled_back};
use crate::services::ServiceDef;
use crate::testutil::{OPERATOR, Rig, beat, desired, id, last, secs, version};

const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// `b` with the pin of `COMMIT`.
fn pinned(mut b: Heartbeat) -> Heartbeat {
    b.pinned = Some(PinReport {
        commit: Commit::parse(COMMIT),
    });
    b
}

/// A pinned host is held (`pinned`) while the desired version differs, before every rule that
/// would order it; once its beat carries no pin it is ordered again. A pin whose commit is not
/// known holds the same.
///
/// Catches: an order sent to a host whose agent will not run it; a pin that outlives its beat.
#[test]
fn a_pinned_host_is_held_and_ordered_again_once_unpinned() {
    let s = services("", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    beats(&mut f, "a", &pinned(beat("1.0")), 100);
    assert_eq!(why(&s, &f, "a", 100), held(Hold::Pinned));
    assert_eq!(
        why(&s, &f, "a", 10),
        held(Hold::Quiet),
        "the quiet start first"
    );
    let mut unknown = beat("1.0");
    unknown.pinned = Some(PinReport { commit: None });
    beats(&mut f, "a", &unknown, 115);
    assert_eq!(why(&s, &f, "a", 115), held(Hold::Pinned));
    beats(&mut f, "a", &beat("1.0"), 130);
    assert_eq!(why(&s, &f, "a", 130), go());
}

/// A pinned host keeps a slot of its role, whatever it runs and however healthy it is, so the
/// rollout does not go on to the next host because one was pinned; without the pin the next
/// host is ordered.
///
/// Catches: a pin that frees the slot of a host that failed the release (it is healthy again
/// on the release it was pinned to).
#[test]
fn a_pinned_host_holds_its_slot_and_frees_none() {
    let s = services("", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    beats(
        &mut f,
        "a",
        &pinned(last(beat("1.0"), "2.0", Outcome::Failed)),
        100,
    );
    assert!(f.get(&id("a")).unwrap().holds_slot());
    assert_eq!(why(&s, &f, "b", 100), held(Hold::NoSlot));
    beats(&mut f, "a", &last(beat("1.0"), "2.0", Outcome::Failed), 115);
    assert!(!f.get(&id("a")).unwrap().holds_slot(), "healthy again");
    assert_eq!(why(&s, &f, "b", 115), go());
}

/// An order outstanding for a host ends when its beat says it is pinned: its agent will not run
/// it.
///
/// Catches: an order that stands for its whole age at a pinned host.
#[test]
fn a_pinned_beat_ends_the_order_it_was_given() {
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    f.order(KBF, id("a"), version("2.0"), secs(100));
    beats(&mut f, "a", &beat("1.0"), 115);
    assert!(f.is_ordered(KBF, &id("a"), secs(115)), "not reported yet");
    beats(&mut f, "a", &pinned(beat("1.0")), 130);
    assert!(!f.is_ordered(KBF, &id("a"), secs(130)));
}

/// A live pinned host that failed the desired version is counted pinned, not failed: the farm
/// is `in_progress`, not `blocked`, and it is not `converged` while the host runs another
/// version. Pinned on the desired version it counts there too, and the farm converges. Without
/// the pin the same host blocks the farm. A pinned host that went silent is counted down only.
///
/// Catches: a pin read as a failure (a blocked deploy) or as a host done at another version.
#[test]
fn a_pinned_host_counts_neither_failed_nor_converged() {
    let want = desired("2.0", 0, 0);
    let kbf = ServiceDef::kbf();
    let failed = last(beat("1.0"), "2.0", Outcome::Failed);
    let farm_of = |b: &Heartbeat| {
        let mut f = Fleet::new(Duration::ZERO);
        f.record(id("s1"), HostRole::STORAGE, beat("2.0"), secs(100));
        f.record(id("s2"), HostRole::STORAGE, b.clone(), secs(100));
        f
    };
    let c = converge_kbf(
        &kbf,
        Some(&want),
        &farm_of(&pinned(failed.clone())),
        secs(100),
    );
    assert_eq!(c.state, State::InProgress);
    let storage = &c.roles[0];
    assert_eq!(
        (
            storage.at_desired,
            storage.failed,
            storage.pinned,
            storage.down
        ),
        (1, 0, 1, 0)
    );
    let c = converge_kbf(&kbf, Some(&want), &farm_of(&failed), secs(100));
    assert_eq!(c.state, State::Blocked);
    assert_eq!((c.roles[0].failed, c.roles[0].pinned), (1, 0));
    let c = converge_kbf(&kbf, Some(&want), &farm_of(&pinned(beat("2.0"))), secs(100));
    assert_eq!(c.state, State::Converged);
    assert_eq!((c.roles[0].at_desired, c.roles[0].pinned), (2, 1));

    // Silent: down, and no longer counted pinned; a role of only such hosts is still listed.
    let mut f = Fleet::new(Duration::ZERO);
    f.record(id("s1"), HostRole::STORAGE, pinned(failed), secs(100));
    let c = converge_kbf(&kbf, Some(&want), &f, secs(1_000));
    assert_eq!((c.roles[0].pinned, c.roles[0].down), (0, 1));
}

/// The failures of pinned hosts are no votes for the automatic rollback: two failed hosts of
/// six roll the service back, the same two pinned do not.
///
/// Catches: a pin that trips the rollback (or, later, a halt) the operator pinned to avoid.
#[test]
fn a_pinned_hosts_failure_does_not_roll_the_service_back() {
    let (phase, class) = release();
    let automatic = def(AUTO);
    let failing = farm(6, 2, phase, class);
    assert_eq!(plan_for(&automatic, &good(), &failing), rolled_back());
    let mut held_back = failing.clone();
    for host in ["d0", "d1"] {
        let b = failing.get(&id(host)).unwrap().beat.clone();
        held_back.record(id(host), HostRole::DAEMON, pinned(b), secs(2_000));
    }
    assert_eq!(plan_for(&automatic, &good(), &held_back), vec![]);
}

/// A pinned host is never chosen as a canary: with a canary of one and the smallest id pinned,
/// the next host goes.
///
/// Catches: a canary gate that waits forever for a host that will not update.
#[test]
fn a_pinned_host_is_not_chosen_as_a_canary() {
    let s = services("canary = { count = 1 }", 3, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    beats(&mut f, "a", &pinned(beat("1.0")), 100);
    assert_eq!(why(&s, &f, "a", 100), held(Hold::Pinned));
    assert_eq!(why(&s, &f, "b", 100), go());
    assert_eq!(why(&s, &f, "c", 100), held(Hold::CanaryWait));
}

fn status_of(rig: &Rig) -> Value {
    serde_json::to_value(rig.coordinator.status(Some(OPERATOR)).expect("status")).expect("JSON")
}

fn host<'a>(hosts: &'a Value, node: &str) -> &'a Value {
    hosts
        .as_array()
        .expect("hosts")
        .iter()
        .find(|h| h["node_id"] == node)
        .unwrap_or_else(|| panic!("{node} is listed"))
}

/// The status lists a pinned host as pinned: its entry carries the pin and its commit, holds a
/// slot, and its hold under `kbf` is `pinned`; its role counts it pinned and not failed; its
/// heartbeat is answered with nothing. A farm with no pin keeps the convergence it always gave,
/// with no `pinned` count.
///
/// Catches: a pinned host the status does not show, or shows as failed or ordered.
#[tokio::test]
async fn the_status_lists_a_pinned_host_as_pinned() {
    let rig = Rig::new("pin-status");
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    rig.at(2_000);
    let who = |name: &str| Identity {
        node_id: id(name),
        role: HostRole::STORAGE,
    };
    let c = &rig.coordinator;
    assert_eq!(c.heartbeat(who("s1"), beat("2.0")), ServiceReply::Nothing);
    let roles = &status_of(&rig)["per_service"]["kbf"]["roles"];
    assert_eq!(
        roles,
        &json!([{"role": "storage", "at_desired": 1, "updating": 0, "failed": 0, "down": 0}])
    );
    let answer = c.heartbeat(who("s2"), pinned(last(beat("1.0"), "2.0", Outcome::Failed)));
    assert_eq!(answer, ServiceReply::Nothing);

    let status = status_of(&rig);
    let top = host(&status["hosts"], "s2");
    assert_eq!(top["pinned"], json!({"commit": COMMIT}));
    assert_eq!(top["holds_slot"], json!(true));
    let kbf = &status["per_service"]["kbf"];
    assert_eq!(host(&kbf["hosts"], "s2")["hold"], json!("pinned"));
    assert_eq!(host(&kbf["hosts"], "s1").get("pinned"), None);
    assert_eq!(
        kbf["roles"],
        json!([{"role": "storage", "at_desired": 1, "updating": 0, "failed": 0, "down": 0,
                "pinned": 1}])
    );
    assert_eq!(kbf["convergence"]["state"], json!("in_progress"));
    assert_eq!(kbf["slots_held"], json!(1));
}
