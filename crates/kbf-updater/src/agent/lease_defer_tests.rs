//! A Mac whose helper refuses the switch because leases still run on it (`leases_live`): the
//! agent's side of the deferral. The frontend's count reads 0, so the drain ends; the
//! helper's own count does not, so nothing is switched. Time is two clocks a test moves: the
//! agent's and the Unix one (`retry_after`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_types::ReleasePlatform;

use super::flags::Timing;
use super::local::LocalError;
use super::run::{Agent, World};
use super::status::Status;
use super::testing::{
    Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA, VB, host, version,
};
use crate::inventory::Role;
use crate::ports::Health;
use crate::wire::{Heartbeat, Outcome, Reply, Token, UpdateResult};

const T0: u64 = 10_000;
const RETRY: u64 = 900;

struct Rig {
    events: Events,
    coordinator: FakeCoordinator,
    local: FakeLocal,
    drain: FakeDrain,
    health: FakeHealth,
    clock: Arc<ManualClock>,
    unix: Arc<AtomicU64>,
    dir: std::path::PathBuf,
}

impl Rig {
    /// A Mac that runs `VA`, offered `VB`, whose node the frontend counts idle and whose
    /// helper counts a lease still running.
    fn new(name: &str) -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        let local = FakeLocal::new(&events);
        local.world().switch = Err(LocalError::Refused("leases_live".to_owned()));
        Self {
            coordinator: FakeCoordinator::default(),
            local,
            drain: FakeDrain::new(&events),
            health,
            clock: Arc::new(ManualClock::default()),
            unix: Arc::new(AtomicU64::new(T0)),
            dir: crate::testutil::scratch(name),
            events,
        }
    }

    fn world(&self) -> World {
        let unix = Arc::clone(&self.unix);
        World {
            coordinator: Arc::new(self.coordinator.clone()),
            local: Arc::new(self.local.clone()),
            drain: Arc::new(self.drain.clone()),
            health: Arc::new(self.health.clone()),
            clock: self.clock.clone(),
            unix_now: Arc::new(move || unix.load(Ordering::SeqCst)),
            renewer: None,
            log: Arc::new(|_| {}),
        }
    }

    /// The agent on a Mac, announcing `deferred-v1` and `halt-v1`.
    fn agent(&self) -> Agent {
        let agent = Agent::new(
            host(Role::MacDaemon),
            "stable".to_owned(),
            Timing {
                retry: Duration::from_secs(RETRY),
                ..Timing::DEFAULT
            },
            self.world(),
            Status::unknown(ReleasePlatform::DarwinArm64, version("0")),
        )
        .with_host_updates(self.dir.clone());
        agent.refresh();
        agent
    }

    /// A heartbeat, and then the order its reply holds, the way the two threads do it.
    fn take_order(&self, agent: &Agent) {
        self.coordinator.reply(Ok(Reply::Update {
            version: version(VB),
        }));
        let order = agent.beat_once().expect("an order");
        agent.order(order.0.as_deref(), &order.1);
    }

    fn last_beat(&self) -> Heartbeat {
        self.coordinator
            .beats()
            .pop()
            .expect("a heartbeat was sent")
    }

    fn switches(&self) -> usize {
        self.events.count("local:switch")
    }

    fn advance(&self, secs: u64) {
        self.clock.advance(Duration::from_secs(secs));
        self.unix.fetch_add(secs, Ordering::SeqCst);
    }
}

fn token(text: &str) -> Token {
    Token::parse(text).expect("a token")
}

/// Catches a live lease on a Mac taken as a failed release, or one that keeps the host's
/// slot: with `deferred-v1` advertised, a switch the helper refused `leases_live` is a
/// `deferred` of the target with the reason `leases_live` and the whole retry time, no
/// result and nothing updating; nothing is reverted (nothing was switched) and the node is
/// left drained, as a drain bound leaves it. The same order is declined until the retry time
/// has come, and then, the leases over, the release is switched to, confirmed and the node
/// given back.
#[test]
fn an_order_with_a_live_lease_is_deferred_and_holds_no_slot() {
    let rig = Rig::new("lease-defer");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.agent();
    rig.take_order(&agent);
    agent.beat_once();
    let beat = rig.last_beat();
    let report = beat.deferred.expect("a deferral");
    assert_eq!(report.target, version(VB));
    assert_eq!(report.reason, token("leases_live"));
    assert_eq!(report.retry_after_secs, RETRY);
    assert_eq!(beat.last_update, None);
    assert_eq!(beat.updating, None);
    assert_eq!(rig.events.count("local:revert"), 0);
    assert!(rig.drain.is_drained());

    rig.take_order(&agent);
    assert_eq!(rig.switches(), 1, "declined while deferred");

    rig.advance(RETRY);
    rig.local.world().switch = Ok(T0 + RETRY + 600);
    rig.take_order(&agent);
    assert_eq!(rig.switches(), 2);
    agent.beat_once();
    let beat = rig.last_beat();
    assert_eq!(beat.deferred, None);
    assert_eq!(
        beat.last_update,
        Some(UpdateResult::new(version(VB), Outcome::Succeeded))
    );
    assert_eq!(rig.events.count(&format!("local:confirm {VB}")), 1);
    assert!(!rig.drain.is_drained());
}

/// Catches a deferral reported to a coordinator that never advertised one: without
/// `deferred-v1` the same refusal is a plain `Failed` of the target with no `deferred`, as
/// any switch the helper refuses was before, and still nothing is reverted.
#[test]
fn without_deferred_v1_a_live_lease_on_a_mac_is_a_plain_failure() {
    let rig = Rig::new("lease-plain");
    rig.coordinator.advertise(&[]);
    let agent = rig.agent();
    rig.take_order(&agent);
    agent.beat_once();
    let beat = rig.last_beat();
    assert_eq!(beat.deferred, None);
    assert_eq!(
        beat.last_update,
        Some(UpdateResult::new(version(VB), Outcome::Failed))
    );
    assert_eq!(rig.events.count("local:revert"), 0);
}

/// Every code the previous release's helper (v0.0.63) refuses a switch with: it has no
/// `leases_live`.
const PREVIOUS_HELPER_CODES: [&str; 22] = [
    "bad_command",
    "channel_not_allowed",
    "config_invalid",
    "config_unsafe",
    "unknown_release",
    "ambiguous_release",
    "fetch_failed",
    "verify_failed",
    "pending",
    "not_staged",
    "nothing_pending",
    "no_current",
    "busy",
    "stage_failed",
    "switch_failed",
    "revert_failed",
    "state_unavailable",
    "audit_unavailable",
    "peer_not_ready",
    "runtime_failed",
    "not_kept",
    "restart_failed",
];

/// Catches this agent taking a previous helper's refusal for a deferral: against a helper of
/// the previous release, which never sends `leases_live`, a switch refused with any code that
/// helper has ends as it did, even with `deferred-v1` advertised: no deferral, a `Failed` of
/// the target, and the agent goes back (it asks the helper to revert).
#[test]
fn with_a_previous_helper_a_refused_switch_still_goes_back_as_before() {
    for code in PREVIOUS_HELPER_CODES {
        let rig = Rig::new(&format!("lease-prev-{code}"));
        rig.local.world().switch = Err(LocalError::Refused(code.to_owned()));
        rig.coordinator.advertise(&["deferred-v1"]);
        let agent = rig.agent();
        rig.take_order(&agent);
        agent.beat_once();
        let beat = rig.last_beat();
        assert_eq!(beat.deferred, None, "{code}");
        assert_eq!(
            beat.last_update,
            Some(UpdateResult::new(version(VB), Outcome::Failed)),
            "{code}"
        );
        assert_eq!(rig.events.count("local:revert"), 1, "{code}");
    }
}
