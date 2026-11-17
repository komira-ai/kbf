//! `GET /v1/frontend/drain` and `GET /v1/frontend/retire`: what they count now that kbf holds
//! no queue and no worker sessions.

use std::sync::{Arc, OnceLock};

use axum::extract::State;
use futures::future::AbortHandle;
use kbf_front::{Backend, Cache, closing};

use super::*;
use crate::health::Readiness;
use crate::retire::Retire;
use crate::sequence::{DrainTiming, Sequence};
use crate::stand::Stand;
use crate::stop::Stop;

/// An API over a fresh in-memory front whose retire mode is set, and the front's backend.
fn api_with_retire() -> (Arc<ApiState>, Backend) {
    let backend = Backend::from(Arc::new(Cache::memory()));
    let (closer, _closing) = closing();
    let (drain, _goaway) = Drain::new(closer, Arc::new(Readiness::default()));
    let drain = Arc::new(drain);
    let (sweeper, _sweeping) = AbortHandle::new_pair();
    let stand = Stand {
        drain: Arc::clone(&drain),
        backend: backend.clone(),
        worker: Stop::new(),
        sweeper,
    };
    let sequence = Sequence::new(stand.clone(), Stop::new(), DrainTiming::default());
    let retire = Arc::new(OnceLock::new());
    let _ = retire.set(Retire::new(stand, sequence));
    let api = Arc::new(ApiState {
        backend: backend.clone(),
        token: None,
        drain,
        retire,
        members: None,
        store: None,
    });
    (api, backend)
}

async fn json_of(answer: Response) -> serde_json::Value {
    assert_eq!(answer.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    serde_json::from_slice(&bytes).expect("JSON")
}

/// Asserts that the drain status counts the Execute and WaitExecution streams open on the
/// front's store as they come and go, and that `queued_operations` is 0 whatever is open: kbf
/// queues nothing, and the field stays in the document for its readers.
#[tokio::test]
async fn the_drain_status_counts_open_executions_and_queues_nothing() {
    let (api, backend) = api_with_retire();
    let idle = json_of(drain_status(State(Arc::clone(&api))).await).await;
    assert_eq!(idle["open_executions"], 0, "{idle}");
    assert_eq!(idle["queued_operations"], 0, "{idle}");
    assert_eq!(idle["draining"], false, "{idle}");

    let _one = backend.metrics().execute.open_executions.raise();
    let _two = backend.metrics().execute.open_executions.raise();
    let busy = json_of(drain_status(State(api)).await).await;
    assert_eq!(busy["open_executions"], 2, "{busy}");
    assert_eq!(busy["queued_operations"], 0, "{busy}");
}

/// Asserts that the retire status counts the open streams of the front's store and reports
/// `open_sessions` as 0 (kbf holds no worker sessions), before and after the retire has run,
/// and that `retired` turns true only once it has.
#[tokio::test]
async fn the_retire_status_has_no_sessions_and_counts_open_streams() {
    let (api, backend) = api_with_retire();
    let before = json_of(retire::status(State(Arc::clone(&api))).await).await;
    assert_eq!(
        before,
        serde_json::json!({"retired": false, "open_streams": 0, "open_sessions": 0})
    );

    let open = backend.metrics().execute.open_executions.raise();
    let retire = api.retire.get().expect("a retire mode");
    assert!(retire.begin().await, "the first call retired it");
    assert!(!retire.begin().await, "a second call changes nothing");
    let after = json_of(retire::status(State(Arc::clone(&api))).await).await;
    assert_eq!(
        after,
        serde_json::json!({"retired": true, "open_streams": 1, "open_sessions": 0})
    );
    drop(open);
    let drained = json_of(retire::status(State(api)).await).await;
    assert_eq!(drained["open_streams"], 0);
    assert_eq!(drained["open_sessions"], 0);
}

/// Asserts that a server with no retire mode (no state store) answers the retire status 409,
/// as it did before.
#[tokio::test]
async fn the_retire_status_of_a_server_with_no_state_store_is_a_conflict() {
    let (api, _backend) = api_with_retire();
    let bare = Arc::new(ApiState {
        backend: api.backend.clone(),
        token: None,
        drain: Arc::clone(&api.drain),
        retire: Arc::default(),
        members: None,
        store: None,
    });
    let answer = retire::status(State(bare)).await;
    assert_eq!(answer.status(), StatusCode::CONFLICT);
}

/// Asserts that `POST /v1/frontend/retire` retires the frontend once and answers a second call
/// with the same retired status, the retire not run again.
#[tokio::test]
async fn a_second_retire_through_the_route_answers_the_same_status() {
    let (api, _backend) = api_with_retire();
    let api = Arc::new(ApiState {
        backend: api.backend.clone(),
        token: Some(ApiToken::for_tests("retire-token-0123456789abcdef0123")),
        drain: Arc::clone(&api.drain),
        retire: Arc::clone(&api.retire),
        members: None,
        store: None,
    });
    let peer = Caller::direct(std::net::SocketAddr::from(([127, 0, 0, 1], 9)));
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, JSON.parse().expect("a header value"));
    let bearer = "Bearer retire-token-0123456789abcdef0123";
    headers.insert(AUTHORIZATION, bearer.parse().expect("a header value"));
    let call = || {
        retire::begin(
            State(Arc::clone(&api)),
            peer.clone(),
            headers.clone(),
            Bytes::new(),
        )
    };
    let first = json_of(call().await).await;
    let second = json_of(call().await).await;
    assert_eq!(first["retired"], true, "{first}");
    assert_eq!(second, first);
}
