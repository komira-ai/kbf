//! A daemon host's health when its node is connected to one of several frontends and its
//! updater reaches them through a load balancer: the answer of the frontend that does not hold
//! the node ("not connected", or 404) must not decide it. Each test scripts the answer of
//! connection `n`, as the balancer's choice, and counts connections: nothing waits or measures
//! time.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::fakes::{FakeInstaller, Log};
use crate::inventory::{Host, Role};
use crate::node_health::{FRONTEND_SAMPLES, NodeHealth};
use crate::operator::OperatorApi;
use crate::ports::{Health, HealthCheck, PortError};
use crate::testutil::{FakeApi, KEY, TOKEN, fake_api_by, file, http, scratch};
use crate::version::Version;

const TARGET: &str = "0.1.7+g0123456789ab-3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn host(role: Role) -> Host {
    Host {
        alias: "mac-x".to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role,
        host_key: KEY.to_owned(),
        node_id: Some("mac-x".to_owned()),
        member: None,
    }
}

fn target() -> Version {
    Version::parse(TARGET).expect("version")
}

/// What a frontend answers for the node: connected here, or held by another frontend.
fn here(connected: bool, placement: &str) -> Vec<u8> {
    http(
        200,
        &json!({"node_id": "mac-x", "connected": connected, "protocol": 1, "software": null,
                "needs_attention": [], "placement": {"state": placement}, "leases": []})
        .to_string(),
    )
}

/// The same frontends, as the updater's client sees them: `answer(n)` for connection `n`.
fn balancer(answer: impl Fn(usize) -> Vec<u8> + Send + 'static) -> FakeApi {
    fake_api_by(move |n, _| Some(answer(n)))
}

fn api(server: &FakeApi, name: &str) -> OperatorApi {
    let token = file(&scratch(name), "token", TOKEN, 0o600);
    OperatorApi::new(server.addr, token)
        .expect("api")
        .with_timeout(Duration::from_millis(500))
}

fn installer() -> Arc<FakeInstaller> {
    let installer = FakeInstaller::new(&Log::default());
    installer.set_identity("mac-x", Some("mac-x"), Some(COMMIT));
    Arc::new(installer)
}

fn updater_side(server: &FakeApi, name: &str) -> NodeHealth {
    NodeHealth::for_updater(api(server, name), installer())
}

fn waiting(health: &Result<Health, PortError>) -> String {
    match health {
        Ok(Health::Waiting(why)) => why.clone(),
        other => panic!("expected waiting, got {other:?}"),
    }
}

/// Catches the flap: the node is connected to the frontend that answers on the odd
/// connections, and the other frontend (even connections) says "not connected" or "no such
/// node". Whichever answers first, a Mac and a Linux daemon are healthy, held or not, and the
/// asking stops at the answer that says connected.
#[test]
fn a_node_connected_to_the_other_frontend_is_healthy_whichever_answers_first() {
    for role in [Role::MacDaemon, Role::Daemon] {
        for (first_miss, name) in [
            (here(false, "serving"), "nhf-not-connected"),
            (http(404, "{\"error\":\"no_node\"}"), "nhf-404"),
        ] {
            for held_first in [false, true] {
                let miss = first_miss.clone();
                let hit = here(true, "serving");
                let server = balancer(move |n| {
                    if (n % 2 == 0) == held_first {
                        hit.clone()
                    } else {
                        miss.clone()
                    }
                });
                let health = updater_side(&server, name);
                let mac = host(role);
                assert_eq!(
                    health.check(&mac, &target()),
                    Ok(Health::Healthy),
                    "{role:?}"
                );
                assert_eq!(health.check_held(&mac, &target()), Ok(Health::Healthy));
                assert!(
                    server.requests().len() <= 4,
                    "{:?}",
                    server.requests().len()
                );
            }
        }
    }
}

/// Catches a node that no frontend holds being called healthy by the sampling, and a wait
/// that hides why: every answer says "not connected" (or none knows the node), so the check
/// waits with the reason, after asking exactly [`FRONTEND_SAMPLES`] times.
#[test]
fn a_node_no_frontend_holds_waits_after_every_sample() {
    let server = balancer(|_| here(false, "serving"));
    let result = updater_side(&server, "nhf-none").check(&host(Role::MacDaemon), &target());
    assert!(waiting(&result).contains("mac-x is not connected"));
    assert_eq!(server.requests().len(), FRONTEND_SAMPLES);
    let server = balancer(|_| http(404, "{}"));
    let result = updater_side(&server, "nhf-404s").check(&host(Role::MacDaemon), &target());
    assert!(waiting(&result).contains("has not registered with the frontend"));
    assert_eq!(server.requests().len(), FRONTEND_SAMPLES);
}

/// Catches the connected answer being judged less strictly after sampling: the frontend that
/// holds the node says it is cordoned, so the check waits with that, and a held node is
/// still healthy.
#[test]
fn the_frontend_that_holds_the_node_decides_whether_it_serves() {
    let server = balancer(|n| {
        if n % 2 == 0 {
            here(false, "serving")
        } else {
            here(true, "cordoned")
        }
    });
    let health = updater_side(&server, "nhf-cordoned");
    let mac = host(Role::MacDaemon);
    assert!(waiting(&health.check(&mac, &target())).contains("mac-x is cordoned, not serving"));
    assert_eq!(health.check_held(&mac, &target()), Ok(Health::Healthy));
}

/// Catches a frontend that failed once sinking the check: the first connection gets a 503,
/// the next is the frontend that holds the node.
#[test]
fn a_frontend_that_errs_does_not_hide_the_one_that_holds_the_node() {
    let server = balancer(|n| {
        if n == 0 {
            http(503, "{}")
        } else {
            here(true, "serving")
        }
    });
    let result = updater_side(&server, "nhf-err").check(&host(Role::MacDaemon), &target());
    assert_eq!(result, Ok(Health::Healthy));
    let server = balancer(|_| http(503, "{}"));
    let result = updater_side(&server, "nhf-always-err").check(&host(Role::MacDaemon), &target());
    assert!(result.expect_err("every answer failed").is_unreachable());
}

/// Catches the sampling reaching a client that talks to one frontend (the operator token's
/// loopback API): there one "not connected" is the answer, asked once.
#[test]
fn a_client_of_one_frontend_asks_once() {
    let server = balancer(|_| here(false, "serving"));
    let health = NodeHealth::with_api(api(&server, "nhf-one"), installer());
    assert!(waiting(&health.check(&host(Role::MacDaemon), &target())).contains("not connected"));
    assert_eq!(server.requests().len(), 1);
}
