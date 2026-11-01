use std::sync::Arc;

use kbf_oidc::testkit::{
    STATIC_ISSUER, config_text, good_claims, keys, mint, mint_with, static_verifier,
};
use kbf_types::HostRole;
use kbf_updater::wire::{Outcome, Reply};
use serde_json::json;

use crate::converge::State;
use crate::desired::DesiredStore;
use crate::desired::KBF;
use crate::fleet::ORDER_AGE;
use crate::identity::Identity;
use crate::service::{CiRefused, ConvergenceError, Coordinator, SetError, Unauthorized};
use crate::services::Services;
use crate::testutil::{OPERATOR, Rig, beat, ci_token, clock_of, id, last, updating, version};

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
        .heartbeat(who("d1", HostRole::DAEMON), beat("1.0"));
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
        .heartbeat(who("d1", HostRole::DAEMON), beat("1.0"));
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
        .heartbeat(who("d1", HostRole::DAEMON), updating(beat("1.0"), "2.0"));
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
            .heartbeat(who("d1", HostRole::DAEMON), beat("1.0")),
        update("2.0")
    );
    rig.at(3_000);
    rig.restart();
    let busy = updating(beat("1.0"), "2.0");
    let early = rig
        .coordinator
        .heartbeat(who("d1", HostRole::DAEMON), busy.clone());
    assert_eq!(early, Reply::Nothing);
    rig.at(3_029);
    let d2 = || who("d2", HostRole::DAEMON);
    assert_eq!(
        rig.coordinator.heartbeat(d2(), beat("1.0")),
        Reply::Nothing,
        "quiet"
    );
    rig.at(3_030);
    rig.coordinator.heartbeat(who("d1", HostRole::DAEMON), busy);
    assert_eq!(
        rig.coordinator.heartbeat(d2(), beat("1.0")),
        Reply::Nothing,
        "d1 holds the only daemon slot"
    );
    rig.coordinator
        .heartbeat(who("d1", HostRole::DAEMON), beat("2.0"));
    assert_eq!(rig.coordinator.heartbeat(d2(), beat("1.0")), update("2.0"));
}

/// Catches: the coordinator's debug text leaking its operator token.
#[test]
fn the_coordinators_debug_text_holds_no_secret() {
    let rig = Rig::new("svc-debug");
    let text = format!("{:?}", rig.coordinator);
    assert!(!text.contains(OPERATOR), "{text}");
}

/// Catches: the convergence read opening to anyone but CI: no bearer, an empty one, a long
/// one, the operator token, a token signed by another key, and a CI token used a second
/// time are each refused; and the CI token is refused by the operator's status read.
#[tokio::test]
async fn the_convergence_is_read_with_a_ci_token_and_nothing_else() {
    let rig = Rig::new("svc-converge-auth");
    rig.at(2_000);
    let k = keys();
    let other = mint(&k[1], &good_claims(STATIC_ISSUER));
    for (bearer, why) in [
        (None, "missing"),
        (Some(""), "malformed"),
        (Some(OPERATOR), "malformed"),
        (Some(other.as_str()), "unknown_key"),
    ] {
        let read = rig.coordinator.convergence(bearer, KBF).await;
        assert_eq!(
            read.map(|_| ()),
            Err(ConvergenceError::Refused(CiRefused::Unauthorized(why))),
            "{bearer:?}"
        );
    }
    let long = "a".repeat(9_000);
    assert!(matches!(
        rig.coordinator.convergence(Some(&long), KBF).await,
        Err(ConvergenceError::Refused(CiRefused::Unauthorized(
            "missing"
        )))
    ));
    let token = ci_token();
    rig.coordinator
        .convergence(Some(&token), KBF)
        .await
        .expect("the first use is accepted");
    assert_eq!(
        rig.coordinator
            .convergence(Some(&token), KBF)
            .await
            .map(|_| ()),
        Err(ConvergenceError::Refused(CiRefused::Unauthorized(
            "replayed"
        )))
    );
    assert_eq!(
        rig.coordinator.status(Some(&ci_token())).map(|_| ()),
        Err(Unauthorized)
    );
}

/// Catches: the convergence read answering from anything but the heartbeats and the stored
/// desired state: with none set it is in progress; once set, hosts that heartbeat move it
/// from in progress to converged; reading it sets no state and orders nothing.
#[tokio::test]
async fn the_convergence_follows_the_heartbeats() {
    let rig = Rig::new("svc-converge");
    rig.at(2_000);
    let none = rig.coordinator.convergence(Some(&ci_token()), KBF).await;
    let none = none.expect("read");
    assert_eq!((none.desired, none.state), (None, State::InProgress));
    rig.set(STATE).await;
    let d1 = || who("d1", HostRole::DAEMON);
    rig.coordinator.heartbeat(d1(), beat("1.0"));
    let behind = rig.coordinator.convergence(Some(&ci_token()), KBF).await;
    assert_eq!(behind.expect("read").state, State::InProgress);
    rig.coordinator.heartbeat(d1(), beat("2.0"));
    let short = rig.coordinator.convergence(Some(&ci_token()), KBF).await;
    assert_eq!(
        short.expect("read").state,
        State::InProgress,
        "STATE keeps one storage host and one frontend up, and none has been heard from"
    );
    for (name, role) in [("st1", HostRole::STORAGE), ("fe1", HostRole::FRONTEND)] {
        rig.coordinator.heartbeat(who(name, role), beat("2.0"));
    }
    let done = rig.coordinator.convergence(Some(&ci_token()), KBF).await;
    let done = done.expect("read");
    assert_eq!(done.state, State::Converged);
    let daemons = done
        .roles
        .iter()
        .find(|r| r.role == "daemon")
        .expect("daemon");
    assert_eq!(daemons.at_desired, 1);
}

// ---- an order holds its slot until the host reports ------------------------------------

const ONE_STORAGE: &str = r#"{"version":"2.0","max_updating":{"storage":1},
    "min_up":{"storage":1,"frontend":1}}"#;

/// Two storage hosts may update at once; the minimum up is one.
const TWO_STORAGE: &str = r#"{"version":"2.0","max_updating":{"storage":2},
    "min_up":{"storage":1,"frontend":1}}"#;

fn storage(name: &str) -> Identity {
    who(name, HostRole::STORAGE)
}

/// A rig past the quiet start with `state` set.
async fn started(name: &str, state: &str) -> Rig {
    let rig = Rig::new(name);
    rig.at(2_000);
    rig.set(state).await;
    rig.at(2_100);
    rig
}

/// Catches: the limit counted only from heartbeats, so every host that beats before the
/// ordered one reports is ordered too (four of six storage hosts restarted within seconds
/// with a limit of one).
#[tokio::test]
async fn hosts_that_beat_before_the_ordered_one_reports_are_not_ordered() {
    let rig = started("svc-order-one", ONE_STORAGE).await;
    let c = &rig.coordinator;
    let replies: Vec<kbf_updater::wire::ServiceReply> = ["s1", "s2", "s3"]
        .iter()
        .map(|h| c.heartbeat(storage(h), beat("1.0")).reply)
        .collect();
    assert_eq!(
        replies,
        [Reply::Nothing, update("2.0"), Reply::Nothing]
            .map(Into::<kbf_updater::wire::ServiceReply>::into)
    );
    assert_eq!(c.heartbeat(storage("s1"), beat("1.0")), Reply::Nothing);
    assert_eq!(c.heartbeat(storage("s3"), beat("1.0")), Reply::Nothing);
}

/// Catches: the slot freeing before the host is done, or never freeing: it is held while the
/// host reports updating, and the next host is ordered once it reports the new version.
#[tokio::test]
async fn the_slot_frees_when_the_ordered_host_is_done() {
    let rig = started("svc-order-done", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    c.heartbeat(storage("s2"), updating(beat("1.0"), "2.0"));
    assert_eq!(c.heartbeat(storage("s3"), beat("1.0")), Reply::Nothing);
    let done = last(beat("2.0"), "2.0", Outcome::Succeeded);
    c.heartbeat(storage("s2"), done);
    assert_eq!(c.heartbeat(storage("s3"), beat("1.0")), update("2.0"));
}

/// Catches: a failed update leaving the order holding the slot after the host said so, when
/// it is healthy again it is the failure rule that holds the slot, not the order.
#[tokio::test]
async fn a_failed_report_ends_the_order() {
    let rig = started("svc-order-failed", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    c.heartbeat(storage("s2"), last(beat("1.0"), "2.0", Outcome::Failed));
    assert_eq!(
        c.heartbeat(storage("s3"), beat("1.0")),
        update("2.0"),
        "s2 is healthy and failed: it holds no slot and is up, and its order is over"
    );
}

/// Catches: a report of an earlier success, or of a failure of some other version, ending
/// the order of the version asked for, which would give the slot away while the ordered
/// update is still to come.
#[tokio::test]
async fn a_report_of_another_update_keeps_the_order() {
    let rig = started("svc-order-other-failed", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    for report in [
        last(beat("1.0"), "0.9", Outcome::Succeeded),
        last(beat("1.0"), "1.5", Outcome::Failed),
    ] {
        c.heartbeat(storage("s2"), report);
        assert_eq!(
            c.heartbeat(storage("s3"), beat("1.0")),
            Reply::Nothing,
            "s2 still holds the slot it was ordered"
        );
    }
}

/// Catches: an order for a host that never reports holding its slot for ever.
#[tokio::test]
async fn an_order_nobody_reports_ages_out() {
    let rig = started("svc-order-age", ONE_STORAGE).await;
    let c = &rig.coordinator;
    let age = ORDER_AGE.as_secs();
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    rig.at(2_100 + age - 1);
    // Both beat afresh, so neither is down: only the order can refuse them.
    assert_eq!(c.heartbeat(storage("s1"), beat("1.0")), Reply::Nothing);
    assert_eq!(c.heartbeat(storage("s3"), beat("1.0")), Reply::Nothing);
    rig.at(2_100 + age);
    assert_eq!(c.heartbeat(storage("s1"), beat("1.0")), update("2.0"));
}

/// Catches: the minimum of hosts up counted from heartbeats alone: with room for two
/// updates and one host that must stay up, the host that was just ordered is not up, so the
/// other is not ordered.
#[tokio::test]
async fn an_ordered_host_is_not_counted_as_up() {
    let rig = started("svc-order-min-up", TWO_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    assert_eq!(c.heartbeat(storage("s1"), beat("1.0")), Reply::Nothing);
}

/// Catches: a host that asks again before it reports (it did not hear the reply) being
/// refused, or its own order counting against it, or the repeat taking a second slot: it is
/// told the same again, and the others still wait.
#[tokio::test]
async fn a_host_that_asks_again_is_told_the_same_order() {
    let rig = started("svc-order-again", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    assert_eq!(c.heartbeat(storage("s3"), beat("1.0")), Reply::Nothing);
    c.heartbeat(storage("s2"), updating(beat("1.0"), "2.0"));
    assert_eq!(
        c.heartbeat(storage("s2"), updating(beat("1.0"), "2.0")),
        Reply::Nothing
    );
}

/// Catches: a re-beating host extending its own hold (the order time must stay the first
/// one), and Fleet::order restarting the clock on an order for the same target.
#[tokio::test]
async fn a_repeat_does_not_extend_the_hold() {
    let rig = started("svc-order-keep-time", ONE_STORAGE).await;
    let c = &rig.coordinator;
    let age = ORDER_AGE.as_secs();
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    rig.at(2_100 + age - 1);
    c.heartbeat(storage("s1"), beat("1.0"));
    c.heartbeat(storage("s3"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    rig.at(2_100 + age);
    assert_eq!(
        c.heartbeat(storage("s1"), beat("1.0")),
        update("2.0"),
        "the order was given at the first ask; the repeat did not move it"
    );
}

/// Catches: the operator status reading heartbeats alone, so it shows an ordered host as up
/// and not holding a slot while the coordinator refuses others because of it.
#[tokio::test]
async fn the_status_shows_an_ordered_host_as_holding_a_slot() {
    let rig = started("svc-order-status", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    let status = c.status(Some(OPERATOR)).expect("status");
    let of = |n: &str| status.hosts.iter().find(|h| h.node_id == n).expect("host");
    assert!(of("s2").holds_slot && !of("s2").up);
    assert!(!of("s1").holds_slot && of("s1").up);
}

/// Catches: any failure report ending an order: a host that reports an older update to a
/// different version as failed, while still at the old version and not yet updating to the
/// ordered one, has not reported the order, so it still holds its slot.
#[tokio::test]
async fn a_failure_for_another_target_does_not_end_the_order() {
    let rig = started("svc-order-other-failure", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    c.heartbeat(storage("s2"), last(beat("1.0"), "1.5", Outcome::Failed));
    assert_eq!(
        c.heartbeat(storage("s3"), beat("1.0")),
        Reply::Nothing,
        "s2's report is about 1.5; its order for 2.0 still holds the slot"
    );
}

/// Catches: any report of a finished update ending an order: a host that reports a
/// successful update to a different version, while still at the old version and not yet
/// updating to the ordered one, has not reported the order, so it still holds its slot.
#[tokio::test]
async fn a_success_for_another_target_does_not_end_the_order() {
    let rig = started("svc-order-other-success", ONE_STORAGE).await;
    let c = &rig.coordinator;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    c.heartbeat(storage("s2"), last(beat("1.0"), "1.5", Outcome::Succeeded));
    assert_eq!(
        c.heartbeat(storage("s3"), beat("1.0")),
        Reply::Nothing,
        "s2's report is about 1.5; its order for 2.0 still holds the slot"
    );
}

/// Catches: a new desired version leaving the host's old order in place: the host is
/// ordered to the new version, the order is dated from then (not from the first), and it
/// holds the slot until it ages out from that time.
#[tokio::test]
async fn an_order_for_a_new_version_replaces_the_old_one() {
    let rig = started("svc-order-replaced", ONE_STORAGE).await;
    let c = &rig.coordinator;
    let age = ORDER_AGE.as_secs();
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("2.0"));
    rig.at(2_100 + age - 20);
    rig.set(&ONE_STORAGE.replace("2.0", "3.0")).await;
    c.heartbeat(storage("s1"), beat("1.0"));
    assert_eq!(c.heartbeat(storage("s2"), beat("1.0")), update("3.0"));
    rig.at(2_100 + age);
    assert_eq!(
        c.heartbeat(storage("s1"), beat("1.0")),
        Reply::Nothing,
        "the first order would have aged out by now; the one for 3.0 has not"
    );
}

const OTHER: &str = r#"{"service":"other","version":"3.0","min_up":{"storage":0,"frontend":0}}"#;

fn with_services(rig: &mut Rig, text: &str) {
    rig.coordinator =
        Rig::start(&rig.dir, &rig.clock).with_services(Services::parse(text).expect("services"));
}

const OTHER_SERVICE: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\n";

/// The testkit's single table with `services` appended to it.
fn single_table(services: &str) -> String {
    format!("{}{services}\n", config_text(STATIC_ISSUER))
}

/// Catches: a defined service that cannot be set, or one the file does not define that can:
/// with `other` defined and the entry naming it CI sets it (the status lists it beside
/// kbf's), the heartbeat of a kbf host is still answered from kbf alone, and a service
/// nobody defined is refused.
#[tokio::test]
async fn ci_sets_a_defined_service_and_kbf_is_unchanged() {
    let mut rig = Rig::new("svc-defined");
    rig.coordinator = Rig::start_with_allow_list(
        &rig.dir,
        &rig.clock,
        &single_table("services = [\"kbf\", \"other\"]"),
    )
    .with_services(Services::parse(OTHER_SERVICE).expect("services"));
    rig.set(OTHER).await;
    rig.set(STATE).await;
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.services["other"].version.as_str(), "3.0");
    assert_eq!(status.desired.expect("kbf").version.as_str(), "2.0");
    rig.at(2_000);
    let reply = rig
        .coordinator
        .heartbeat(who("d1", HostRole::DAEMON), beat("1.0"));
    assert_eq!(reply, update("2.0"));
    let unknown = OTHER.replace("other", "unknown");
    let set = rig
        .coordinator
        .set_desired(Some(&ci_token()), unknown.as_bytes())
        .await;
    assert!(matches!(set, Err(SetError::Invalid(_))));
}

/// Catches: a single-table allow-list that means more than kbf, or less: it sets kbf, and a
/// defined service the table does not name is a 403 that changes nothing.
#[tokio::test]
async fn a_single_table_sets_kbf_and_is_refused_for_another_service() {
    let mut rig = Rig::new("svc-single");
    with_services(&mut rig, OTHER_SERVICE);
    let set = rig
        .coordinator
        .set_desired(Some(&ci_token()), OTHER.as_bytes())
        .await;
    assert!(matches!(set, Err(SetError::Forbidden)));
    assert!(SetError::Forbidden.to_string().contains("may not"));
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.desired.is_none() && status.services.is_empty());
    rig.set(STATE).await;
}

const SERVICES_ONE_AT_A_TIME: &str =
    "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmax_updating = 1\nmin_up = 0\n";

/// A beat of a new updater: kbf at `kbf`, `other` at `other`.
fn new_beat(kbf: &str, other: &str) -> kbf_updater::wire::Heartbeat {
    let mut b = beat(kbf);
    b.services = Some(vec![service_beat(other)]);
    b
}

fn service_beat(running: &str) -> kbf_updater::wire::ServiceBeat {
    let b = beat(running);
    kbf_updater::wire::ServiceBeat {
        name: "other".to_owned(),
        version: b.version,
        health: b.health,
        last_update: None,
        updating: None,
        step: None,
        commit: None,
        step_secs: Default::default(),
        promotion_pending: false,
        deferred: None,
    }
}

fn update_of(service: &str, target: &str) -> kbf_updater::wire::ServiceReply {
    kbf_updater::wire::ServiceReply::Update {
        service: Some(service.to_owned()),
        version: version(target),
    }
}

async fn services_rig(name: &str) -> Rig {
    let mut rig = Rig::new(name);
    rig.coordinator = Rig::start_with_allow_list(
        &rig.dir,
        &rig.clock,
        &single_table("services = [\"kbf\", \"other\"]"),
    )
    .with_services(Services::parse(SERVICES_ONE_AT_A_TIME).expect("services"));
    rig.set(STATE).await;
    rig.set(OTHER).await;
    rig.at(2_000);
    rig
}

/// Behaviour: a host that sends `services` is ordered a service in the reply shape that
/// names it, and its service beat is recorded as that service's state of the host: while d1
/// reports `other` updating, d2 (one slot) is told nothing, and when d1 is done d2 is told.
#[tokio::test]
async fn a_new_updater_is_ordered_a_service_and_its_beat_is_recorded() {
    let rig = services_rig("svc-wire-order").await;
    let c = &rig.coordinator;
    assert_eq!(
        c.heartbeat(who("d1", HostRole::DAEMON), new_beat("2.0", "1.0")),
        update_of("other", "3.0")
    );
    let mut busy = new_beat("2.0", "1.0");
    busy.services.as_mut().expect("services")[0].updating = Some(version("3.0"));
    c.heartbeat(who("d1", HostRole::DAEMON), busy);
    assert_eq!(
        c.heartbeat(who("d2", HostRole::DAEMON), new_beat("2.0", "1.0")),
        kbf_updater::wire::ServiceReply::Nothing
    );
    c.heartbeat(who("d1", HostRole::DAEMON), new_beat("2.0", "3.0"));
    assert_eq!(
        c.heartbeat(who("d2", HostRole::DAEMON), new_beat("2.0", "1.0")),
        update_of("other", "3.0")
    );
}

/// Behaviour: one order per reply, kbf first. A host behind on both is told kbf in the old
/// shape; while that order stands it is told nothing for `other`; once it runs kbf's version
/// it is told `other`.
#[tokio::test]
async fn kbf_is_ordered_before_a_service_and_one_order_per_reply() {
    let rig = services_rig("svc-wire-first").await;
    let c = &rig.coordinator;
    let host = || who("d1", HostRole::DAEMON);
    assert_eq!(c.heartbeat(host(), new_beat("1.0", "1.0")), update("2.0"));
    assert_eq!(
        c.heartbeat(host(), new_beat("1.0", "1.0")),
        update("2.0"),
        "asked again before it reports, it is told the same"
    );
    assert_eq!(
        c.heartbeat(host(), new_beat("2.0", "1.0")),
        update_of("other", "3.0")
    );
}

/// Behaviour: a host that sent no `services` is never named a service, even
/// when other hosts are ordered `other` and it is behind on it; and entries for a service nobody
/// defined, or one that does not run on the host's role, are ignored (not recorded, not
/// ordered).
#[tokio::test]
async fn only_an_updater_that_announced_a_service_is_ordered_it() {
    let rig = services_rig("svc-wire-old").await;
    let c = &rig.coordinator;
    let host = || who("d1", HostRole::DAEMON);
    assert_eq!(
        c.heartbeat(host(), new_beat("2.0", "1.0")),
        update_of("other", "3.0")
    );
    assert_eq!(
        c.heartbeat(who("d3", HostRole::DAEMON), beat("2.0")),
        kbf_updater::wire::ServiceReply::Nothing,
        "an old updater is a kbf-only host"
    );
    assert_eq!(
        c.heartbeat(host(), beat("2.0")),
        kbf_updater::wire::ServiceReply::Nothing,
        "the latest beat says what the host is: it announces nothing now"
    );
    let mut stray = new_beat("2.0", "1.0");
    stray.services.as_mut().expect("services")[0].name = "unheard-of".to_owned();
    assert_eq!(
        c.heartbeat(who("d4", HostRole::DAEMON), stray),
        kbf_updater::wire::ServiceReply::Nothing
    );
    assert_eq!(
        c.heartbeat(who("s1", HostRole::STORAGE), new_beat("2.0", "1.0")),
        kbf_updater::wire::ServiceReply::Nothing,
        "other does not run on storage hosts"
    );
}

const SERVER_AGENT: &str = "[[service]]\nname = \"js-server\"\n[service.roles.daemon]\n\
    [[service]]\nname = \"js-agent\"\n[service.roles.daemon]\n";

fn body_for(service: &str) -> String {
    format!(r#"{{"service":"{service}","version":"3.0","min_up":{{"storage":0,"frontend":0}}}}"#)
}

fn entry_b_token() -> String {
    let mut claims = good_claims(STATIC_ISSUER);
    claims["repository_id"] = json!("777");
    claims["job_workflow_ref"] =
        json!("komira-ai/other/.github/workflows/release.yml@refs/heads/main");
    mint(&keys()[0], &claims)
}

/// Catches: services leaking between entries, or one entry's services ignored. Entry A
/// names `kbf` and entry B `js-server` and `js-agent`: A's token sets kbf and gets a 403 for
/// js-server; B's token sets js-server and js-agent and gets a 403 for kbf; a token no entry
/// admits is refused as before, and each refusal leaves the state as it was.
#[tokio::test]
async fn each_entry_sets_only_its_own_services() {
    let mut rig = Rig::new("svc-entries");
    let list = format!(
        "{}services = [\"kbf\"]\n\n[[remote_operator]]\nissuer = \"{STATIC_ISSUER}\"\n\
         audience = \"kbf-gamma\"\nrepository_id = \"777\"\n\
         job_workflow_ref = [\"komira-ai/other/.github/workflows/release.yml@refs/heads/main\"]\n\
         refs = [\"refs/heads/main\"]\nservices = [\"js-server\", \"js-agent\"]\n",
        config_text(STATIC_ISSUER).replace("[remote_operator]", "[[remote_operator]]"),
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(SERVER_AGENT).expect("services"));
    let c = &rig.coordinator;

    let a = ci_token();
    assert!(matches!(
        c.set_desired(Some(&a), body_for("js-server").as_bytes())
            .await,
        Err(SetError::Forbidden)
    ));
    let a = ci_token();
    c.set_desired(Some(&a), STATE.as_bytes())
        .await
        .expect("A sets kbf");

    let b = entry_b_token();
    assert!(matches!(
        c.set_desired(Some(&b), STATE.as_bytes()).await,
        Err(SetError::Forbidden)
    ));
    for service in ["js-server", "js-agent"] {
        let b = entry_b_token();
        c.set_desired(Some(&b), body_for(service).as_bytes())
            .await
            .expect("B sets its service");
    }

    let mut stranger = good_claims(STATIC_ISSUER);
    stranger["repository_id"] = json!("999");
    let stranger = mint(&keys()[0], &stranger);
    let refused = c.set_desired(Some(&stranger), STATE.as_bytes()).await;
    assert!(
        matches!(refused, Err(SetError::Unauthorized(_))),
        "{refused:?}"
    );

    let status = c.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.desired.expect("kbf").version.as_str(), "2.0");
    assert_eq!(status.services["js-server"].version.as_str(), "3.0");
    assert_eq!(status.services["js-agent"].version.as_str(), "3.0");
    assert_eq!(status.services.len(), 3);
}

const OTHER_PAIR: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 2\n";

/// Behaviour: the convergence and the status of a second service come from the heartbeats
/// that carry that service and from its own desired state. While `other` has one of its two
/// required daemons at 3.0, kbf (all at 2.0) reads converged and `other` in progress; when
/// the second daemon reports 3.0 `other` converges, and the status counts each service's
/// hosts on their own. A name the coordinator does not manage is an error, and so is a
/// read with a bad token.
#[tokio::test]
async fn a_second_services_convergence_and_status_come_from_its_own_hosts() {
    let mut rig = Rig::new("svc-converge-other");
    rig.coordinator = Rig::start_with_allow_list(
        &rig.dir,
        &rig.clock,
        &single_table("services = [\"kbf\", \"other\"]"),
    )
    .with_services(Services::parse(OTHER_PAIR).expect("services"));
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    rig.set(OTHER).await;
    rig.at(2_000);
    let c = &rig.coordinator;
    let read = |service: &'static str| async move {
        c.convergence(Some(&ci_token()), service)
            .await
            .expect("read")
    };
    c.heartbeat(who("d1", HostRole::DAEMON), new_beat("2.0", "3.0"));
    c.heartbeat(who("d2", HostRole::DAEMON), new_beat("2.0", "1.0"));
    let kbf = read("kbf").await;
    assert_eq!(kbf.state, State::Converged);
    assert_eq!(kbf.roles[0].at_desired, 2);
    let other = read("other").await;
    assert_eq!(other.state, State::InProgress);
    assert_eq!(other.desired.expect("desired").as_str(), "3.0");
    assert_eq!(other.roles[0].at_desired, 1);
    c.heartbeat(who("d2", HostRole::DAEMON), new_beat("2.0", "3.0"));
    assert_eq!(read("other").await.state, State::Converged);

    let status = c.status(Some(OPERATOR)).expect("status");
    assert_eq!(
        status
            .per_service
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["kbf", "other"]
    );
    assert_eq!(status.per_service["kbf"].roles[0].at_desired, 2);
    assert_eq!(status.per_service["other"].roles[0].at_desired, 2);
    assert_eq!(
        status.per_service["other"]
            .desired
            .as_ref()
            .expect("other")
            .version
            .as_str(),
        "3.0"
    );

    let unknown = c.convergence(Some(&ci_token()), "nope").await;
    assert_eq!(unknown.map(|_| ()), Err(ConvergenceError::UnknownService));
    let refused = c.convergence(None, "other").await;
    assert_eq!(
        refused.map(|_| ()),
        Err(ConvergenceError::Refused(CiRefused::Unauthorized(
            "missing"
        )))
    );
    assert_eq!(
        ConvergenceError::UnknownService.to_string(),
        "no such service"
    );
}

/// Catches: a service matched by prefix or without regard to case. The entry names `kbf`
/// and `js`, and the services file defines `js` and `js-server`: `js` is set, `js-server`
/// (which starts with `js`) is a 403, and `JS` is no defined service, so a 400. An entry
/// that names `JS` gets a 403 for `js`. Each refusal leaves the state as it was.
#[tokio::test]
async fn a_service_is_allowed_by_its_exact_name_only() {
    let start = |rig: &Rig, named: &str| {
        let list = config_text(STATIC_ISSUER) + &format!("services = [\"kbf\", \"{named}\"]\n");
        Rig::start_with_allow_list(&rig.dir, &rig.clock, &list).with_services(
            Services::parse(
                "[[service]]\nname = \"js\"\n[service.roles.daemon]\n\
                 [[service]]\nname = \"js-server\"\n[service.roles.daemon]\n",
            )
            .expect("services"),
        )
    };
    let mut rig = Rig::new("svc-exact");
    rig.coordinator = start(&rig, "js");
    let c = &rig.coordinator;
    c.set_desired(Some(&ci_token()), body_for("js").as_bytes())
        .await
        .expect("js is named");
    assert!(matches!(
        c.set_desired(Some(&ci_token()), body_for("js-server").as_bytes())
            .await,
        Err(SetError::Forbidden)
    ));
    assert!(matches!(
        c.set_desired(Some(&ci_token()), body_for("JS").as_bytes())
            .await,
        Err(SetError::Invalid(_))
    ));
    let status = c.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.services.keys().collect::<Vec<_>>(), ["js"]);

    let mut rig = Rig::new("svc-exact-case");
    rig.coordinator = start(&rig, "JS");
    let c = &rig.coordinator;
    assert!(matches!(
        c.set_desired(Some(&ci_token()), body_for("js").as_bytes())
            .await,
        Err(SetError::Forbidden)
    ));
    assert!(
        c.status(Some(OPERATOR))
            .expect("status")
            .services
            .is_empty()
    );
}
