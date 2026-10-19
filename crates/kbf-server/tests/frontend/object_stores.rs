//! The cold-store registration through a real `kbf frontend` (`start_frontend`, the path the
//! binary takes): the operator registers a store on the API listener, a storage node under
//! mutual TLS on the worker listener learns the generation from `Membership`, fetches the
//! store from `ColdStore`, reports it in its next heartbeat, and the operator's status shows
//! the farm converged. `object_store_api/` and `store_config.rs` test the pieces with seams
//! of their own; this is where the frontend joins them.

use std::os::unix::fs::PermissionsExt as _;

use kbf_proto::kbf::enroll::v1::GetStoreConfigRequest;
use kbf_proto::kbf::enroll::v1::cold_store_client::ColdStoreClient;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_server::api_client::OperatorClient;
use kbf_types::object_store_api::{MemberStanding, NodeStatus, Status};
use tonic::Code;

use super::membership_tls::{beat, register, storage_channel, tls_flags};
use super::*;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const SECRET: &str = "frontend-test-secret-key";

// Catches a frontend that serves the operator routes and nothing of the registration to the
// nodes: a registered store has to reach a storage node and the node's report has to reach
// the operator. In order, over the started frontend:
//
// - before anything is registered, `Register` answers generation 0 and `GetStoreConfig`
//   answers generation 0 with no store;
// - the operator's connection and binding make generation 2, and the node's next heartbeat
//   announces it (the `Membership` answers carry the registry's generation), the node
//   saying `pending`;
// - `GetStoreConfig` over the node's storage certificate returns the store, its bucket and
//   prefix and the secret read from the 0600 file, stamped generation 2; asked again with
//   `have_generation` 2 it returns the bare generation and no secret;
// - until the node reports, the status shows it pending and the farm not converged; after
//   the heartbeat that says generation 2 `ok`, the status shows one expected, one applied,
//   converged, the node named by the address it registered and standing `active`.
//
// Mutants seen red: the API's acks left as `NoAcks` (the status never converges); the
// `ColdStore` service not added to the worker listener (the fetch is UNIMPLEMENTED);
// `Membership` built without the delivery (every answer carries generation 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registered_store_reaches_a_storage_node_and_its_report_reaches_the_operator() {
    let pki = Pki::new("object-stores");
    let dir = scratch_dir("frontend-object-stores-delivery");
    let token_file = dir.join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).expect("token");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let secrets = dir.join("secrets");
    let front = Front::start(&tls_flags(
        &pki,
        &dir.join("state.db"),
        &[
            format!("--api-token-file={}", token_file.display()),
            format!("--object-store-secrets-dir={}", secrets.display()),
        ],
    ))
    .await;

    let channel = storage_channel(&pki, front.worker, &["node-a"]).await;
    let mut membership = MembershipClient::new(channel.clone());
    let mut cold = ColdStoreClient::new(channel);
    let joined = membership
        .register(register("node-a:8982", "node-a:8982"))
        .await
        .expect("the node registers")
        .into_inner();
    assert_eq!(joined.objstore_generation, 0, "nothing is registered yet");
    let nothing = cold
        .get_store_config(GetStoreConfigRequest { have_generation: 0 })
        .await
        .expect("the node may ask")
        .into_inner();
    assert_eq!((nothing.generation, nothing.config.is_none()), (0, true));

    let api = front.api;
    let operator = move |send: fn(&OperatorClient) -> Reply| {
        tokio::task::spawn_blocking(move || send(&OperatorClient::new(api, Some(TOKEN.to_owned()))))
    };
    let connection = operator(|c| {
        c.put(
            "/v1/object-stores/connections/main",
            &format!(
                r#"{{"endpoint":"http://s3.example:9000","access_key_id":"AKID","secret_access_key":"{SECRET}","verify":false}}"#
            ),
        )
        .expect("put")
    })
    .await
    .expect("client");
    assert_eq!(connection.status, 201, "{}", connection.body);
    let binding = operator(|c| {
        c.put(
            "/v1/object-stores/bindings/cold",
            r#"{"connection":"main","bucket":"farm-bucket","prefix":"cold/","verify":false}"#,
        )
        .expect("put")
    })
    .await
    .expect("client");
    assert_eq!(binding.status, 201, "{}", binding.body);

    let mut pending = beat(&joined, joined.boot);
    pending.apply_status = pb::ApplyStatus::Pending.into();
    let beaten = membership
        .heartbeat(pending)
        .await
        .expect("the node beats")
        .into_inner();
    assert_eq!(
        beaten.objstore_generation, 2,
        "the answer announces the registration"
    );

    let fetched = cold
        .get_store_config(GetStoreConfigRequest { have_generation: 0 })
        .await
        .expect("the node fetches")
        .into_inner();
    assert_eq!(fetched.generation, 2);
    let config = fetched.config.expect("the store");
    assert_eq!(
        (
            config.endpoint.as_str(),
            config.bucket.as_str(),
            config.prefix.as_str(),
            config.access_key_id.as_str(),
            config.secret_access_key.as_str(),
            config.secret_version,
        ),
        (
            "http://s3.example:9000",
            "farm-bucket",
            "cold/",
            "AKID",
            SECRET,
            2
        )
    );
    let again = cold
        .get_store_config(GetStoreConfigRequest { have_generation: 2 })
        .await
        .expect("the node asks again")
        .into_inner();
    assert_eq!((again.generation, again.config.is_none()), (2, true));

    let status = status_of(&operator).await;
    assert_eq!(status.generation, 2);
    assert!(!status.convergence.converged, "the node has not reported");
    assert_eq!(status.convergence.nodes[0].status, NodeStatus::Pending);

    let mut report = beat(&joined, joined.boot);
    report.applied_objstore_generation = 2;
    report.apply_status = pb::ApplyStatus::Ok.into();
    membership
        .heartbeat(report)
        .await
        .expect("the node reports");
    let status = status_of(&operator).await;
    let convergence = status.convergence;
    assert_eq!(
        (
            convergence.converged,
            convergence.expected,
            convergence.applied
        ),
        (true, 1, 1),
        "{convergence:?}"
    );
    let node = &convergence.nodes[0];
    assert_eq!(
        (
            node.id.as_str(),
            node.state,
            node.status,
            node.applied_generation
        ),
        (
            "node-a:8982",
            MemberStanding::Active,
            NodeStatus::Ok,
            Some(2)
        )
    );
}

// Catches a `ColdStore` mounted on the worker listener that serves a caller who is not a
// storage node: a plain-text listener (no worker TLS) must refuse the one RPC that carries
// the secret, with PERMISSION_DENIED, and still serve the operator routes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_text_worker_listener_refuses_the_store_fetch() {
    let dir = scratch_dir("frontend-object-stores-plain");
    let token_file = dir.join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).expect("token");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let front = Front::start(
        &flags(&[
            "--storage=http://127.0.0.1:1",
            &format!("--api-token-file={}", token_file.display()),
            &format!(
                "--object-store-secrets-dir={}",
                dir.join("secrets").display()
            ),
            &format!("--state-store=sqlite://{}", dir.join("state.db").display()),
        ])
        .expect("parses"),
    )
    .await;
    let channel = Endpoint::from_shared(format!("http://{}", front.worker))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let refused = ColdStoreClient::new(channel)
        .get_store_config(GetStoreConfigRequest { have_generation: 0 })
        .await
        .expect_err("plain text is refused");
    assert_eq!(refused.code(), Code::PermissionDenied, "{refused}");
}

type Reply = kbf_server::api_client::Reply;

/// `GET /v1/object-stores` as the operator, once a heartbeat's ack has landed.
async fn status_of(
    operator: &impl Fn(fn(&OperatorClient) -> Reply) -> tokio::task::JoinHandle<Reply>,
) -> Status {
    let reply = operator(|c| c.get_gated("/v1/object-stores").expect("get"))
        .await
        .expect("client");
    assert_eq!(reply.status, 200, "{}", reply.body);
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
}

// Catches a `ColdStore` mounted without the frontend's two deny sources, which would hand a
// revoked node the registration's secret: node A is named by the worker listener's deny list
// file and node B by the operator's `POST /v1/denied` row, and each fetch is PERMISSION_DENIED
// naming the entry, while node C, listed nowhere, is served.
//
// Mutants seen red: `ColdStore` built with no deny list file; built without the `deny/` rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_store_fetch_is_refused_to_denied_nodes() {
    let pki = Pki::new("object-stores-deny");
    let dir = scratch_dir("frontend-object-stores-deny");
    let token_file = dir.join("token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).expect("token");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let list = dir.join("deny.list");
    std::fs::write(&list, "node node-a\n").expect("list");
    let front = Front::start(&tls_flags(
        &pki,
        &dir.join("state.db"),
        &[
            format!("--api-token-file={}", token_file.display()),
            format!(
                "--object-store-secrets-dir={}",
                dir.join("secrets").display()
            ),
            format!("--worker-deny-list={}", list.display()),
        ],
    ))
    .await;
    let api = front.api;
    let denied = tokio::task::spawn_blocking(move || {
        OperatorClient::new(api, Some(TOKEN.to_owned()))
            .post(
                "/v1/denied",
                r#"{"entry":"node node-b","reason":"retired"}"#,
            )
            .expect("post")
    })
    .await
    .expect("client");
    assert_eq!(denied.status, 200, "{}", denied.body);

    for (node, listed) in [("node-a", true), ("node-b", true), ("node-c", false)] {
        let channel = storage_channel(&pki, front.worker, &[node]).await;
        let fetched = ColdStoreClient::new(channel)
            .get_store_config(GetStoreConfigRequest { have_generation: 0 })
            .await;
        if listed {
            let refused = fetched.expect_err(node);
            assert_eq!(refused.code(), Code::PermissionDenied, "{node}: {refused}");
        } else {
            fetched.expect("an unlisted node is served");
        }
    }
}
