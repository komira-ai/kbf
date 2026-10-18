//! Connections: create, replace, read, delete, and what each refuses.

use kbf_types::object_store_api::{ConnectionInfo, ConnectionList, ConnectionPutAnswer, Deleted};

use super::harness::{Farm, SENTINEL, conn_body, registered};

const CONNS: &str = "/v1/object-stores/connections";

fn at(name: &str) -> String {
    format!("{CONNS}/{name}")
}

// Catches a first PUT that answers anything but 201 with the connection's name, the new
// generation, `changed`, the key id and `secret_set` (and no secret); one that does not
// leave exactly one 0600 file; and a read of the list or of one connection that carries
// the secret or a field the operator did not send.
#[tokio::test]
async fn a_first_put_creates_the_connection_and_reads_show_it_without_the_secret() {
    let farm = Farm::new();
    let body = r#"{"endpoint":"http://s3.example:9000","region":"eu-west-1","access_key_id":"AKID","secret_access_key":"SENTINEL-s3-secret-9f8e7d6c5b4a","conditional_put":true,"min_put_rate":4096,"verify":false}"#;
    let put = farm.call("PUT", &at("cold"), body).await;
    assert_eq!(put.status, 201, "{}", put.body);
    assert_eq!(put.header("cache-control"), Some("no-store"));
    let answer: ConnectionPutAnswer = put.json();
    assert_eq!(answer.name.as_str(), "cold");
    assert_eq!((answer.generation, answer.changed), (1, true));
    assert_eq!(
        (answer.access_key_id.as_str(), answer.secret_set),
        ("AKID", true)
    );
    assert_eq!(answer.verified, None, "verify:false runs no probe");
    assert!(!put.body.contains(SENTINEL));
    assert_eq!(farm.files(), ["cold.0000000000000001.json"]);

    let one = farm.call("GET", &at("cold"), "").await;
    assert_eq!(one.status, 200);
    let info: ConnectionInfo = one.json();
    assert_eq!(
        (
            info.endpoint.as_str(),
            info.region.as_str(),
            info.access_key_id.as_str()
        ),
        ("http://s3.example:9000", "eu-west-1", "AKID")
    );
    assert!(info.conditional_put);
    assert_eq!(info.min_put_rate.map(std::num::NonZeroU64::get), Some(4096));
    assert_eq!(info.updated_generation, 1);

    let list = farm.call("GET", CONNS, "").await;
    assert_eq!(list.status, 200);
    let list: ConnectionList = list.json();
    assert_eq!(list.connections, [info]);
    assert!(!one.body.contains(SENTINEL));

    let missing = farm.call("GET", &at("other"), "").await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code(), "no_such_connection");
}

// Catches an idempotent retry that bumps the generation (nodes would refetch for nothing and
// `converged` would flap), answers `changed:true`, or writes a second file; and a real change
// (a new secret, a new region) that answers 201 or leaves the generation where it was.
#[tokio::test]
async fn the_same_put_again_is_200_unchanged_and_a_change_is_200_changed() {
    let farm = Farm::new();
    let first = farm.call("PUT", &at("cold"), &conn_body(SENTINEL)).await;
    assert_eq!(first.status, 201);

    let again = farm.call("PUT", &at("cold"), &conn_body(SENTINEL)).await;
    assert_eq!(again.status, 200, "{}", again.body);
    let answer: ConnectionPutAnswer = again.json();
    assert_eq!((answer.generation, answer.changed), (1, false));
    assert_eq!(farm.generation(), 1);
    assert_eq!(farm.files(), ["cold.0000000000000001.json"]);

    let rotated = farm
        .call("PUT", &at("cold"), &conn_body("a-different-secret"))
        .await;
    assert_eq!(rotated.status, 200);
    let answer: ConnectionPutAnswer = rotated.json();
    assert_eq!((answer.generation, answer.changed), (2, true));
    assert_eq!(
        farm.files(),
        ["cold.0000000000000002.json"],
        "the old version is gone"
    );

    let moved = conn_body("a-different-secret").replace("}", r#","region":"us-west-2"}"#);
    let region: ConnectionPutAnswer = farm.call("PUT", &at("cold"), &moved).await.json();
    assert_eq!((region.generation, region.changed), (3, true));
}

// Catches a name that reaches the registry unchecked (it becomes a file name and a URL
// segment): `..`, an encoded slash, a colon, upper case, a dot and a name of 64 characters
// all answer 400 `bad_name`, on every method that takes a name, and no file appears
// anywhere near the secrets directory.
#[tokio::test]
async fn a_bad_connection_name_is_400_and_touches_no_file() {
    let farm = Farm::new();
    let parent = farm.dir.parent().expect("parent").to_path_buf();
    let before = super::harness::listing(&parent);
    let long = "x".repeat(64);
    for name in [
        "..", "a%2fb", "a%2e%2e", "A", "a.b", "-a", "a:b", "%00", &long,
    ] {
        for (method, body) in [
            ("PUT", conn_body(SENTINEL)),
            ("GET", String::new()),
            ("DELETE", String::new()),
        ] {
            let reply = farm.call(method, &at(name), &body).await;
            assert_eq!(reply.status, 400, "{method} {name}: {}", reply.body);
            assert_eq!(reply.code(), "bad_name", "{method} {name}");
        }
    }
    // The traversal an unchecked name would allow: nothing outside the directory changes.
    assert_eq!(super::harness::listing(&parent), before);
    assert!(farm.files().is_empty());
    assert_eq!(farm.generation(), 0);
}

// Catches a body that is taken with a field it does not know (a typo in `conditional_put`
// would silently keep the default), a missing required field, a wrong type, JSON that is not
// JSON, an empty body, and one larger than the limit (413): each is refused with a closed code
// and writes nothing.
#[tokio::test]
async fn a_malformed_body_is_refused_whole() {
    let farm = Farm::new();
    let good = conn_body(SENTINEL);
    let cases = [
        (
            good.replace("}", r#","conditional_putt":true}"#),
            400,
            "bad_body",
        ),
        (
            r#"{"endpoint":"http://s3.example:9000","access_key_id":"AKID"}"#.to_owned(),
            400,
            "bad_body",
        ),
        (
            good.replace("}", r#","min_put_rate":"fast"}"#),
            400,
            "bad_body",
        ),
        (good.replace("}", r#","min_put_rate":0}"#), 400, "bad_body"),
        ("not json".to_owned(), 400, "bad_body"),
        (String::new(), 400, "bad_body"),
        ("[]".to_owned(), 400, "bad_body"),
        (
            good.replace("}", &format!(r#","region":"{}"}}"#, "x".repeat(20_000))),
            413,
            "payload_too_large",
        ),
    ];
    for (body, status, code) in cases {
        let reply = farm.call("PUT", &at("cold"), &body).await;
        assert_eq!(reply.status, status, "{}", &reply.body);
        assert_eq!(reply.code(), code);
        assert!(!reply.body.contains(SENTINEL));
    }
    assert_eq!(farm.generation(), 0);
    assert!(farm.files().is_empty());
}

// Catches field checks that let through an empty key id or secret (a connection no node can
// use), a control character or space in either (it would be signed into a header), a
// region that is not a region name, and a key id or secret of absurd length (a row bound).
#[tokio::test]
async fn a_field_that_cannot_be_a_key_or_region_is_422() {
    let farm = Farm::new();
    let with = |key: &str, secret: &str, extra: &str| {
        format!(
            r#"{{"endpoint":"http://s3.example:9000","access_key_id":"{key}","secret_access_key":"{secret}"{extra}}}"#
        )
    };
    let cases = [
        with("", SENTINEL, ""),
        with("AKID", "", ""),
        with("AK ID", SENTINEL, ""),
        with("AKID", "has space", ""),
        with("AKID", "tab\\there", ""),
        with(&"K".repeat(300), SENTINEL, ""),
        with("AKID", &"s".repeat(2000), ""),
        with("AKID", SENTINEL, r#","region":"""#),
    ];
    for body in cases {
        let reply = farm.call("PUT", &at("cold"), &body).await;
        assert_eq!(reply.status, 422, "{}", reply.body);
        assert_eq!(reply.code(), "invalid_field");
        assert!(!reply.body.contains(SENTINEL));
    }
    for region in ["US EAST", "us_east", "-x", &"r".repeat(65)] {
        let body = with("AKID", SENTINEL, &format!(r#","region":"{region}""#));
        let reply = farm.call("PUT", &at("cold"), &body).await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (422, "invalid_field"),
            "{region}"
        );
    }
    assert_eq!(farm.generation(), 0);
    assert!(farm.probe.calls().is_empty());
}

// Catches an `https` endpoint accepted (S3Store refuses TLS, so every node would fail on a
// registration the API accepted), a refusal that is not distinguished from other bad
// endpoints, and an endpoint with a path, userinfo or the wrong scheme accepted.
#[tokio::test]
async fn an_https_endpoint_is_422_tls_unsupported_and_other_bad_endpoints_422() {
    let farm = Farm::new();
    let put = |endpoint: &str| {
        let body = conn_body(SENTINEL).replace("http://s3.example:9000", endpoint);
        let farm = &farm;
        async move { farm.call("PUT", &at("cold"), &body).await }
    };
    let tls = put("https://s3.example:9000").await;
    assert_eq!((tls.status, tls.code().as_str()), (422, "tls_unsupported"));
    for bad in [
        "ftp://s3.example",
        "s3.example:9000",
        "http://user:hunter2@s3.example",
        "http://s3.example/p",
        "",
    ] {
        let reply = put(bad).await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (422, "invalid_endpoint"),
            "{bad}"
        );
        assert!(!reply.body.contains("hunter2"), "{}", reply.body);
    }
    assert_eq!(farm.generation(), 0);
    assert!(
        farm.probe.calls().is_empty(),
        "a refused endpoint is never contacted"
    );
}

// Catches `expect_generation` ignored (two operators would overwrite each other), a mismatch
// that writes anyway, and a match that is refused.
#[tokio::test]
async fn expect_generation_guards_the_write() {
    let farm = Farm::new();
    farm.call("PUT", &at("cold"), &conn_body(SENTINEL)).await;
    let stale = conn_body("rotated-secret").replace("}", r#","expect_generation":0}"#);
    let reply = farm.call("PUT", &at("cold"), &stale).await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (409, "generation_mismatch")
    );
    assert_eq!(farm.generation(), 1);
    assert_eq!(
        farm.files(),
        ["cold.0000000000000001.json"],
        "the refused write left no file"
    );
    let fresh = conn_body("rotated-secret").replace("}", r#","expect_generation":1}"#);
    let reply = farm.call("PUT", &at("cold"), &fresh).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(farm.generation(), 2);
}

// Catches a delete of a bound connection (nodes would hold a binding to nothing), a delete
// that leaves the file, a missing connection answered 200, and a delete that does not move
// the generation (the nodes would never learn).
#[tokio::test]
async fn delete_refuses_a_bound_connection_and_removes_an_unbound_one() {
    let farm = Farm::new();
    registered(&farm).await;
    let bound = farm.call("DELETE", &at("cold"), "").await;
    assert_eq!((bound.status, bound.code().as_str()), (409, "in_use"));
    assert_eq!(farm.generation(), 2);

    farm.call("PUT", &at("spare"), &conn_body("spare-secret"))
        .await;
    assert_eq!(farm.files().len(), 2);
    let gone = farm.call("DELETE", &at("spare"), "").await;
    assert_eq!(gone.status, 200, "{}", gone.body);
    assert_eq!(gone.json::<Deleted>(), Deleted { generation: 4 });
    assert_eq!(farm.files(), ["cold.0000000000000001.json"]);
    assert_eq!(farm.call("GET", &at("spare"), "").await.status, 404);

    let again = farm.call("DELETE", &at("spare"), "").await;
    assert_eq!(
        (again.status, again.code().as_str()),
        (404, "no_such_connection")
    );
}

// Catches a registry failure reported as a client error or lost: a state store that fails
// answers 503 `state_unavailable` on reads and writes, a write that failed after its file was
// made leaves no file, and a row that does not decode answers 500 `corrupt_rows` (the text
// never carries the store's own message).
#[tokio::test]
async fn store_failures_are_503_and_a_corrupt_row_is_500() {
    use kbf_state::Store as _;
    use std::sync::atomic::Ordering::SeqCst;
    let farm = Farm::new();
    farm.flaky.fail_writes.store(true, SeqCst);
    let put = farm.call("PUT", &at("cold"), &conn_body(SENTINEL)).await;
    assert_eq!(
        (put.status, put.code().as_str()),
        (503, "state_unavailable"),
        "{}",
        put.body
    );
    assert!(!put.body.contains("injected"), "{}", put.body);
    assert!(
        farm.files().is_empty(),
        "the file of a write that did not commit is removed"
    );
    farm.flaky.fail_writes.store(false, SeqCst);

    farm.call("PUT", &at("cold"), &conn_body(SENTINEL)).await;
    farm.flaky.fail_reads.store(true, SeqCst);
    for path in [
        CONNS.to_owned(),
        at("cold"),
        "/v1/object-stores".to_owned(),
        "/v1/object-stores/audit".to_owned(),
    ] {
        let get = farm.call("GET", &path, "").await;
        assert_eq!(
            (get.status, get.code().as_str()),
            (503, "state_unavailable"),
            "{path}"
        );
    }
    farm.flaky.fail_reads.store(false, SeqCst);

    let txn = kbf_state::Txn::new().put(
        "objstore/conn/cold",
        vec![0xff, 0xff, 0xff],
        kbf_state::Cond::Any,
    );
    farm.store.txn(txn).expect("store").expect("no conflict");
    let broken = farm.call("GET", CONNS, "").await;
    assert_eq!(
        (broken.status, broken.code().as_str()),
        (500, "corrupt_rows"),
        "{}",
        broken.body
    );
}
