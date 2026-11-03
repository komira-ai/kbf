//! A farm whose kbf hosts are Macs (role `mac-daemon`), heard through the coordinator's own
//! heartbeat and convergence calls: a Mac that declines its order because leases still run
//! on it (`leases_live`) holds no slot and does not block the roll, is ordered again once the
//! wait is over, and the role converges. Every time is set by the test; nothing sleeps.

use kbf_types::{HostRole, ReleasePlatform};
use kbf_updater::wire::{DeferredReport, Heartbeat, Outcome, Reply, Token};

use crate::converge::State;
use crate::desired::KBF;
use crate::identity::Identity;
use crate::testutil::{Rig, beat, ci_token, id, last, updating, version};

/// Macs only: no storage host or frontend must stay up, one Mac updates at a time.
const MACS: &str = r#"{"version":"2.0","max_updating":{"mac-daemon":1},
    "min_up":{"storage":0,"frontend":0}}"#;

/// The agent's own retry time, which its report counts down.
const RETRY: u64 = 900;

fn mac(name: &str) -> Identity {
    Identity {
        node_id: id(name),
        role: HostRole::MAC_DAEMON,
    }
}

/// A Mac's healthy heartbeat, running `running`, from an agent that announces deferrals.
fn mac_beat(running: &str) -> Heartbeat {
    let mut b = beat(running);
    b.platform = ReleasePlatform::DarwinArm64;
    b.agent_features = vec!["deferred-v1".to_owned()];
    b
}

/// A Mac running `running` that declined `target` because leases still run on it.
fn leases_live(running: &str, target: &str) -> Heartbeat {
    let mut b = mac_beat(running);
    b.deferred = Some(DeferredReport {
        target: version(target),
        reason: Token::parse("leases_live").expect("a token"),
        retry_after_secs: RETRY,
    });
    b
}

fn update(target: &str) -> Reply {
    Reply::Update {
        version: version(target),
    }
}

/// Catches a farm of one Mac that never converges, or a deferral that ends the roll: the
/// Mac behind is ordered, declines for a live lease (the roll is in progress, not blocked,
/// and the Mac is not ordered again during the wait), is ordered again once the agent's wait
/// is over, updates and reports the release, and the farm converges with one `mac-daemon` at
/// the desired version.
#[tokio::test]
async fn the_mac_daemon_role_converges_on_a_fleet_of_one_mac() {
    let rig = Rig::new("mac-converge");
    rig.at(2_000);
    rig.set(MACS).await;
    let c = &rig.coordinator;
    let state = || async { c.convergence(Some(&ci_token()), KBF).await.expect("read") };
    assert_eq!(c.heartbeat(mac("m1"), mac_beat("1.0")), update("2.0"));
    assert_eq!(state().await.state, State::InProgress);

    rig.at(2_005);
    assert_eq!(
        c.heartbeat(mac("m1"), leases_live("1.0", "2.0")),
        Reply::Nothing
    );
    assert_eq!(state().await.state, State::InProgress);
    rig.at(2_005 + RETRY - 1);
    assert_eq!(c.heartbeat(mac("m1"), mac_beat("1.0")), Reply::Nothing);

    rig.at(2_005 + RETRY);
    assert_eq!(c.heartbeat(mac("m1"), mac_beat("1.0")), update("2.0"));
    c.heartbeat(mac("m1"), updating(mac_beat("1.0"), "2.0"));
    assert_eq!(state().await.state, State::InProgress);
    c.heartbeat(mac("m1"), last(mac_beat("2.0"), "2.0", Outcome::Succeeded));
    let done = state().await;
    assert_eq!(done.state, State::Converged);
    let roles: Vec<(&str, usize)> = done
        .roles
        .iter()
        .map(|r| (r.role.as_str(), r.at_desired))
        .collect();
    assert_eq!(roles, [("mac-daemon", 1)]);
}

/// Catches a Mac that keeps its slot while it declines for a live lease: with one Mac
/// updating at a time, the second Mac waits for the slot while the first holds its order,
/// and is ordered as soon as the first reports the deferral.
#[tokio::test]
async fn a_mac_deferred_for_a_live_lease_holds_no_slot() {
    let rig = Rig::new("mac-slot");
    rig.at(2_000);
    rig.set(MACS).await;
    let c = &rig.coordinator;
    assert_eq!(c.heartbeat(mac("m1"), mac_beat("1.0")), update("2.0"));
    assert_eq!(c.heartbeat(mac("m2"), mac_beat("1.0")), Reply::Nothing);
    rig.at(2_005);
    c.heartbeat(mac("m1"), leases_live("1.0", "2.0"));
    assert_eq!(c.heartbeat(mac("m2"), mac_beat("1.0")), update("2.0"));
}
