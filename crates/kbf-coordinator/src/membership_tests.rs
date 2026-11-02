//! Which hosts a service counts: selectors, exclusions, absent and missing hosts, and what
//! each does to the convergence and to the orders.

use kbf_types::HostRole;
use kbf_updater::wire::{Heartbeat, Reply, ServiceBeat, ServiceReply};
use serde_json::{Value, json};

use crate::identity::Identity;
use crate::service::FEATURES;
use crate::services::{Services, ServicesError};
use crate::testutil::{OPERATOR, Rig, beat, id};

pub(crate) const V2: &str = "heartbeat-v2";

pub(crate) const DAEMONS: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n\
    [service.selector]\nroles = [\"daemon\"]\n";

const DAEMONS_OF_TWO_ROLES: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n\
    [service.roles.storage]\nmin_up = 0\n[service.selector]\nroles = [\"daemon\"]\n";

const NO_SELECTOR: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

pub(crate) const ZONE_A: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n\
    [service.selector]\nlabels = { zone = \"a\" }\n";

const TWO: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n\
    [service.selector]\nroles = [\"daemon\"]\n\
    [[service]]\nname = \"second\"\ninstall_when_absent = true\n\
    [service.roles.daemon]\nmin_up = 0\n[service.selector]\nroles = [\"daemon\"]\n";

const KBF_ZONE_B: &str = "[[service]]\nname = \"kbf\"\n[service.roles.daemon]\n\
    [service.selector]\nlabels = { zone = \"b\" }\n\
    [[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

/// A rig past the quiet start over the services of `text`, each but kbf desired at 3.0.
pub(crate) async fn rig(name: &str, text: &str, services: &[&str]) -> Rig {
    let mut rig = Rig::new(name);
    let names: Vec<String> = ["kbf"]
        .iter()
        .chain(services)
        .map(|s| format!("\"{s}\""))
        .collect();
    let list = format!(
        "{}services = [{}]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER),
        names.join(", ")
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(text).expect("services"));
    for service in services {
        rig.set(&format!(
            r#"{{"service":"{service}","version":"3.0","min_up":{{"storage":0,"frontend":0}}}}"#
        ))
        .await;
    }
    rig.at(2_000);
    rig
}

pub(crate) fn who(name: &str) -> Identity {
    Identity {
        node_id: id(name),
        role: HostRole::DAEMON,
    }
}

fn storage(name: &str) -> Identity {
    Identity {
        node_id: id(name),
        role: HostRole::STORAGE,
    }
}

/// The heartbeat of `name`, a v2 agent or not, listing `services` as (name, version, health).
fn beat_of(v2: bool, services: &[(&str, &str, &str)]) -> Heartbeat {
    let mut b = beat("1.0");
    if v2 {
        b.agent_features = vec![V2.to_owned()];
    }
    let list: Vec<Value> = services
        .iter()
        .map(|(n, v, h)| json!({"name": n, "version": v, "health": h}))
        .collect();
    b.services = Some(serde_json::from_value::<Vec<ServiceBeat>>(json!(list)).expect("beats"));
    b
}

fn send(rig: &Rig, name: &str, v2: bool, services: &[(&str, &str, &str)]) -> ServiceReply {
    rig.coordinator
        .heartbeat(who(name), beat_of(v2, services))
        .reply
}

pub(crate) fn operate(rig: &Rig, body: &str) {
    rig.coordinator
        .operate(Some(OPERATOR), body.as_bytes())
        .expect("an operator action");
}

pub(crate) fn labels(rig: &Rig, node: &str, set: &str) {
    operate(
        rig,
        &format!(r#"{{"action":"set_labels","node_id":"{node}","labels":{set}}}"#),
    );
}

fn convergence(rig: &Rig, service: &str) -> Value {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    serde_json::to_value(&status.per_service[service].convergence).expect("json")
}

fn service_status(rig: &Rig, service: &str) -> Value {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    serde_json::to_value(&status.per_service[service]).expect("json")
}

fn counts(missing: u64, absent: u64, excluded: u64, unmanaged: u64) -> Value {
    json!({"missing": missing, "absent": absent, "excluded": excluded,
        "unmanaged": unmanaged, "deferred": 0})
}

const OTHER: (&str, &str, &str) = ("other", "3.0", "healthy");

/// Catches: the denominator hole: a host selected for the service that never announced it
/// leaving the farm reading converged. The host is missing and the service is in progress
/// until it announces; when it stops announcing it is missing again and no longer counted at
/// the desired version.
#[tokio::test]
async fn a_selected_live_v2_host_that_never_announced_the_service_is_missing_and_blocks_converged()
{
    let rig = rig("mem-missing", DAEMONS, &["other"]).await;
    send(&rig, "d1", true, &[OTHER]);
    send(&rig, "d2", true, &[]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(1, 0, 0, 0));
    assert_eq!(c["state"], "in_progress");
    send(&rig, "d2", true, &[OTHER]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 0, 0));
    assert_eq!(c["state"], "converged");
    send(&rig, "d2", true, &[]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(1, 0, 0, 0));
    assert_eq!(c["state"], "in_progress");
    assert_eq!(c["roles"][0]["at_desired"], 1);
}

/// Catches: the deadlock with old agents: a selected host that cannot announce is counted
/// missing for ever. It is unmanaged, not counted, and not ordered, whether or not it listed
/// the service; a v2 host that is behind is ordered.
#[tokio::test]
async fn a_host_without_heartbeat_v2_is_unmanaged_not_missing() {
    let rig = rig("mem-unmanaged", DAEMONS, &["other"]).await;
    send(&rig, "d1", true, &[OTHER]);
    send(&rig, "d2", false, &[]);
    let old = send(&rig, "d3", false, &[("other", "1.0", "healthy")]);
    assert_eq!(old, ServiceReply::Nothing);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 0, 2));
    assert_eq!(c["state"], "converged");
    assert_eq!(c["roles"][0]["at_desired"], 1);
    let v2 = send(&rig, "d4", true, &[("other", "1.0", "healthy")]);
    assert_eq!(
        v2,
        ServiceReply::Update {
            service: Some("other".to_owned()),
            version: crate::testutil::version("3.0")
        }
    );
}

/// Catches: an excluded host counted or ordered, and the status silent about why: it is not
/// counted, not ordered, listed with the operator's reason, and counted again once included.
#[tokio::test]
async fn an_excluded_host_is_not_counted_and_status_gives_its_reason() {
    let rig = rig("mem-excluded", DAEMONS, &["other"]).await;
    operate(
        &rig,
        r#"{"action":"exclude","service":"other","node_id":"d2","reason":"disk replaced"}"#,
    );
    send(&rig, "d1", true, &[OTHER]);
    let behind = send(&rig, "d2", true, &[("other", "1.0", "healthy")]);
    assert_eq!(behind, ServiceReply::Nothing);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 1, 0));
    assert_eq!(c["state"], "converged");
    let status = service_status(&rig, "other");
    assert_eq!(status["hosts"].as_array().expect("hosts").len(), 1);
    assert_eq!(status["hosts"][0]["node_id"], "d1");
    assert_eq!(
        status["members"],
        json!([{"node_id": "d2", "role": "daemon", "membership": "excluded",
            "reason": "disk replaced"}])
    );
    operate(
        &rig,
        r#"{"action":"include","service":"other","node_id":"d2"}"#,
    );
    assert!(matches!(
        send(&rig, "d2", true, &[("other", "1.0", "healthy")]),
        ServiceReply::Update { .. }
    ));
    assert_eq!(convergence(&rig, "other")["hosts"], counts(0, 0, 0, 0));
    assert!(service_status(&rig, "other").get("members").is_none());
}

/// Catches: an exclusion that needs a selector, or that leaves the excluded host's earlier
/// heartbeat in the counts: the host is dropped at once, and the counts appear only when
/// there is something to count.
#[tokio::test]
async fn an_exclusion_applies_to_a_service_without_a_selector() {
    let rig = rig("mem-excluded-plain", NO_SELECTOR, &["other"]).await;
    send(&rig, "d1", false, &[OTHER]);
    send(&rig, "d2", false, &[("other", "1.0", "healthy")]);
    let c = convergence(&rig, "other");
    assert_eq!(c["state"], "in_progress");
    assert!(c.get("hosts").is_none());
    operate(
        &rig,
        r#"{"action":"exclude","service":"other","node_id":"d2","reason":"moved"}"#,
    );
    let c = convergence(&rig, "other");
    assert_eq!(c["state"], "converged");
    assert_eq!(c["hosts"], counts(0, 0, 1, 0));
}

/// Catches: labels read from the wrong place or cached: a host is selected by the labels the
/// operator gives it, a change moves it in at once, and moving it out drops what it said.
#[tokio::test]
async fn labels_select_and_changing_them_moves_a_host_out() {
    let rig = rig("mem-labels", ZONE_A, &["other"]).await;
    labels(&rig, "d1", r#"{"zone":"a"}"#);
    send(&rig, "d1", true, &[OTHER]);
    send(&rig, "d2", true, &[]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 0, 0));
    assert_eq!(c["state"], "converged");
    labels(&rig, "d2", r#"{"zone":"a"}"#);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(1, 0, 0, 0));
    assert_eq!(c["state"], "in_progress");
    labels(&rig, "d2", r#"{"zone":"b"}"#);
    assert_eq!(convergence(&rig, "other")["state"], "converged");
    labels(&rig, "d1", "{}");
    let c = convergence(&rig, "other");
    assert_eq!(c["roles"], json!([]));
    assert_eq!(c["state"], "in_progress");
}

/// Catches: a service no selected host announces reading converged: hosts that are not
/// selected do not count, so there is nothing at the desired version.
#[tokio::test]
async fn a_service_no_selected_host_announces_is_in_progress() {
    let rig = rig("mem-none", ZONE_A, &["other"]).await;
    send(&rig, "d1", true, &[OTHER]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 0, 0));
    assert_eq!(c["roles"], json!([]));
    assert_eq!(c["state"], "in_progress");
}

/// Catches: liveness checked after membership: a silent selected host that never announced
/// is down, not missing, and one that did is down in the role counts; an operator action
/// does not forget a down host.
#[tokio::test]
async fn a_down_selected_host_is_down_not_missing() {
    let rig = rig("mem-down", DAEMONS, &["other"]).await;
    send(&rig, "d1", true, &[OTHER]);
    send(&rig, "d2", true, &[OTHER]);
    send(&rig, "d3", true, &[]);
    rig.at(2_100);
    send(&rig, "d1", true, &[OTHER]);
    let check = || convergence(&rig, "other");
    assert_eq!(check()["hosts"], counts(0, 0, 0, 0));
    assert_eq!(check()["roles"][0]["down"], 1);
    assert_eq!(check()["state"], "converged");
    labels(&rig, "d9", r#"{"x":"y"}"#);
    assert_eq!(check()["roles"][0]["down"], 1);
}

/// Catches: an absent host counted up or ordered: it holds the service in progress even at the
/// desired version, is not ordered and says why, and is ordered when the definition says
/// `install_when_absent`.
#[tokio::test]
async fn an_absent_host_is_not_up_and_is_ordered_only_with_install_when_absent() {
    let rig = rig("mem-absent", TWO, &["other", "second"]).await;
    send(&rig, "d1", true, &[("other", "3.0", "absent")]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 1, 0, 0));
    assert_eq!(c["state"], "in_progress");
    let both = [("other", "1.0", "absent"), ("second", "1.0", "absent")];
    assert_eq!(
        send(&rig, "d1", true, &both),
        ServiceReply::Update {
            service: Some("second".to_owned()),
            version: crate::testutil::version("3.0")
        }
    );
    assert_eq!(service_status(&rig, "other")["hosts"][0]["hold"], "absent");
    send(&rig, "d1", true, &[OTHER]);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 0, 0));
    assert_eq!(c["state"], "converged");
}

/// Catches: a deferral not shown in the counts, or shown after its wait: a host that declined
/// the desired version is counted `deferred` while its backoff lasts and not after, and is not
/// ordered meanwhile.
#[tokio::test]
async fn a_host_that_declined_the_version_is_counted_deferred_until_its_wait_ends() {
    let rig = rig("mem-deferred", DAEMONS, &["other"]).await;
    let declined = json!({"name": "other", "version": "1.0", "health": "healthy",
        "deferred": {"target": "3.0", "reason": "drain_bound", "retry_after_secs": 300}});
    let mut b = beat_of(true, &[]);
    b.services = Some(serde_json::from_value(json!([declined])).expect("beats"));
    let reply = rig.coordinator.heartbeat(who("d1"), b).reply;
    assert_eq!(reply, ServiceReply::Nothing);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"]["deferred"], 1);
    rig.at(2_400);
    assert_eq!(convergence(&rig, "other")["hosts"]["deferred"], 0);
}

/// Catches: a selector that names roles selecting every role of the service: a host of a role
/// the selector leaves out is not counted, not missing and not ordered, even when the service
/// runs on its role.
#[tokio::test]
async fn a_selector_leaves_out_the_roles_it_does_not_name() {
    let rig = rig("mem-roles", DAEMONS_OF_TWO_ROLES, &["other"]).await;
    send(&rig, "d1", true, &[OTHER]);
    let behind = beat_of(true, &[("other", "1.0", "healthy")]);
    let reply = rig.coordinator.heartbeat(storage("s1"), behind).reply;
    assert_eq!(reply, ServiceReply::Nothing);
    let c = convergence(&rig, "other");
    assert_eq!(c["hosts"], counts(0, 0, 0, 0));
    assert_eq!(c["state"], "converged");
}

/// Catches: a definition without a selector changing what it counts or says: no `hosts` and
/// no `members` in its answers, a host that announces is counted whatever its agent, and it
/// is ordered as before.
#[tokio::test]
async fn a_definition_without_a_selector_counts_exactly_as_before() {
    let rig = rig("mem-plain", NO_SELECTOR, &["other"]).await;
    let order = send(&rig, "d1", false, &[("other", "1.0", "healthy")]);
    assert_eq!(
        order,
        ServiceReply::Update {
            service: Some("other".to_owned()),
            version: crate::testutil::version("3.0")
        }
    );
    send(&rig, "d2", false, &[]);
    let c = convergence(&rig, "other");
    assert!(c.get("hosts").is_none());
    assert!(service_status(&rig, "other").get("members").is_none());
    assert_eq!(
        service_status(&rig, "other")["hosts"]
            .as_array()
            .expect("hosts")
            .len(),
        1
    );
}

/// Catches: a selector that names a role the service lacks, a bad label, or a key this build
/// does not read being accepted: each is refused when the file is read.
#[test]
fn a_selector_naming_a_role_the_definition_lacks_is_refused() {
    let head = "[[service]]\nname = \"other\"\n[service.roles.daemon]\n[service.selector]\n";
    let why = |body: &str| match Services::parse(&format!("{head}{body}")) {
        Err(ServicesError::Invalid(why)) => why,
        other => panic!("not refused as invalid: {other:?}"),
    };
    let lacking = why("roles = [\"storage\"]\n");
    assert!(lacking.contains("selector"), "{lacking}");
    assert!(lacking.contains("storage"), "{lacking}");
    assert!(why("roles = [\"nonsense\"]\n").contains("nonsense"));
    assert!(why("labels = { Zone = \"a\" }\n").contains("label"));
    let long = "a".repeat(33);
    assert!(why(&format!("labels = {{ zone = \"{long}\" }}\n")).contains("label"));
    let nine: Vec<String> = (0..9).map(|i| format!("k{i} = \"v\"")).collect();
    assert!(why(&format!("labels = {{ {} }}\n", nine.join(", "))).contains("label"));
    assert!(matches!(
        Services::parse(&format!("{head}hosts = []\n")),
        Err(ServicesError::Parse(_))
    ));
    Services::parse(&format!(
        "{head}roles = [\"daemon\"]\nlabels = {{ zone = \"a\" }}\n"
    ))
    .expect("a good selector");
}

/// Catches: a host with no `kbf` beat in the service it is selected for, or one that kbf's
/// selector leaves out, being left out of every service: it is a node, counted for the other
/// service, absent from kbf's hosts, and in them once its labels select it.
#[tokio::test]
async fn a_kbf_less_v2_host_is_a_node_and_is_selected_for_other_services() {
    let rig = rig("mem-kbf-less", KBF_ZONE_B, &["other"]).await;
    send(&rig, "d1", true, &[OTHER]);
    let c = convergence(&rig, "other");
    assert_eq!(c["state"], "converged");
    assert_eq!(c["roles"][0]["at_desired"], 1);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.hosts.is_empty());
    assert!(status.per_service["kbf"].hosts.is_empty());
    labels(&rig, "d1", r#"{"zone":"b"}"#);
    send(&rig, "d1", true, &[OTHER]);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.hosts.len(), 1);
    assert_eq!(status.per_service["kbf"].hosts.len(), 1);
}

/// Catches: `heartbeat-v2` advertised without the coordinator acting on it, or not
/// advertised to the agent that announces it: the name is in the answer to an agent that asks
/// for it, and a v2 host that did not announce is missing where an agent without it is
/// unmanaged.
pub(crate) async fn heartbeat_v2_is_acted_on() {
    let rig = rig("mem-v2-proof", DAEMONS, &["other"]).await;
    let answer = rig
        .coordinator
        .heartbeat(who("d1"), beat_of(true, &[OTHER]));
    assert_eq!(answer.features, Some(vec![V2.to_owned()]));
    assert_eq!(answer, Reply::Nothing);
    send(&rig, "d2", true, &[]);
    send(&rig, "d3", false, &[]);
    assert_eq!(convergence(&rig, "other")["hosts"], counts(1, 0, 0, 1));
}

#[tokio::test]
async fn heartbeat_v2_is_advertised_and_acted_on() {
    assert!(FEATURES.contains(&V2));
    heartbeat_v2_is_acted_on().await;
}
