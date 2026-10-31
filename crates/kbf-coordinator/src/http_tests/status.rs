//! Over HTTP: the convergence without a query is still kbf's, and the status lists the hosts
//! of every service.

use super::*;

const SERVICES_TEXT: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

async fn beat_from(rig: &Rig, name: &str, role: &str, body: &str) {
    let (code, _) = call(
        rig,
        Surface::Heartbeat,
        &updater(name, role),
        request("POST", HEARTBEAT_PATH, None, body),
    )
    .await;
    assert_eq!(code, 200);
}

/// kbf wants 2.0 and `other` wants 3.0; the daemon `d1` runs kbf at 2.0 and `other` at 1.0.
async fn farm(name: &str) -> Rig {
    let mut rig = Rig::new(name);
    let list = format!(
        "{}services = [\"kbf\", \"other\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(crate::services::Services::parse(SERVICES_TEXT).expect("services"));
    for body in [
        r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#,
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
    beat_from(
        &rig,
        "d1",
        "daemon",
        r#"{"platform":"linux-x86_64","version":"2.0","health":"healthy","services":[{"name":"other","version":"1.0","health":"healthy"}]}"#,
    )
    .await;
    rig
}

async fn read(rig: &Rig, path: &str) -> Value {
    let (code, body) = call(
        rig,
        Surface::Control,
        &None,
        request("GET", path, Some(&ci_token()), ""),
    )
    .await;
    assert_eq!(code, 200, "{path}");
    body
}

/// Pins: with another service defined and wanting another version, `GET /v1/convergence`
/// with no query is kbf's answer, byte for byte what `?service=kbf` says, and not
/// `other`'s. Catches the default moving to the first or last service by name.
#[tokio::test]
async fn the_convergence_default_service_is_still_kbf() {
    let rig = farm("http-status-default").await;
    let bare = read(&rig, CONVERGENCE_PATH).await;
    assert_eq!(bare, read(&rig, "/v1/convergence?service=kbf").await);
    assert_eq!(bare["desired"], "2.0");
    assert_eq!(bare["state"], "converged");
    let other = read(&rig, "/v1/convergence?service=other").await;
    assert_eq!(other["desired"], "3.0");
    assert_eq!(other["state"], "in_progress");
}

/// Behaviour: `GET /v1/status` lists, under each service, the hosts that heartbeated it. The
/// daemon was ordered to update `other`, so under `other` it holds a slot, is not up and has
/// no hold reason; under kbf it is busy elsewhere (a host does one update at a time). The
/// top-level `hosts` list, kbf's, has no `hold` key. Catches the hosts of a service missing from the HTTP answer.
#[tokio::test]
async fn the_status_over_http_lists_the_hosts_of_every_service() {
    let rig = farm("http-status-hosts").await;
    let status = status(&rig).await;
    let under = |service: &str| status["per_service"][service]["hosts"][0].clone();
    let other = under("other");
    assert_eq!(other["node_id"], "d1");
    assert_eq!(
        (&other["up"], &other["holds_slot"], &other["hold"]),
        (&json!(false), &json!(true), &Value::Null)
    );
    let kbf = under("kbf");
    assert_eq!(
        (&kbf["up"], &kbf["holds_slot"], &kbf["hold"]),
        (&json!(true), &json!(false), &json!("busy_elsewhere"))
    );
    assert_eq!(status["hosts"][0].get("hold"), None);
    assert_eq!(status["per_service"]["other"]["up"], 0);
    assert_eq!(status["per_service"]["other"]["slots_held"], 1);
}
