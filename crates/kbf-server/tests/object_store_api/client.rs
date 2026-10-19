//! The routes over a real listener, called by the real client: paths, methods and the gate as
//! `kbf` meets them. (Behavior is pinned through the router in the other modules.)

use std::net::SocketAddr;
use std::sync::Arc;

use kbf_server::api_client::OperatorClient;
use kbf_types::object_store_api::{
    AuditList, BindingPutAnswer, ConnectionList, ConnectionPutAnswer, Deleted, Status, VerifyAnswer,
};

use super::harness::{BIND_BODY, Farm, SENTINEL, TOKEN, conn_body};

async fn serve(farm: &Farm) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = farm.router.clone();
    tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.expect("serve");
    });
    addr
}

// Catches a route mounted at another path or method than the client calls, a client whose
// PUT or GET lacks what the gate asks of every call, and an answer a client cannot read:
// the whole registration round trip works through the real client, and the plain GET (no
// content type) is refused on the reads.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_reaches_every_route_through_the_gate() {
    let farm = Arc::new(Farm::new());
    let addr = serve(&farm).await;
    let held = Arc::clone(&farm);
    tokio::task::spawn_blocking(move || {
        let client = OperatorClient::new(addr, Some(TOKEN.to_owned()));
        let base = "/v1/object-stores";

        let put = client
            .put(&format!("{base}/connections/cold"), &conn_body(SENTINEL))
            .expect("put");
        assert_eq!(put.status, 201, "{}", put.body);
        assert_eq!(
            serde_json::from_str::<ConnectionPutAnswer>(&put.body)
                .expect("answer")
                .generation,
            1
        );
        let bind = client
            .put(&format!("{base}/bindings/cold"), BIND_BODY)
            .expect("bind");
        assert_eq!(bind.status, 201, "{}", bind.body);
        assert_eq!(
            serde_json::from_str::<BindingPutAnswer>(&bind.body)
                .expect("answer")
                .generation,
            2
        );

        let list = client
            .get_gated(&format!("{base}/connections"))
            .expect("list");
        assert_eq!(
            serde_json::from_str::<ConnectionList>(&list.body)
                .expect("list")
                .connections
                .len(),
            1
        );
        let one = client
            .get_gated(&format!("{base}/connections/cold"))
            .expect("one");
        assert_eq!(one.status, 200);
        let status = client.get_gated(base).expect("status");
        assert_eq!(
            serde_json::from_str::<Status>(&status.body)
                .expect("status")
                .generation,
            2
        );
        let audit = client
            .get_gated(&format!("{base}/audit?limit=5"))
            .expect("audit");
        assert_eq!(
            serde_json::from_str::<AuditList>(&audit.body)
                .expect("audit")
                .entries
                .len(),
            2
        );
        let verify = client
            .post(&format!("{base}/connections/cold:verify"), "")
            .expect("verify");
        assert_eq!(verify.status, 200, "{}", verify.body);
        assert!(
            serde_json::from_str::<VerifyAnswer>(&verify.body)
                .expect("verify")
                .verified
                .ok
        );

        for path in [
            base.to_owned(),
            format!("{base}/connections"),
            format!("{base}/audit"),
        ] {
            assert_eq!(client.get(&path).expect("plain get").status, 415, "{path}");
        }

        let unbound = client
            .delete(&format!("{base}/bindings/cold"))
            .expect("unbind");
        assert_eq!(unbound.status, 200, "{}", unbound.body);
        assert_eq!(
            serde_json::from_str::<Deleted>(&unbound.body)
                .expect("deleted")
                .generation,
            3
        );
        let removed = client
            .delete(&format!("{base}/connections/cold"))
            .expect("delete");
        assert_eq!(removed.status, 200, "{}", removed.body);

        let anonymous = OperatorClient::new(addr, None);
        assert_eq!(anonymous.get_gated(base).expect("get").status, 401);
        assert_eq!(
            anonymous
                .put(&format!("{base}/connections/cold"), "{}")
                .expect("put")
                .status,
            401
        );
    })
    .await
    .expect("the client side");
    assert_eq!(held.generation(), 4);
}
