//! The operator's side: the routes called by the real `OperatorClient` on a real listener.

use std::net::SocketAddr;
use std::time::Duration;

use kbf_server::api_client::{ClientError, OperatorClient, Reply};
use kbf_types::object_store_api::{AuditList, Status};

use crate::farm::{BUCKET, Keys, PREFIX, TOKEN};

pub const BASE: &str = "/v1/object-stores";

/// An operator at the API of one frontend run.
#[derive(Clone, Copy)]
pub struct Operator(pub SocketAddr);

/// The body of `PUT .../connections/main`: `keys` for the store at `endpoint`. `verify`
/// false skips the frontend's own probe: an operator pushing a key the provider has not
/// enabled yet.
pub fn connection_body(endpoint: &str, keys: Keys, verify: bool) -> String {
    format!(
        r#"{{"endpoint":"{endpoint}","access_key_id":"{}","secret_access_key":"{}","conditional_put":true,"verify":{verify}}}"#,
        keys.id, keys.secret
    )
}

/// The body of `PUT .../bindings/cold`: the store every node's `--cold-store` names.
pub fn binding_body() -> String {
    format!(r#"{{"connection":"main","bucket":"{BUCKET}","prefix":"{PREFIX}"}}"#)
}

impl Operator {
    async fn call<F>(self, send: F) -> Reply
    where
        F: FnOnce(&OperatorClient, &str) -> Result<Reply, ClientError> + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let client = OperatorClient::new(self.0, Some(TOKEN.to_owned()));
            send(&client, BASE).expect("the call reached the frontend")
        })
        .await
        .expect("the client side")
    }

    /// `PUT /v1/object-stores/connections/main`.
    pub async fn put_connection(self, body: String) -> Reply {
        self.call(move |c, base| c.put(&format!("{base}/connections/main"), &body))
            .await
    }

    /// `PUT /v1/object-stores/bindings/cold`.
    pub async fn put_binding(self, body: String) -> Reply {
        self.call(move |c, base| c.put(&format!("{base}/bindings/cold"), &body))
            .await
    }

    /// Registers `main` with `keys` and binds `cold` to it; both answered 201.
    pub async fn register(self, endpoint: &str, keys: Keys) {
        let put = self
            .put_connection(connection_body(endpoint, keys, true))
            .await;
        assert_eq!(put.status, 201, "{}", put.body);
        let bound = self.put_binding(binding_body()).await;
        assert_eq!(bound.status, 201, "{}", bound.body);
    }

    /// `GET /v1/object-stores`.
    pub async fn status(self) -> Status {
        let reply = self.call(|c, base| c.get_gated(base)).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
    }

    /// `GET /v1/object-stores/audit`: the newest rows first.
    pub async fn audit(self) -> AuditList {
        let reply = self
            .call(|c, base| c.get_gated(&format!("{base}/audit?limit=50")))
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
    }

    /// The status, once `done` holds of it. A heartbeat is a real one: this waits for it.
    pub async fn status_when(self, what: &str, done: impl Fn(&Status) -> bool) -> Status {
        for _ in 0..2000 {
            let status = self.status().await;
            if done(&status) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let last = self.status().await;
        panic!("timed out waiting for {what}; the status is {last:?}");
    }
}
