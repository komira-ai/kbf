//! `GET /v1/denied` (`docs/api.md`): the storage deny list with the reason and time of each
//! entry, as the state store's rows hold it now ([`Members::denials`]).
//!
//! It passes every gate of a write ([`crate::api::gate`]), the `GET` included: the reasons of
//! a deny list are for the operator. The answer is `Cache-Control: no-store`. The read goes
//! through the [`kbf_state::Store`] trait and assumes no single writer: any frontend over the
//! store answers from the same rows.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use kbf_state::StoreError;
use kbf_types::operator_api::{DeniedEntry, DeniedList};

use crate::api::{DENIED, error, gate, json, no_members};
use crate::members::{Members, MembershipError};
use crate::token::ApiToken;

/// What the handler shares: the token the gate checks (`None`: writes are off) and the
/// storage members (`None`: no state store).
struct Verbs {
    token: Option<ApiToken>,
    members: Option<Arc<Members>>,
}

/// The route. `token` is the operator API token the gate checks. Serve it with
/// `into_make_service_with_connect_info`, as [`crate::api::router`] is; the router merges
/// with it, the `GET` joining the `POST` and `DELETE` of the same path.
pub fn routes(token: Option<ApiToken>, members: Option<Arc<Members>>) -> Router {
    Router::new()
        .route(DENIED, get(list_denied))
        .with_state(Arc::new(Verbs { token, members }))
}

/// `answer` that no cache keeps.
fn no_store(mut answer: Response) -> Response {
    answer
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    answer
}

/// The status and fixed text a failed store read answers with: never a stored row or the
/// store's own message.
fn store_failure(e: &MembershipError) -> (StatusCode, &'static str) {
    match e {
        MembershipError::Store(StoreError::Fenced | StoreError::Failed(_))
        | MembershipError::Contended => (StatusCode::SERVICE_UNAVAILABLE, "the state store failed"),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "a stored row is unreadable",
        ),
    }
}

async fn list_denied(
    State(verbs): State<Arc<Verbs>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err((code, why)) = gate(verbs.token.as_ref(), peer, &headers) {
        return no_store(error(code, &why));
    }
    let Some(members) = verbs.members.clone() else {
        return no_store(no_members());
    };
    // The store's calls block (its trait says so).
    let listed = tokio::task::spawn_blocking(move || members.denials()).await;
    match listed {
        Ok(Ok(denials)) => no_store(json(
            StatusCode::OK,
            &DeniedList {
                denied: denials
                    .into_iter()
                    .map(|d| DeniedEntry {
                        entry: d.entry.to_string(),
                        reason: d.reason,
                        at_ms: d.at_ms,
                    })
                    .collect(),
            },
        )),
        Ok(Err(e)) => {
            let (code, why) = store_failure(&e);
            no_store(error(code, why))
        }
        Err(_) => no_store(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the listing task failed",
        )),
    }
}
