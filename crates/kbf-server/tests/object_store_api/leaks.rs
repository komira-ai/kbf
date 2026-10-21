//! The secret key reaches nothing but its file: not an answer, not an error body, not a row,
//! not a debug print.

use kbf_types::object_store_api::AuditList;

use super::harness::{Farm, SENTINEL, TOKEN, conn_body, send};

/// Every answer and the stored bytes after a spread of calls that touch the secret.
#[tokio::test]
async fn the_secret_is_in_its_file_and_in_nothing_else() {
    let farm = Farm::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut note = |what: &str, reply: &super::harness::Reply| {
        seen.push((
            what.to_owned(),
            format!("{:?}\n{}", reply.headers, reply.body),
        ));
    };

    // Success paths.
    let put = farm
        .call(
            "PUT",
            "/v1/object-stores/connections/cold",
            &conn_body(SENTINEL),
        )
        .await;
    note("put", &put);
    let bind = farm
        .call(
            "PUT",
            "/v1/object-stores/bindings/cold",
            super::harness::BIND_BODY,
        )
        .await;
    note("bind", &bind);
    for path in [
        "/v1/object-stores",
        "/v1/object-stores/connections",
        "/v1/object-stores/connections/cold",
        "/v1/object-stores/audit",
    ] {
        note(path, &farm.call("GET", path, "").await);
    }
    note(
        "verify",
        &farm
            .call("POST", "/v1/object-stores/connections/cold:verify", "")
            .await,
    );
    note(
        "unchanged",
        &farm
            .call(
                "PUT",
                "/v1/object-stores/connections/cold",
                &conn_body(SENTINEL),
            )
            .await,
    );

    // The refusals, each of which holds the secret in the request: the gate, the parse, the
    // checks, the probe, the registry.
    let bearer = format!("Bearer {TOKEN}");
    let json = ("content-type", "application/json");
    let sentinel_body = conn_body(SENTINEL);
    note(
        "401",
        &farm
            .with(
                "PUT",
                "/v1/object-stores/connections/cold",
                &[json],
                &sentinel_body,
            )
            .await,
    );
    note(
        "415",
        &farm
            .with(
                "PUT",
                "/v1/object-stores/connections/cold",
                &[("authorization", &bearer)],
                &sentinel_body,
            )
            .await,
    );
    note(
        "403",
        &farm
            .with(
                "PUT",
                "/v1/object-stores/connections/cold",
                &[json, ("authorization", &bearer), ("origin", "http://x")],
                &sentinel_body,
            )
            .await,
    );
    note(
        "403 peer",
        &send(
            &farm.router,
            "192.0.2.1:9".parse().expect("addr"),
            "PUT",
            "/v1/object-stores/connections/cold",
            &[json, ("authorization", &bearer)],
            sentinel_body.as_bytes(),
        )
        .await,
    );
    // The secret in a field of the wrong type, as a field name, in broken JSON, in a long body.
    let wrong_type = sentinel_body.replace("}", &format!(r#","conditional_put":"{SENTINEL}"}}"#));
    note(
        "wrong type",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &wrong_type)
            .await,
    );
    let wrong_rate = sentinel_body.replace("}", &format!(r#","min_put_rate":"{SENTINEL}"}}"#));
    note(
        "wrong rate",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &wrong_rate)
            .await,
    );
    let unknown = sentinel_body.replace("}", &format!(r#","{SENTINEL}":1}}"#));
    note(
        "unknown field",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &unknown)
            .await,
    );
    note(
        "not json",
        &farm
            .call(
                "PUT",
                "/v1/object-stores/connections/cold",
                &format!("{SENTINEL} not json"),
            )
            .await,
    );
    let truncated = &sentinel_body[..sentinel_body.len() - 3];
    note(
        "truncated",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", truncated)
            .await,
    );
    let big = sentinel_body.replace(
        "}",
        &format!(r#","region":"{}{SENTINEL}"}}"#, "x".repeat(20_000)),
    );
    note(
        "413",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &big)
            .await,
    );
    let tls = sentinel_body.replace("http://", "https://");
    note(
        "tls",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &tls)
            .await,
    );
    let userinfo = sentinel_body.replace("http://", &format!("http://u:{SENTINEL}@"));
    note(
        "userinfo",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &userinfo)
            .await,
    );
    let identity = sentinel_body.replace("s3.example", "elsewhere.example");
    note(
        "identity",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &identity)
            .await,
    );
    let bad_key = format!(
        r#"{{"endpoint":"http://s3.example:9000","access_key_id":"A K","secret_access_key":"{SENTINEL}"}}"#
    );
    note(
        "invalid field",
        &farm
            .call("PUT", "/v1/object-stores/connections/cold", &bad_key)
            .await,
    );
    farm.probe.fail_at_get();
    note(
        "probe",
        &farm
            .call(
                "PUT",
                "/v1/object-stores/connections/cold",
                &conn_body(&format!("{SENTINEL}-2")),
            )
            .await,
    );
    note(
        "probe verify",
        &farm
            .call("POST", "/v1/object-stores/connections/cold:verify", "")
            .await,
    );
    note(
        "bad name",
        &farm
            .call(
                "PUT",
                &format!("/v1/object-stores/connections/{SENTINEL}"),
                &sentinel_body,
            )
            .await,
    );
    farm.flaky
        .fail_writes
        .store(true, std::sync::atomic::Ordering::SeqCst);
    farm.flaky
        .fail_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    note(
        "store down",
        &farm
            .call(
                "PUT",
                "/v1/object-stores/connections/cold",
                &conn_body(&format!("{SENTINEL}-3")),
            )
            .await,
    );
    farm.flaky
        .fail_writes
        .store(false, std::sync::atomic::Ordering::SeqCst);
    farm.flaky
        .fail_reads
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // A vacuous scan proves nothing: the secret is where it belongs, and only there.
    let files = farm.files();
    assert_eq!(files.len(), 1, "{files:?}");
    let on_disk = std::fs::read_to_string(farm.dir.join(&files[0])).expect("the secret file");
    assert!(
        on_disk.contains(SENTINEL),
        "the scan below would pass for nothing"
    );
    assert!(
        !farm.state_dump().contains(SENTINEL),
        "state rows hold no secret"
    );
    assert!(!farm.state_dump().contains("SENTINEL"));
    for (what, text) in &seen {
        assert!(!text.contains(SENTINEL), "{what}: {text}");
        assert!(!text.contains("SENTINEL"), "{what}: {text}");
    }
    let audit: AuditList = farm.call("GET", "/v1/object-stores/audit", "").await.json();
    assert!(!format!("{audit:?}").contains("SENTINEL"));

    // The debug prints of what the routes hold.
    let api = format!("{:?}", farm.registry);
    assert!(!api.contains("SENTINEL"), "{api}");
    assert!(
        seen.len() > 20,
        "the spread of calls is the point: {}",
        seen.len()
    );
}

// Catches a refusal that carries the store's or the parser's own words: an error body is the
// route's sentence and a closed code and nothing else, whatever the request held.
#[tokio::test]
async fn an_error_body_holds_only_a_sentence_and_a_closed_code() {
    let farm = Farm::new();
    let wrong_type = conn_body(SENTINEL).replace("}", r#","conditional_put":"yes please"}"#);
    let reply = farm
        .call("PUT", "/v1/object-stores/connections/cold", &wrong_type)
        .await;
    assert_eq!(reply.status, 400);
    let value: serde_json::Value = serde_json::from_str(&reply.body).expect("json");
    let keys: Vec<_> = value.as_object().expect("object").keys().cloned().collect();
    assert_eq!(keys, ["code", "error"]);
    assert!(!reply.body.contains("yes please"), "{}", reply.body);
    assert!(
        !reply.body.contains("invalid type"),
        "serde's own text: {}",
        reply.body
    );
}
