//! `POST /v1/epoch/bump` in one process: the operator's raise of the epoch the state store
//! holds, behind the write gates, as `kbf cluster epoch-bump` sends it.

mod support;

use std::sync::Arc;

use kbf_server::api::EPOCH_BUMP;
use kbf_server::api_client::{OperatorClient, Reply};
use kbf_server::{EpochPolicy, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use serde_json::{Value, json};
use support::{API_TOKEN, Cell};

/// A call to the cell's operator API, off the async threads: the client blocks.
async fn bump(cell: &Cell, token: bool, body: &'static str) -> Reply {
    let api = cell.api.expect("a cell with an operator API");
    let token = token.then(|| API_TOKEN.to_owned());
    tokio::task::spawn_blocking(move || {
        let client = OperatorClient::new(api, token);
        client.post(EPOCH_BUMP, body).expect("a reply")
    })
    .await
    .expect("the call runs")
}

fn new_store() -> Arc<Fake> {
    let clock = Arc::new(ManualClock::default());
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

fn state_over(store: &Arc<Fake>) -> StateStore {
    StateStore {
        store: store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }
}

fn stored_epoch(store: &Fake) -> u64 {
    let row = store.get("meta/epoch").expect("read").expect("an epoch");
    u64::from_be_bytes(row.value.as_slice().try_into().expect("eight bytes"))
}

fn answer(reply: &Reply) -> Value {
    assert_eq!(reply.status, 200, "{reply:?}");
    serde_json::from_str(&reply.body).expect("JSON")
}

/// Asserts that a bump raises the stored epoch by one each time, that the answer names the
/// epoch before, the epoch stored now and the epoch the frontend serves (which is the one
/// it started under, however often the stored one is raised), and that an empty body and
/// `{}` are both accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bump_raises_the_stored_epoch_and_names_the_one_served() {
    let store = new_store();
    let cell = Cell::start_with_api_and_state(state_over(&store)).await;
    let served = stored_epoch(&store);

    let first = bump(&cell, true, "").await;
    let want = json!({"previous": served, "epoch": served + 1, "serving": served});
    assert_eq!(answer(&first), want);
    assert_eq!(stored_epoch(&store), served + 1);

    let second = bump(&cell, true, "{}").await;
    let want = json!({"previous": served + 1, "epoch": served + 2, "serving": served});
    assert_eq!(answer(&second), want);
    assert_eq!(stored_epoch(&store), served + 2);
}

/// Asserts that a bump the gates refuse (no token) or that carries a body changes
/// nothing: the stored epoch stays as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_bump_leaves_the_stored_epoch_alone() {
    let store = new_store();
    let cell = Cell::start_with_api_and_state(state_over(&store)).await;
    let served = stored_epoch(&store);

    let no_token = bump(&cell, false, "").await;
    assert_eq!(no_token.status, 401, "{no_token:?}");
    let with_body = bump(&cell, true, r#"{"by": 5}"#).await;
    assert_eq!(with_body.status, 400, "{with_body:?}");

    assert_eq!(stored_epoch(&store), served);
}

/// Asserts that a server with no state store answers a bump that passes the gates with a
/// 404 that says why: it keeps no epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_no_state_store_has_no_epoch_to_raise() {
    let cell = Cell::start_with_api().await;

    let reply = bump(&cell, true, "").await;

    assert_eq!(reply.status, 404, "{reply:?}");
    assert!(reply.body.contains("no state store"), "{reply:?}");
}
