use std::sync::Arc;

use kbf_oidc::testkit::{STATIC_ISSUER, good_claims, keys, mint, mint_with, static_verifier};
use kbf_types::HostRole;
use kbf_updater::wire::Reply;
use serde_json::json;

use crate::desired::DesiredStore;
use crate::identity::Identity;
use crate::service::{Coordinator, SetError, Unauthorized};
use crate::testutil::{OPERATOR, Rig, beat, ci_token, clock_of, id, updating, version};

const STATE: &str = r#"{"version":"2.0","max_updating":{"daemon":1},
    "min_up":{"storage":1,"frontend":1}}"#;

fn who(name: &str, role: HostRole) -> Identity {
    Identity {
        node_id: id(name),
        role,
    }
}

fn update(target: &str) -> Reply {
    Reply::Update {
        version: version(target),
    }
}

fn refused(result: Result<crate::desired::Desired, SetError>) -> &'static str {
    match result {
        Err(SetError::Unauthorized(why)) => why,
        other => panic!("not refused as unauthorized: {other:?}"),
    }
}

/// Catches: a coordinator with no desired state telling a host to do something, and a
/// heartbeat not being noted (the status would not list the host).
#[tokio::test]
async fn with_no_desired_state_every_host_is_told_nothing_and_is_noted() {
    let rig = Rig::new("svc-none");
    rig.at(2_000);
    let reply = rig
        .coordinator
        .heartbeat(who("d1", HostRole::Daemon), beat("1.0"));
    assert_eq!(reply, Reply::Nothing);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.desired.is_none());
    assert_eq!(status.hosts.len(), 1);
    assert_eq!(status.hosts[0].node_id, "d1");
    assert_eq!(status.hosts[0].role, "daemon");
}

/// Catches: the OIDC call not setting the state the decision uses: with a valid token the
/// state is stored and the next heartbeat of a host behind is told to update.
#[tokio::test]
async fn a_valid_ci_token_sets_the_state_hosts_are_told_about() {
    let rig = Rig::new("svc-set");
    rig.at(2_000);
    let token = ci_token();
    let set = rig
        .coordinator
        .set_desired(Some(&token), STATE.as_bytes())
        .await;
    assert_eq!(set.expect("set").version.as_str(), "2.0");
    let reply = rig
        .coordinator
        .heartbeat(who("d1", HostRole::Daemon), beat("1.0"));
    assert_eq!(reply, update("2.0"));
}

/// Catches: a token accepted that the allow-list refuses: a wrong repository, a wrong ref, a
/// wrong workflow, a wrong environment, a token signed by another key, one for another
/// audience, an expired one, and the same token twice; each is refused before the state is
/// read, and the state stays what it was.
#[tokio::test]
async fn a_token_the_allow_list_refuses_sets_nothing() {
    let rig = Rig::new("svc-refused");
    rig.at(2_000);
    let k = keys();
    let sign = |claims: &serde_json::Value| mint(&k[0], claims);
    let claims = || good_claims(STATIC_ISSUER);
    let with = |name: &str, value: serde_json::Value| {
        let mut c = claims();
        c[name] = value;
        sign(&c)
    };
    for (token, why) in [
        (with("repository_id", json!("1")), "repository"),
        (with("ref", json!("refs/heads/other")), "ref"),
        (
            with(
                "job_workflow_ref",
                json!("owner/repo/.github/workflows/x.yml@refs/heads/main"),
            ),
            "workflow",
        ),
        (with("environment", json!("production")), "environment"),
        (with("aud", json!("someone-else")), "audience"),
        (with("exp", json!(1)), "expired"),
        (
            mint_with(
                &k[1],
                &json!({"alg": "RS256", "typ": "JWT", "kid": "k1"}),
                &claims(),
            ),
            "signature",
        ),
        (mint(&k[1], &claims()), "unknown_key"),
    ] {
        let set = rig
            .coordinator
            .set_desired(Some(&token), STATE.as_bytes())
            .await;
        assert_eq!(refused(set), why);
    }
    let again = ci_token();
    rig.coordinator
        .set_desired(Some(&again), STATE.as_bytes())
        .await
        .expect("the first use is accepted");
    let replay = rig
        .coordinator
        .set_desired(Some(&again), STATE.as_bytes())
        .await;
    assert_eq!(refused(replay), "replayed");
}

/// Catches: the operator token opening the call that only CI's token may make: it is refused
/// as the bearer of the OIDC call (it is no signed token), and so are no bearer at all, an
/// empty one and one longer than a token is.
#[tokio::test]
async fn the_operator_token_does_not_set_the_state() {
    let rig = Rig::new("svc-operator");
    for bearer in [Some(OPERATOR), None, Some(""), Some(&"a".repeat(9_000)[..])] {
        let set = rig.coordinator.set_desired(bearer, STATE.as_bytes()).await;
        assert!(matches!(set, Err(SetError::Unauthorized(_))), "{bearer:?}");
    }
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.desired.is_none());
}

/// Catches: the state parsed before the caller is known (a probe would learn what the schema
/// is), and a body that is no state being stored: with a valid token and a bad body the call
/// fails as invalid and the old state stands.
#[tokio::test]
async fn a_valid_token_with_a_bad_body_sets_nothing() {
    let rig = Rig::new("svc-invalid");
    rig.set(STATE).await;
    let token = ci_token();
    let set = rig.coordinator.set_desired(Some(&token), b"{}").await;
    assert!(matches!(set, Err(SetError::Invalid(_))));
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.desired.expect("kept").version.as_str(), "2.0");
    let garbage = rig.coordinator.set_desired(None, b"{}").await;
    assert!(matches!(garbage, Err(SetError::Unauthorized(_))));
}

/// Catches: an issuer that cannot be reached being reported as a bad token (CI would stop
/// retrying a call that is only waiting for GitHub), and a state file that cannot be
/// written being reported as success.
#[tokio::test]
async fn failures_that_are_not_the_callers_are_told_apart() {
    let rig = Rig::with_unreachable_issuer("svc-down");
    let set = rig
        .coordinator
        .set_desired(Some(&ci_token()), STATE.as_bytes())
        .await;
    assert!(matches!(set, Err(SetError::Unavailable)));
    assert!(SetError::Unavailable.to_string().contains("unavailable"));

    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "svc-fail");
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let (verifier, _) = static_verifier();
    let store =
        DesiredStore::open(&dir.path().join("missing").join("desired.json")).expect("store");
    let coordinator = Coordinator::new(store, verifier, OPERATOR.to_owned(), clock_of(&clock));
    let set = coordinator
        .set_desired(Some(&ci_token()), STATE.as_bytes())
        .await;
    assert!(matches!(set, Err(SetError::Store(_))));
}

/// Catches: the status readable without the operator token, or with a CI token, or with a
/// near miss of the operator token; and the status leaving out what an operator reads: the
/// desired state, the quiet period, and for each host its liveness, slot and last word.
#[test]
fn the_status_is_for_the_operator_token_only() {
    let rig = Rig::new("svc-status");
    let near = format!("{OPERATOR}x");
    for bearer in [
        None,
        Some(""),
        Some(&near[..]),
        Some("operator"),
        Some(&ci_token()[..]),
    ] {
        assert_eq!(
            rig.coordinator.status(bearer).err(),
            Some(Unauthorized),
            "{bearer:?}"
        );
    }
    rig.at(1_010);
    rig.coordinator
        .heartbeat(who("d1", HostRole::Daemon), updating(beat("1.0"), "2.0"));
    rig.at(1_020);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(
        status.quiet,
        "20 s since the start is within the quiet period"
    );
    let host = &status.hosts[0];
    assert_eq!(
        (host.live, host.up, host.holds_slot, host.silent_secs),
        (true, false, true, 10)
    );
    rig.at(1_100);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(!status.quiet);
    assert!(!status.hosts[0].live);
    let text = serde_json::to_value(&status).expect("json");
    assert_eq!(text["hosts"][0]["version"], "1.0");
    assert_eq!(text["hosts"][0]["updating"], "2.0");
    assert_eq!(text["hosts"][0]["platform"], "linux-x86_64");
}

/// Catches: a restart forgetting what the hosts said: after a restart the coordinator orders
/// nothing for 30 seconds, then counts a host that heartbeats as updating against its role's
/// limit, so a second daemon is not started beside it; and the desired state is the one set
/// before the restart.
#[tokio::test]
async fn after_a_restart_a_host_that_was_updating_is_still_counted() {
    let mut rig = Rig::new("svc-restart");
    rig.at(2_000);
    rig.set(STATE).await;
    assert_eq!(
        rig.coordinator
            .heartbeat(who("d1", HostRole::Daemon), beat("1.0")),
        update("2.0")
    );
    rig.at(3_000);
    rig.restart();
    let busy = updating(beat("1.0"), "2.0");
    let early = rig
        .coordinator
        .heartbeat(who("d1", HostRole::Daemon), busy.clone());
    assert_eq!(early, Reply::Nothing);
    rig.at(3_029);
    let d2 = || who("d2", HostRole::Daemon);
    assert_eq!(
        rig.coordinator.heartbeat(d2(), beat("1.0")),
        Reply::Nothing,
        "quiet"
    );
    rig.at(3_030);
    rig.coordinator.heartbeat(who("d1", HostRole::Daemon), busy);
    assert_eq!(
        rig.coordinator.heartbeat(d2(), beat("1.0")),
        Reply::Nothing,
        "d1 holds the only daemon slot"
    );
    rig.coordinator
        .heartbeat(who("d1", HostRole::Daemon), beat("2.0"));
    assert_eq!(rig.coordinator.heartbeat(d2(), beat("1.0")), update("2.0"));
}

/// Catches: the coordinator's debug text leaking its operator token.
#[test]
fn the_coordinators_debug_text_holds_no_secret() {
    let rig = Rig::new("svc-debug");
    let text = format!("{:?}", rig.coordinator);
    assert!(!text.contains(OPERATOR), "{text}");
}
