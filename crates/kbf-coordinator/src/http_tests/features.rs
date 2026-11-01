//! The features of a reply over HTTP: only to an agent that announced `agent_features`, and
//! never to one whose strict reader would refuse them.

use super::*;

use crate::service::FEATURES;

const SERVICES_TEXT: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

/// The reply structs of the updaters before `features`, frozen with their strict reads.
mod frozen {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct Reply {
        pub reply: String,
        pub version: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct ServiceReply {
        pub reply: String,
        pub service: Option<String>,
        pub version: Option<String>,
    }
}

/// A heartbeat of a host running kbf `running`, with `extra` after its fields.
fn beat(running: &str, extra: &str) -> String {
    format!(r#"{{"platform":"linux-x86_64","version":"{running}","health":"healthy"{extra}}}"#)
}

const OTHER: &str = r#","services":[{"name":"other","version":"1.0","health":"healthy"}]"#;

/// A rig past the quiet start that advertises `advertised`, with kbf and `other` both
/// desired at 2.0 / 3.0.
async fn rig(name: &str, advertised: &[&str]) -> Rig {
    let mut rig = Rig::new(name);
    let list = format!(
        "{}services = [\"kbf\", \"other\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(crate::services::Services::parse(SERVICES_TEXT).expect("services"))
        .with_features(advertised);
    for body in [
        r#"{"version":"2.0","min_up":{"storage":1,"frontend":1}}"#,
        r#"{"service":"other","version":"3.0","min_up":{"storage":0,"frontend":0}}"#,
    ] {
        let (code, _) = raw(
            &rig,
            Surface::Control,
            &None,
            request("POST", DESIRED_PATH, Some(&ci_token()), body),
        )
        .await;
        assert_eq!(code, 200);
    }
    rig.at(2_000);
    rig
}

async fn beat_text(rig: &Rig, host: &str, role: &str, body: &str) -> String {
    let (code, bytes) = raw(
        rig,
        Surface::Heartbeat,
        &updater(host, role),
        request("POST", HEARTBEAT_PATH, None, body),
    )
    .await;
    assert_eq!(code, 200);
    String::from_utf8(bytes).expect("text")
}

/// Catches: `features` sent to an agent that never asked, even by a coordinator that
/// advertises some: the bytes to an agent without `agent_features` are the reply alone, for
/// nothing, a kbf order and a service order, and each frozen strict reader of the old agents
/// parses them.
#[tokio::test]
async fn an_agent_that_announced_nothing_gets_the_reply_alone() {
    let rig = rig("http-features-none", &["deferred-v1"]).await;
    let nothing = beat_text(&rig, "d1", "daemon", &beat("2.0", "")).await;
    assert_eq!(nothing, r#"{"reply":"nothing"}"#);
    let kbf_order = beat_text(&rig, "d2", "daemon", &beat("1.0", "")).await;
    assert_eq!(kbf_order, r#"{"reply":"update","version":"2.0"}"#);
    let service_order = beat_text(&rig, "d3", "daemon", &beat("2.0", OTHER)).await;
    assert_eq!(
        service_order,
        r#"{"reply":"update","service":"other","version":"3.0"}"#
    );
    for text in [&nothing, &kbf_order] {
        serde_json::from_str::<frozen::Reply>(text).expect("the v0.0.18 reader");
    }
    for text in [&nothing, &kbf_order, &service_order] {
        serde_json::from_str::<frozen::ServiceReply>(text).expect("the services reader");
    }
}

/// Catches: an announcement ignored, or the coordinator echoing what it does not support:
/// an agent that announced is answered with `features`, the ones it announced that the
/// coordinator advertises, in the coordinator's order, and no other.
#[tokio::test]
async fn an_agent_that_announced_features_gets_the_supported_subset() {
    let rig = rig("http-features-subset", &["deferred-v1", "halt-v1"]).await;
    let both = beat(
        "2.0",
        r#","agent_features":["halt-v1","unknown-v9","deferred-v1"]"#,
    );
    assert_eq!(
        beat_text(&rig, "d1", "daemon", &both).await,
        r#"{"reply":"nothing","features":["deferred-v1","halt-v1"]}"#
    );
    let unknown = beat("2.0", r#","agent_features":["unknown-v9"]"#);
    assert_eq!(
        beat_text(&rig, "d1", "daemon", &unknown).await,
        r#"{"reply":"nothing","features":[]}"#
    );
    let order = beat("1.0", r#","agent_features":["halt-v1"]"#);
    assert_eq!(
        beat_text(&rig, "d2", "daemon", &order).await,
        r#"{"reply":"update","version":"2.0","features":["halt-v1"]}"#
    );
}

/// Catches: a coordinator that advertises nothing staying silent to a new agent: it answers
/// `features: []`, which tells the agent this coordinator knows the field and supports none.
#[tokio::test]
async fn a_coordinator_that_advertises_nothing_answers_an_announcing_agent_with_an_empty_list() {
    let rig = rig("http-features-empty", &[]).await;
    let announcing = beat("2.0", r#","agent_features":["deferred-v1","halt-v1"]"#);
    assert_eq!(
        beat_text(&rig, "d1", "daemon", &announcing).await,
        r#"{"reply":"nothing","features":[]}"#
    );
}

/// Catches: a bad announcement costing the host its updates or being answered as a good one:
/// the heartbeat is still read, the host is still ordered its kbf update, and the answer
/// carries no `features` (a bad list is no announcement).
#[tokio::test]
async fn a_bad_announcement_is_dropped_and_the_host_is_answered_as_an_old_agent() {
    let rig = rig("http-features-bad", &["halt-v1"]).await;
    let bad = [
        r#","agent_features":["Halt-V1"]"#,
        r#","agent_features":["halt-v1","halt-v1"]"#,
        r#","agent_features":["a","b","c","d","e","f","g","h","i"]"#,
        r#","agent_features":[""]"#,
    ];
    assert_eq!(
        beat_text(&rig, "d1", "daemon", &beat("1.0", bad[0])).await,
        r#"{"reply":"update","version":"2.0"}"#
    );
    for list in bad {
        assert_eq!(
            beat_text(&rig, "d2", "daemon", &beat("2.0", list)).await,
            r#"{"reply":"nothing"}"#,
            "{list}"
        );
    }
}

/// Catches: a new agent's heartbeat with every new fact refused, or its facts changing what
/// it is ordered: the heartbeat is accepted and answered as the same heartbeat without them.
#[tokio::test]
async fn a_heartbeat_with_every_new_fact_is_accepted_and_ordered_as_without_them() {
    let rig = rig("http-features-facts", &[]).await;
    let facts = concat!(
        r#","step":{"name":"verify","attempt":2},"commit":"committed","#,
        r#""step_secs":{"cutover":41},"promotion_pending":false,"#,
        r#""deferred":{"target":"1.2","reason":"drain_bound","retry_after_secs":300}"#
    );
    assert_eq!(
        beat_text(&rig, "d1", "daemon", &beat("1.0", facts)).await,
        r#"{"reply":"update","version":"2.0"}"#
    );
}

/// Catches: the advertised set going vacuous: every name the coordinator advertises must be
/// handled, and a name is added in the change that handles it. For each advertised name that
/// a heartbeat can use, the heartbeat that uses it is answered differently from the same
/// heartbeat without it; `heartbeat-v2` is not seen in a reply, so its proof is the
/// membership one. The table of what uses a name has no entry for a name that is not
/// advertised.
#[tokio::test]
async fn advertised_features_equal_the_handled_set() {
    let uses = std::collections::BTreeMap::from([
        (
            "deferred-v1",
            r#","deferred":{"target":"2.0","reason":"drain_bound","retry_after_secs":300}"#,
        ),
        // A halt report: the host that makes it is ordered nothing, where the host without
        // it is ordered the update. It is the last name, since the halt stands afterwards.
        (
            "halt-v1",
            r#","last_update":{"target":"2.0","outcome":"failed","phase":"halt","reason":"tainted"}"#,
        ),
    ]);
    let announced = FEATURES.join(r#"",""#);
    let announce = format!(r#","agent_features":["{announced}"]"#);
    let rig = rig("http-features-handled", FEATURES).await;
    let mut proven = Vec::new();
    for name in FEATURES {
        if let Some(extra) = uses.get(name) {
            let without = beat_text(&rig, "d1", "daemon", &beat("1.0", &announce)).await;
            let with_it = beat("1.0", &format!("{announce}{extra}"));
            let with = beat_text(&rig, "d2", "daemon", &with_it).await;
            assert_ne!(with, without, "{name} changes nothing");
        } else {
            assert_eq!(*name, "heartbeat-v2");
            crate::membership_tests::heartbeat_v2_is_acted_on().await;
        }
        proven.push(*name);
    }
    let mut handled: Vec<&str> = uses.keys().copied().collect();
    handled.push("heartbeat-v2");
    handled.sort_unstable();
    proven.sort_unstable();
    assert_eq!(proven, handled);
}
