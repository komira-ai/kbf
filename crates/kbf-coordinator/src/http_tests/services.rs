//! The HTTP surface of services besides `kbf`: a service named in the reply only to an
//! updater that sent services, kbf's status and convergence bytes kept, and the convergence
//! read per service with a query.

use super::*;

const SERVICES_TEXT: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

const NEW_GOLDEN_BEAT: &str = concat!(
    r#"{"platform":"linux-x86_64","version":"0.0.19+g123456abcdef-1","health":"healthy","#,
    r#""last_update":null,"updating":null,"services":[{"name":"other","version":"1.0","#,
    r#""health":"healthy","last_update":null,"updating":null}]}"#
);

/// Pins the wire of a service over HTTP. With `other` set to 3.0 and kbf current, an updater
/// that sends `services` is answered with the exact bytes of a named order, while a v0.0.18
/// updater of the same role (its golden beat, no `services`) is answered `{"reply":"nothing"}`
/// and never the new shape; a beat that lists kbf among its services is a 400.
#[tokio::test]
async fn a_service_order_goes_only_to_an_updater_that_sent_services() {
    let mut rig = Rig::new("http-services");
    let list = format!(
        "{}services = [\"kbf\", \"other\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(crate::services::Services::parse(SERVICES_TEXT).expect("services"));
    for body in [
        r#"{"version":"0.0.19+g123456abcdef-1","min_up":{"storage":1,"frontend":1}}"#,
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
    let (code, old) = raw(
        &rig,
        Surface::Heartbeat,
        &updater("d1", "daemon"),
        request(
            "POST",
            HEARTBEAT_PATH,
            None,
            &GOLDEN_BEAT.replace("0.0.18+gabcdefabcdef-2", "0.0.19+g123456abcdef-1"),
        ),
    )
    .await;
    assert_eq!(
        (code, String::from_utf8(old).expect("text")),
        (200, r#"{"reply":"nothing"}"#.to_owned())
    );
    let (code, new) = raw(
        &rig,
        Surface::Heartbeat,
        &updater("d2", "daemon"),
        request("POST", HEARTBEAT_PATH, None, NEW_GOLDEN_BEAT),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        String::from_utf8(new).expect("text"),
        r#"{"reply":"update","service":"other","version":"3.0"}"#
    );
    let kbf_inside = NEW_GOLDEN_BEAT.replace(r#""name":"other""#, r#""name":"kbf""#);
    let (code, dropped) = raw(
        &rig,
        Surface::Heartbeat,
        &updater("d3", "daemon"),
        request("POST", HEARTBEAT_PATH, None, &kbf_inside),
    )
    .await;
    assert_eq!(code, 200);
    assert!(
        !String::from_utf8(dropped)
            .expect("text")
            .contains("service")
    );
}

/// Catches: a host losing its kbf updates over a bad services list, or a bad list going
/// unseen: a heartbeat that lists kbf, or a name twice, is answered 200 as a kbf-only host
/// (it is told the kbf update, in the old reply shape, and its services are not noted),
/// `services_dropped` in the status goes up by one each time, and a heartbeat that is wrong
/// otherwise (no version) is still 400 and not counted. The count is 0 before any.
#[tokio::test]
async fn a_bad_services_list_is_dropped_counted_and_the_host_still_updates() {
    let rig = Rig::new("http-bad-services");
    let who = updater("d1", "daemon");
    rig.set(STATE).await;
    rig.at(2_000);
    assert_eq!(status(&rig).await["services_dropped"], json!(0));
    let entry = |name: &str| format!(r#"{{"name":"{name}","version":"1.0","health":"healthy"}}"#);
    let with = |list: &str| {
        format!(
            r#"{{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":[{list}]}}"#
        )
    };
    let kbf = with(&entry("kbf"));
    let twice = with(&format!("{},{}", entry("other"), entry("other")));
    for (i, body) in [kbf, twice].iter().enumerate() {
        let (code, reply) = call(
            &rig,
            Surface::Heartbeat,
            &who,
            request("POST", HEARTBEAT_PATH, None, body),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(reply, json!({"reply": "update", "version": "2.0"}));
        assert_eq!(status(&rig).await["services_dropped"], json!(i + 1));
    }
    let status = status(&rig).await;
    assert_eq!(hosts(&status), [("d1".to_owned(), "daemon".to_owned())]);
    assert_eq!(status["hosts"][0].get("services"), None);
    let (code, _) = call(
        &rig,
        Surface::Heartbeat,
        &who,
        request(
            "POST",
            HEARTBEAT_PATH,
            None,
            r#"{"platform":"linux-x86_64","health":"healthy","services":[]}"#,
        ),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(status_count(&rig).await, json!(2));
}

async fn status_count(rig: &Rig) -> Value {
    status(rig).await["services_dropped"].clone()
}

async fn get_raw(rig: &Rig, path: &str, bearer: &str) -> (u16, String) {
    let (code, bytes) = raw(
        rig,
        Surface::Control,
        &None,
        request("GET", path, Some(bearer), ""),
    )
    .await;
    (code, String::from_utf8(bytes).expect("text"))
}

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

/// What `GET /v1/convergence` answered for a farm of one storage host at 2.0 before a
/// service could be asked for; `cargo xtask deploy-farm` and the update rehearsal read it.
const GOLDEN_CONVERGENCE: &str = concat!(
    r#"{"desired":"2.0","quiet":false,"state":"converged","#,
    r#""roles":[{"role":"storage","at_desired":1,"updating":0,"failed":0,"down":0}]}"#
);

/// What `GET /v1/status` answered for the same farm before a per-service section was added
/// (with `services_dropped`, a count added since): the new section comes after these keys,
/// and none of them moved.
const GOLDEN_STATUS: &str = concat!(
    r#"{"desired":{"version":"2.0","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":1,"frontend":0}},"#,
    r#""services":{"kbf":{"version":"2.0","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":1,"frontend":0}}},"#,
    r#""quiet":false,"services_dropped":0,"hosts":[{"node_id":"st1","role":"storage","live":true,"up":true,"holds_slot":false,"silent_secs":0,"#,
    r#""platform":"linux-x86_64","version":"2.0","health":"healthy","last_update":null,"updating":null}]"#
);

/// Pins: the bytes of kbf's status and convergence stay what they were. With only kbf
/// managed, the convergence is byte for byte the old answer followed by the build `commit`,
/// whether or not `?service=kbf` is given, and the status is the old answer followed by the
/// new `per_service` key (kbf's `desired` and `roles` as they were, then `hosts`, `up`,
/// `slots_held` and `convergence`) and then `commit`.
#[tokio::test]
async fn the_kbf_status_and_convergence_bytes_are_unchanged() {
    let rig = Rig::new("http-golden-status");
    let (code, _) = raw(
        &rig,
        Surface::Control,
        &None,
        request(
            "POST",
            DESIRED_PATH,
            Some(&ci_token()),
            r#"{"version":"2.0","min_up":{"storage":1,"frontend":0}}"#,
        ),
    )
    .await;
    assert_eq!(code, 200);
    rig.at(2_000);
    beat_from(
        &rig,
        "st1",
        "storage",
        r#"{"platform":"linux-x86_64","version":"2.0","health":"healthy"}"#,
    )
    .await;
    // Each answer has kept its bytes, and ends with the build commit since that was added.
    let commit = format!(r#","commit":"{}"}}"#, crate::BUILD_COMMIT);
    let converged = GOLDEN_CONVERGENCE
        .strip_suffix('}')
        .expect("an object")
        .to_owned()
        + &commit;
    for path in [CONVERGENCE_PATH, "/v1/convergence?service=kbf"] {
        assert_eq!(
            get_raw(&rig, path, &ci_token()).await,
            (200, converged.clone())
        );
    }
    let (code, status) = get_raw(&rig, STATUS_PATH, OPERATOR).await;
    assert_eq!(code, 200);
    let tail = concat!(
        r#","per_service":{"kbf":{"desired":{"version":"2.0","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":1,"frontend":0}},"#,
        r#""roles":[{"role":"storage","at_desired":1,"updating":0,"failed":0,"down":0}],"#,
        r#""hosts":[{"node_id":"st1","role":"storage","live":true,"up":true,"holds_slot":false,"silent_secs":0,"#,
        r#""platform":"linux-x86_64","version":"2.0","health":"healthy","last_update":null,"updating":null,"hold":"up_to_date"}],"#,
        r#""up":1,"slots_held":0,"convergence":"#,
    );
    // The convergence in the status is the answer of `GET /v1/convergence`, with its commit.
    assert_eq!(
        status,
        format!("{GOLDEN_STATUS}{tail}{converged}}}}}{commit}")
    );
}

/// Behaviour: `?service=` picks the service a convergence read is about, over HTTP. With
/// `other` managed and its only daemon behind, `?service=other` is in progress while the
/// read without a query (kbf) is converged; the status lists both; a service nobody manages
/// is 404 `not_found`, and a query that is not exactly `service=<name>` is 400.
#[tokio::test]
async fn the_convergence_is_read_per_service_with_a_query() {
    let mut rig = Rig::new("http-converge-service");
    let list = format!(
        "{}services = [\"kbf\", \"other\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(crate::services::Services::parse(SERVICES_TEXT).expect("services"));
    for body in [
        r#"{"version":"0.0.19+g123456abcdef-1","min_up":{"storage":0,"frontend":0}}"#,
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
    let rolling = NEW_GOLDEN_BEAT.replace(
        r#""last_update":null,"updating":null}]}"#,
        r#""last_update":null,"updating":"3.0"}]}"#,
    );
    beat_from(&rig, "d1", "daemon", &rolling).await;
    let (code, kbf) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(
        (code.as_u16(), kbf["state"].as_str()),
        (200, Some("converged"))
    );
    let path = "/v1/convergence?service=other";
    let (code, other) = call(
        &rig,
        Surface::Control,
        &None,
        request("GET", path, Some(&ci_token()), ""),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(other["state"], "in_progress");
    assert_eq!(other["desired"], "3.0");
    assert_eq!(
        other["roles"],
        json!([{"role": "daemon", "at_desired": 0, "updating": 1, "failed": 0, "down": 0}])
    );
    let status = status(&rig).await;
    assert_eq!(status["per_service"]["other"]["desired"]["version"], "3.0");
    assert_eq!(status["per_service"]["other"]["roles"][0]["role"], "daemon");
    assert_eq!(status["per_service"]["kbf"]["roles"][0]["at_desired"], 1);
    assert_eq!(status["per_service"]["other"]["roles"][0]["updating"], 1);
    for (query, want) in [
        ("service=nope", 404),
        ("service=", 404),
        ("service=other&service=kbf", 400),
        ("name=other", 400),
        ("service", 400),
    ] {
        let (code, _) = raw(
            &rig,
            Surface::Control,
            &None,
            request(
                "GET",
                &format!("/v1/convergence?{query}"),
                Some(&ci_token()),
                "",
            ),
        )
        .await;
        assert_eq!(code, want, "{query}");
    }
    let (code, body) = call(
        &rig,
        Surface::Control,
        &None,
        request("GET", "/v1/convergence?service=nope", Some(OPERATOR), ""),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{body}");
    let (code, body) = call(
        &rig,
        Surface::Control,
        &None,
        request("GET", "/v1/convergence?service=nope", Some(&ci_token()), ""),
    )
    .await;
    assert_eq!(
        (code, body),
        (StatusCode::NOT_FOUND, json!({"error": "not_found"}))
    );
}
