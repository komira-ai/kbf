//! The probe: before a write, and on request.

use kbf_types::object_store_api::{ConnectionPutAnswer, ErrorBody, VerifyAnswer};

use super::harness::{BIND_BODY, Farm, SENTINEL, conn_body, registered};

const CONN: &str = "/v1/object-stores/connections/cold";

// Catches a probe that runs before a binding names a bucket (it has nothing to probe), one
// that is skipped once a binding does, a probe given the stored key instead of the new one
// (the rotation would be checked with the old key), and a target other than the connection's
// endpoint and the cold binding's bucket and prefix.
#[tokio::test]
async fn a_put_probes_the_bound_bucket_with_the_new_key() {
    let farm = Farm::new();
    let first = farm.call("PUT", CONN, &conn_body(SENTINEL)).await;
    let first: ConnectionPutAnswer = first.json();
    assert_eq!(first.verified, None, "no binding yet: no bucket to probe");
    assert!(farm.probe.calls().is_empty());

    assert_eq!(
        farm.call("PUT", "/v1/object-stores/bindings/cold", BIND_BODY)
            .await
            .status,
        201
    );
    farm.probe.calls.lock().expect("calls").clear();

    let conditional = conn_body("the-new-secret").replace(
        "}",
        r#","conditional_put":true,"region":"eu-1","min_put_rate":512}"#,
    );
    let rotated = farm.call("PUT", CONN, &conditional).await;
    assert_eq!(rotated.status, 200, "{}", rotated.body);
    let answer: ConnectionPutAnswer = rotated.json();
    assert_eq!(
        answer.verified,
        Some(super::harness::ScriptedProbe::passing())
    );
    let calls = farm.probe.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].endpoint, "http://s3.example:9000");
    assert_eq!(
        (calls[0].bucket.as_str(), calls[0].prefix.as_str()),
        ("farm-bucket", "cold/")
    );
    assert_eq!(
        calls[0].secret, "the-new-secret",
        "the key being registered, not the stored one"
    );
    assert_eq!(
        (calls[0].access_key_id.as_str(), calls[0].region.as_str()),
        ("AKID", "eu-1")
    );
    assert!(calls[0].conditional_put);
    assert_eq!(
        calls[0].min_put_rate.map(std::num::NonZeroU64::get),
        Some(512)
    );
}

// Catches a failed probe that commits anyway (a registration every node would fail on), one
// that leaves a row, a file or an audit row, a failure reported as something other than 422
// `probe_failed` with the steps, a store's text in the answer, and `verify:false` that still
// probes (or one that does not skip it).
#[tokio::test]
async fn a_failed_probe_is_422_and_leaves_no_row_and_no_file() {
    let farm = Farm::new();
    registered(&farm).await;
    let (generation, files) = (farm.generation(), farm.files());
    let audit_before = farm.state_dump();
    farm.probe.fail_at_get();
    let rotated = farm.call("PUT", CONN, &conn_body("the-new-secret")).await;
    assert_eq!(rotated.status, 422, "{}", rotated.body);
    let body: ErrorBody = rotated.json();
    assert_eq!(body.code, "probe_failed");
    let steps = body.verified.expect("the steps");
    assert!(steps.put && !steps.get && !steps.ok);
    assert_eq!(farm.generation(), generation);
    assert_eq!(farm.files(), files, "no file for the key that failed");
    assert_eq!(
        farm.state_dump(),
        audit_before,
        "no row and no audit row either"
    );
    assert!(!rotated.body.contains("the-new-secret"));

    // The operator may register without the probe.
    let unchecked = conn_body("the-new-secret").replace("}", r#","verify":false}"#);
    let calls_before = farm.probe.calls().len();
    let accepted = farm.call("PUT", CONN, &unchecked).await;
    assert_eq!(accepted.status, 200, "{}", accepted.body);
    assert_eq!(
        farm.probe.calls().len(),
        calls_before,
        "verify:false asks nothing of the store"
    );
}

// Catches `:verify` that changes state (it is a probe, not a write), one that uses a key other
// than the stored one, one that cannot take a bucket from the body, and the missing-bucket
// case answered as a pass.
#[tokio::test]
async fn verify_probes_with_the_stored_key_and_changes_nothing() {
    let farm = Farm::new();
    registered(&farm).await;
    farm.probe.calls.lock().expect("calls").clear();
    let before = farm.state_dump();

    let by_binding = farm.call("POST", &format!("{CONN}:verify"), "").await;
    assert_eq!(by_binding.status, 200, "{}", by_binding.body);
    let answer: VerifyAnswer = by_binding.json();
    assert_eq!(answer.name.as_str(), "cold");
    assert_eq!(answer.verified, super::harness::ScriptedProbe::passing());
    let calls = farm.probe.calls();
    assert_eq!(calls[0].secret, SENTINEL, "the stored key");
    assert_eq!(
        (calls[0].bucket.as_str(), calls[0].prefix.as_str()),
        ("farm-bucket", "cold/")
    );

    let by_body = farm
        .call(
            "POST",
            &format!("{CONN}:verify"),
            r#"{"bucket":"another-one","prefix":"x/"}"#,
        )
        .await;
    assert_eq!(by_body.status, 200);
    let calls = farm.probe.calls();
    assert_eq!(
        (calls[1].bucket.as_str(), calls[1].prefix.as_str()),
        ("another-one", "x/")
    );
    let by_bucket_only = farm
        .call(
            "POST",
            &format!("{CONN}:verify"),
            r#"{"bucket":"another-one"}"#,
        )
        .await;
    assert_eq!(by_bucket_only.status, 200);
    assert_eq!(
        farm.probe.calls()[2].prefix,
        "",
        "a bucket named alone is probed at its root"
    );
    let by_prefix_only = farm
        .call("POST", &format!("{CONN}:verify"), r#"{"prefix":"y/"}"#)
        .await;
    assert_eq!(by_prefix_only.status, 200);
    let calls = farm.probe.calls();
    assert_eq!(
        (calls[3].bucket.as_str(), calls[3].prefix.as_str()),
        ("farm-bucket", "y/")
    );

    assert_eq!(
        farm.state_dump(),
        before,
        "nothing was written, not even an audit row"
    );
    assert_eq!(farm.generation(), 2);

    farm.probe.fail_at_get();
    let failed = farm.call("POST", &format!("{CONN}:verify"), "{}").await;
    assert_eq!(
        (failed.status, failed.code().as_str()),
        (422, "probe_failed")
    );
}

// Catches a verify with no bucket to probe answered as a pass, an unknown connection, a bad
// bucket, a mistyped body field, and a custom method that is not `verify`.
#[tokio::test]
async fn verify_refuses_what_it_cannot_probe() {
    let farm = Farm::new();
    let missing = farm.call("POST", &format!("{CONN}:verify"), "{}").await;
    assert_eq!(
        (missing.status, missing.code().as_str()),
        (404, "no_such_connection")
    );

    farm.call("PUT", CONN, &conn_body(SENTINEL)).await;
    let nowhere = farm.call("POST", &format!("{CONN}:verify"), "{}").await;
    assert_eq!(
        (nowhere.status, nowhere.code().as_str()),
        (422, "no_bucket")
    );
    let bad_bucket = farm
        .call(
            "POST",
            &format!("{CONN}:verify"),
            r#"{"bucket":"NOT A BUCKET"}"#,
        )
        .await;
    assert_eq!(
        (bad_bucket.status, bad_bucket.code().as_str()),
        (422, "invalid_bucket")
    );
    let bad_prefix = farm
        .call(
            "POST",
            &format!("{CONN}:verify"),
            r#"{"bucket":"farm-bucket","prefix":"a/../b"}"#,
        )
        .await;
    assert_eq!(
        (bad_prefix.status, bad_prefix.code().as_str()),
        (422, "invalid_prefix")
    );
    let typo = farm
        .call("POST", &format!("{CONN}:verify"), r#"{"bukket":"x"}"#)
        .await;
    assert_eq!((typo.status, typo.code().as_str()), (400, "bad_body"));
    let other = farm.call("POST", &format!("{CONN}:probe"), "{}").await;
    assert_eq!(
        (other.status, other.code().as_str()),
        (404, "unknown_action")
    );
    let bare = farm.call("POST", CONN, "{}").await;
    assert_eq!((bare.status, bare.code().as_str()), (404, "unknown_action"));
    let badname = farm
        .call("POST", "/v1/object-stores/connections/..:verify", "{}")
        .await;
    assert_eq!((badname.status, badname.code().as_str()), (400, "bad_name"));
    assert!(
        farm.probe.calls().is_empty(),
        "nothing above reached a store"
    );
}

// Catches a secret file that is missing, wide or replaced by a link being read anyway, or its
// failure reported as a client error: the routes fail closed with 500 `secret_missing` and no
// path or file name in the text.
#[tokio::test]
async fn a_missing_or_wide_secret_file_fails_closed() {
    use std::os::unix::fs::PermissionsExt as _;
    let farm = Farm::new();
    registered(&farm).await;
    farm.probe.calls.lock().expect("calls").clear();
    let file = farm.dir.join(&farm.files()[0]);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let wide = farm.call("POST", &format!("{CONN}:verify"), "").await;
    assert_eq!(
        (wide.status, wide.code().as_str()),
        (500, "secret_missing"),
        "{}",
        wide.body
    );
    assert!(
        !wide.body.contains(&farm.dir.display().to_string()),
        "{}",
        wide.body
    );
    std::fs::remove_file(&file).expect("remove");
    let gone = farm.call("POST", &format!("{CONN}:verify"), "").await;
    assert_eq!((gone.status, gone.code().as_str()), (500, "secret_missing"));
    assert!(
        farm.probe.calls().is_empty(),
        "no probe ran with a key that could not be read"
    );
}

// Catches a blocking task that dies (a bug in a probe, a panic in the registry) taking the
// request down without an answer, or leaking the panic text: the route answers 500 `internal`
// and the registry is untouched.
#[tokio::test]
async fn a_task_that_panics_is_500_internal() {
    use std::sync::atomic::Ordering::SeqCst;
    let farm = Farm::new();
    let unchecked = conn_body(SENTINEL).replace("}", r#","verify":false}"#);
    assert_eq!(farm.call("PUT", CONN, &unchecked).await.status, 201);
    farm.probe.panics.store(true, SeqCst);
    let bind = farm
        .call("PUT", "/v1/object-stores/bindings/cold", BIND_BODY)
        .await;
    assert_eq!(
        (bind.status, bind.code().as_str()),
        (500, "internal"),
        "{}",
        bind.body
    );
    assert!(!bind.body.contains("panics"), "{}", bind.body);
    assert_eq!(farm.generation(), 1);
}
