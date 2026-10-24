//! A frontend host's agent: its member drained at the proxy and put back, its health from the
//! proxy and the member's own `healthz`, and the whole update of a frontend host over fakes of
//! the helper, the proxy's pool and the member.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::flags::Timing;
use super::frontend::{FrontendNode, NOT_A_MEMBER, ONLY_FRONTEND};
use super::local::Trial;
use super::status::{Shared, Status};
use super::testing::{COMMIT_A, COMMIT_B, Events, FakeLocal, ManualClock, VA, VB, host, version};
use super::update::{Ports, Update};
use crate::fakes::{FakeFrontend, FakePool, Log, pool_member};
use crate::inventory::{Host, Member, Role};
use crate::ports::{
    Clock, Drain, FrontendControl, Health, HealthCheck, Pool, PoolMember, PortError,
};
use crate::wire::{Outcome, UpdateResult};
use kbf_types::ReleasePlatform;

const NOW: u64 = 10_000;

fn fe_host() -> Host {
    let mut h = host(Role::Frontend);
    h.member = Some(Member {
        instance: "fe1".to_owned(),
        base_port: 18000,
    });
    h
}

/// The pool and the member's API, with every call that matters to the order also noted in the
/// events of the helper's fake.
struct Marked {
    pool: FakePool,
    front: FakeFrontend,
    events: Events,
}

impl Pool for Marked {
    fn view(&self) -> Result<Vec<PoolMember>, PortError> {
        self.pool.view()
    }

    fn drain(&self, name: &str) -> Result<(), PortError> {
        self.events.push(format!("pool:drain {name}"));
        self.pool.drain(name)
    }

    fn undrain(&self, name: &str) -> Result<(), PortError> {
        self.events.push(format!("pool:undrain {name}"));
        self.pool.undrain(name)
    }
}

impl FrontendControl for Marked {
    fn retire(&self, host: &Host) -> Result<crate::ports::RetireStatus, PortError> {
        self.front.retire(host)
    }

    fn retire_status(&self, host: &Host) -> Result<crate::ports::RetireStatus, PortError> {
        self.front.retire_status(host)
    }

    fn commit(&self, host: &Host) -> Result<String, PortError> {
        self.events.push("health:commit");
        self.front.commit(host)
    }
}

struct Rig {
    events: Events,
    pool: FakePool,
    front: FakeFrontend,
    local: FakeLocal,
    node: Arc<FrontendNode>,
    clock: Arc<ManualClock>,
    status: Shared,
    logs: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    /// A frontend host that runs `VA` as member `fe1` of a pool with `fe2`, both up and ready,
    /// whose member reports build B once the helper has switched to it.
    fn new() -> Self {
        let events = Events::default();
        let log = Log::default();
        let pool = FakePool::new(&log);
        pool.set_members(&["fe1", "fe2"]);
        let front = FakeFrontend::new(&log);
        front.script_commit("n1", vec![], Ok(COMMIT_B.to_owned()));
        let marked = Arc::new(Marked {
            pool: pool.clone(),
            front: front.clone(),
            events: events.clone(),
        });
        Self {
            node: Arc::new(FrontendNode::new(marked.clone(), marked)),
            local: FakeLocal::new(&events),
            status: Shared::new(Status::unknown(ReleasePlatform::LinuxX86_64, version(VA))),
            clock: Arc::new(ManualClock::default()),
            logs: Arc::default(),
            events,
            pool,
            front,
        }
    }

    fn run(&self, target: &str) -> UpdateResult {
        let host = fe_host();
        let logs = Arc::clone(&self.logs);
        let log = move |line: &str| logs.lock().unwrap().push(line.to_owned());
        let unix_now = || NOW;
        let update = Update {
            ports: Ports {
                local: &self.local,
                drain: &*self.node,
                health: &*self.node,
                clock: &*self.clock,
                unix_now: &unix_now,
                log: &log,
            },
            host: &host,
            channel: "stable",
            timing: Timing {
                health_timeout: Duration::from_secs(300),
                ..Timing::DEFAULT
            },
            status: &self.status,
        };
        update.run(&version(target))
    }

    fn steps(&self) -> Vec<String> {
        self.events.of(&[
            "local:install",
            "pool:",
            "local:switch",
            "health:commit",
            "local:confirm",
            "local:revert",
        ])
    }

    fn member(&self, name: &str) -> PoolMember {
        self.pool
            .table()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap()
    }

    fn logged(&self, needle: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|l| l.contains(needle))
    }
}

fn outcome(target: &str, outcome: Outcome) -> UpdateResult {
    UpdateResult {
        target: version(target),
        outcome,
    }
}

fn is_message(e: PortError, needle: &str) {
    assert!(e.to_string().contains(needle), "{e}");
}

/// A frontend host's update fetches the release, drains the member at the proxy, switches,
/// asks the member's build until it is the target's, confirms, and only then puts the member
/// back in the pool.
#[test]
fn a_frontend_agent_drains_at_the_proxy_swaps_checks_health_and_undrains_in_order() {
    let rig = Rig::new();
    rig.front
        .script_commit("n1", vec![Ok(COMMIT_A.to_owned())], Ok(COMMIT_B.to_owned()));
    assert_eq!(rig.run(VB), outcome(VB, Outcome::Succeeded));
    assert_eq!(
        rig.steps(),
        [
            format!("local:install {VB} stable"),
            "pool:drain fe1".to_owned(),
            format!("local:switch {VB}"),
            "health:commit".to_owned(),
            "health:commit".to_owned(),
            format!("local:confirm {VB}"),
            "pool:undrain fe1".to_owned(),
        ]
    );
    assert!(!rig.member("fe1").draining);
    assert!(!rig.member("fe2").draining);
}

/// The swap waits for the streams open on the drained member, as the update waits for the leases
/// of a node: it does not switch while one is open, and goes on when none is.
#[test]
fn the_swap_waits_for_the_streams_open_on_the_drained_member() {
    let rig = Rig::new();
    rig.pool.script_streams("fe1", vec![(2, 1), (1, 0)], (0, 0));
    assert_eq!(rig.run(VB), outcome(VB, Outcome::Succeeded));
    assert!(rig.clock.now() >= Duration::from_secs(10));
}

/// A release that does not come up as the target's is reverted, and the member goes back into the
/// pool because A is healthy again; nothing is confirmed.
#[test]
fn a_failed_health_check_reverts_and_puts_the_member_back_when_a_is_healthy() {
    let rig = Rig::new();
    rig.front
        .script_commit("n1", vec![], Ok(COMMIT_A.to_owned()));
    // A reverted host serves A again, which the fake member reports as it did.
    assert_eq!(rig.run(VB), outcome(VB, Outcome::Failed));
    let steps = rig.steps();
    assert_eq!(
        steps[..3],
        [
            format!("local:install {VB} stable"),
            "pool:drain fe1".to_owned(),
            format!("local:switch {VB}"),
        ]
    );
    assert!(steps.contains(&"local:revert".to_owned()), "{steps:?}");
    assert!(
        !steps.iter().any(|s| s.starts_with("local:confirm")),
        "{steps:?}"
    );
    assert_eq!(steps.last().unwrap(), "pool:undrain fe1");
    assert!(!rig.member("fe1").draining);
}

/// When A is not healthy after the revert either (the proxy does not find the member ready), the
/// member is left drained: it is not given back.
#[test]
fn a_member_that_is_not_healthy_on_a_either_stays_drained() {
    let rig = Rig::new();
    rig.pool.set_ready("fe1", false);
    rig.front
        .script_commit("n1", vec![], Ok(COMMIT_A.to_owned()));
    assert_eq!(rig.run(VB), outcome(VB, Outcome::Failed));
    let steps = rig.steps();
    assert!(steps.contains(&"local:revert".to_owned()), "{steps:?}");
    assert!(!steps.contains(&"pool:undrain fe1".to_owned()), "{steps:?}");
    assert!(rig.member("fe1").draining);
    assert!(rig.logged("the node stays drained"));
}

/// The only member that is up and ready is never drained: the update fails before the release is
/// fetched, nothing is switched, the member stays in the pool, and the reason is logged. The
/// other member being drained or not ready is the same as it being absent.
#[test]
fn the_only_frontend_that_is_up_is_not_drained() {
    for other in [None, Some((true, true)), Some((false, false))] {
        let rig = Rig::new();
        match other {
            None => rig.pool.set_members(&["fe1"]),
            Some((draining, ready)) => {
                rig.pool.set_draining("fe2", draining);
                rig.pool.set_ready("fe2", ready);
            }
        }
        assert_eq!(rig.run(VB), outcome(VB, Outcome::Failed), "{other:?}");
        assert_eq!(rig.steps(), Vec::<String>::new(), "{other:?}");
        assert!(rig.logged(ONLY_FRONTEND), "{other:?}");
        assert!(!rig.member("fe1").draining);
    }
}

/// The agent's drain refuses the same way when it is asked directly, whatever was true when the
/// update began: the pool may have changed since, and nothing is drained.
#[test]
fn the_drain_itself_refuses_the_only_frontend_that_is_up() {
    let rig = Rig::new();
    rig.pool.set_ready("fe2", false);
    is_message(
        rig.node
            .drain(&fe_host(), Duration::from_secs(1))
            .unwrap_err(),
        "only member",
    );
    assert!(!rig.member("fe1").draining);
    assert_eq!(rig.events.count("pool:drain"), 0);
}

/// An update the helper has on trial is finished without asking whether another member is up: the
/// member was drained while one was, and it is put back when the release is healthy. A host that
/// runs the target already is a success, with nothing asked of the pool either.
#[test]
fn a_trial_in_progress_and_a_host_at_the_target_need_no_other_member() {
    let rig = Rig::new();
    rig.pool.set_members(&["fe1"]);
    rig.pool.set_draining("fe1", true);
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(Trial {
            version: Some(version(VB)),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(NOW + 600),
        });
    }
    assert_eq!(rig.run(VB), outcome(VB, Outcome::Succeeded));
    assert_eq!(
        rig.steps(),
        [
            "health:commit".to_owned(),
            format!("local:confirm {VB}"),
            "pool:undrain fe1".to_owned(),
        ]
    );

    let rig = Rig::new();
    rig.pool.set_members(&["fe1"]);
    rig.local.world().state.version = Some(version(VB));
    assert_eq!(rig.run(VB), outcome(VB, Outcome::Succeeded));
    assert_eq!(rig.steps(), Vec::<String>::new());
}

/// The leases of the drained member are the streams open on it, both listeners; a member that
/// takes streams again, or the proxy does not list, is an error and not zero.
#[test]
fn the_leases_are_the_streams_open_on_the_drained_member() {
    let rig = Rig::new();
    let h = fe_host();
    rig.node.drain(&h, Duration::from_secs(1)).unwrap();
    rig.pool.script_streams("fe1", vec![(2, 1)], (0, 0));
    assert_eq!(rig.node.leases(&h), Ok(3));
    assert_eq!(rig.node.leases(&h), Ok(0));

    rig.pool.set_draining("fe1", false);
    is_message(rig.node.leases(&h).unwrap_err(), "no longer drained");
    rig.pool.set_members(&["fe2"]);
    is_message(rig.node.leases(&h).unwrap_err(), "no member fe1");
}

/// The undrain puts the member back and checks that the proxy lists it taking streams; a proxy
/// that still lists it drained, or refuses, is an error.
#[test]
fn the_undrain_checks_that_the_proxy_routes_to_the_member_again() {
    let rig = Rig::new();
    let h = fe_host();
    rig.node.drain(&h, Duration::from_secs(1)).unwrap();
    assert_eq!(rig.node.undrain(&h), Ok(()));
    assert!(!rig.member("fe1").draining);

    rig.pool
        .script_views(vec![Ok(vec![pool_member("fe1", true, true, 0)])]);
    is_message(rig.node.undrain(&h).unwrap_err(), "still lists member fe1");
    rig.pool
        .script_undrain("fe1", vec![], Err(PortError::other("refused")));
    is_message(rig.node.undrain(&h).unwrap_err(), "refused");
}

/// The member is healthy when the proxy finds it ready and it was built from the target's
/// commit. Held drained it need not take streams, but asked after the undrain it must. A version
/// without a commit and a member the proxy does not list are abandoned, not waited for; a member
/// that is not ready, or built from another commit, is waited for.
#[test]
fn the_member_is_healthy_when_ready_and_built_from_the_target() {
    let rig = Rig::new();
    let (h, target) = (fe_host(), version(VB));
    assert_eq!(rig.node.check(&h, &target), Ok(Health::Healthy));

    rig.pool.set_draining("fe1", true);
    assert_eq!(rig.node.check_held(&h, &target), Ok(Health::Healthy));
    assert!(matches!(rig.node.check(&h, &target), Ok(Health::Waiting(w)) if w.contains("drained")));
    rig.pool.set_draining("fe1", false);

    rig.pool.set_ready("fe1", false);
    assert!(
        matches!(rig.node.check(&h, &target), Ok(Health::Waiting(w)) if w.contains("not ready"))
    );
    rig.pool.set_ready("fe1", true);

    rig.front
        .script_commit("n1", vec![], Ok(COMMIT_A.to_owned()));
    assert!(
        matches!(rig.node.check(&h, &target), Ok(Health::Waiting(w)) if w.contains("not the target's"))
    );
    rig.front
        .script_commit("n1", vec![], Err(PortError::unreachable("restarting")));
    assert!(rig.node.check(&h, &target).is_err());

    assert!(matches!(
        rig.node.check(&h, &version("0.1.7")),
        Ok(Health::Abandoned(_))
    ));
    rig.pool.set_members(&["fe2"]);
    assert!(
        matches!(rig.node.check(&h, &target), Ok(Health::Abandoned(w)) if w.contains("no member fe1"))
    );
}

/// What is refused before the release is fetched: a pool that cannot be read, a member the
/// proxy does not list, and the only member that is up. A host with a second member up is not
/// refused.
#[test]
fn the_refusals_before_a_fetch() {
    let rig = Rig::new();
    let h = fe_host();
    assert_eq!(rig.node.refusal(&h), None);

    rig.pool.set_ready("fe2", false);
    assert_eq!(rig.node.refusal(&h).as_deref(), Some(ONLY_FRONTEND));
    rig.pool.set_members(&["fe2"]);
    assert!(rig.node.refusal(&h).unwrap().contains("no member fe1"));
    rig.pool
        .script_views(vec![Err(PortError::unreachable("down"))]);
    assert!(rig.node.refusal(&h).unwrap().contains("down"));
}

/// A host the agent was given no member for has nothing to drain: every call says so, and the
/// proxy is not asked.
#[test]
fn a_host_with_no_member_is_refused_by_every_call() {
    let rig = Rig::new();
    let h = host(Role::Frontend);
    is_message(
        rig.node.drain(&h, Duration::from_secs(1)).unwrap_err(),
        NOT_A_MEMBER,
    );
    is_message(rig.node.leases(&h).unwrap_err(), NOT_A_MEMBER);
    is_message(rig.node.undrain(&h).unwrap_err(), NOT_A_MEMBER);
    assert_eq!(rig.node.refusal(&h).as_deref(), Some(NOT_A_MEMBER));
    assert!(rig.node.check(&h, &version(VB)).is_err());
}
