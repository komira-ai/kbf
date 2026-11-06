//! What kbf's own A/B update puts on the wire when it fails after the switch: the last update a
//! heartbeat carries, as JSON. The coordinator's automatic rollback counts only a failure that
//! names its phase (`after_commit` or `gate_release`), so this report is one it does not count
//! (`kbf-coordinator`, `kbf_rollback_tests::the_agents_own_failure_report_is_held_not_rolled_back`
//! feeds these same bytes to kbf as shipped).

use super::local::LocalError;
use super::testing::VB;
use super::update_tests::Rig;
use crate::ports::Health;

/// The last update of a host whose update to `VB` failed, as the heartbeat sends it.
fn reported(rig: &Rig) -> String {
    serde_json::to_string(&rig.run(VB)).expect("JSON")
}

/// Pins: every way the A/B fails after the switch (B never healthy, the confirm refused, the
/// switch itself failed) reports `{"target","outcome":"failed"}` and nothing else: no `phase`,
/// no `class`, no `reason`. When the agent learns to name the phase, this pin moves with it,
/// and so does the coordinator's test of these bytes.
#[test]
fn a_failure_after_the_switch_is_reported_without_a_phase() {
    let want = format!(r#"{{"target":"{VB}","outcome":"failed"}}"#);

    let never_healthy = Rig::new();
    never_healthy
        .health
        .say(VB, vec![Ok(Health::Waiting("not connected".to_owned()))]);
    assert_eq!(reported(&never_healthy), want, "B never healthy");

    let confirm_refused = Rig::new();
    confirm_refused.local.world().confirm = Err(LocalError::Failed("state_unavailable".to_owned()));
    assert_eq!(reported(&confirm_refused), want, "the confirm refused");

    let switch_failed = Rig::new();
    switch_failed.local.world().switch = Err(LocalError::Failed("switch_failed".to_owned()));
    switch_failed.local.world().revert = Err(LocalError::Refused("nothing_pending".to_owned()));
    assert_eq!(reported(&switch_failed), want, "the switch failed");

    // The control: the same rig with nothing failing reports a success, so the pins above are
    // about failures, not about a rig that never updates.
    assert_eq!(
        reported(&Rig::new()),
        format!(r#"{{"target":"{VB}","outcome":"succeeded"}}"#)
    );
}
