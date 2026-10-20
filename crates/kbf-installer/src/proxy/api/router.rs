//! The routes of the update API as a function from a request to a response, with no
//! socket: [`Router::call`]. [`handler::Handler`](super::handler::Handler) puts it behind a
//! listener.
//!
//! ```text
//! POST /v1/update/frontend                 {commit, request_id, nodes, rollback}
//! GET  /v1/update/frontend
//! POST /v1/update/frontend/abort | rollback | force-retire
//! any  /v1/update/desired | /pause | /resume, GET /v1/update   forwarded to the operator port
//! ```
//!
//! Every route is authorized first ([`Authorize`]); a path the router does not know is a
//! 404 before any credential is looked at, since the path space is public anyway. The
//! body is read by the caller (bounded by [`MAX_BODY`] here too).

use std::sync::Arc;

use bytes::Bytes;
use hyper::header::{ALLOW, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE};
use hyper::{Method, Request, Response, StatusCode};
use serde::Serialize;
use serde_json::json;

use super::actor::Api;
use super::auth::{Authorize, Denied};
use super::core::{ApiError, Reply};
use super::model::{ForceRetireRequest, UpdateRequest};
use super::traits::Forwarder;

/// The largest request body accepted.
pub const MAX_BODY: usize = 8 * 1024;

pub struct Router {
    auth: Arc<dyn Authorize>,
    api: Api,
    forwarder: Arc<dyn Forwarder>,
}

enum Route {
    Update,
    Abort,
    Rollback,
    ForceRetire,
    Forward,
}

fn route(path: &str, method: &Method) -> Option<Route> {
    match path {
        "/v1/update/frontend" => Some(Route::Update),
        "/v1/update/frontend/abort" => Some(Route::Abort),
        "/v1/update/frontend/rollback" => Some(Route::Rollback),
        "/v1/update/frontend/force-retire" => Some(Route::ForceRetire),
        "/v1/update/desired" | "/v1/update/pause" | "/v1/update/resume" => Some(Route::Forward),
        "/v1/update" if method == Method::GET => Some(Route::Forward),
        _ => None,
    }
}

impl Router {
    pub fn new(auth: Arc<dyn Authorize>, api: Api, forwarder: Arc<dyn Forwarder>) -> Router {
        Router {
            auth,
            api,
            forwarder,
        }
    }

    pub async fn call(&self, req: Request<Bytes>) -> Response<Bytes> {
        let Some(found) = route(req.uri().path(), req.method()) else {
            return error(&ApiError {
                status: StatusCode::NOT_FOUND,
                code: "not_found",
                message: "no such route".to_owned(),
                extra: None,
            });
        };
        if let Err(denied) = self.auth.authorize(&req) {
            return denied_response(denied);
        }
        if req.body().len() > MAX_BODY {
            return error(&ApiError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                code: "body_too_large",
                message: "the request body is too large".to_owned(),
                extra: None,
            });
        }
        match (found, req.method().clone()) {
            (Route::Forward, _) => self.forward(req).await,
            (Route::Update, Method::POST) => match parse::<UpdateRequest>(req.body()) {
                Ok(body) => self.reply(self.api.post(body).await, StatusCode::ACCEPTED),
                Err(e) => error(&e),
            },
            (Route::Update, Method::GET) => self.reply(self.api.status().await, StatusCode::OK),
            (Route::Abort, Method::POST) => self.reply(self.api.abort().await, StatusCode::OK),
            (Route::Rollback, Method::POST) => {
                self.reply(self.api.rollback().await, StatusCode::OK)
            }
            (Route::ForceRetire, Method::POST) => match parse::<ForceRetireRequest>(req.body()) {
                Ok(body) => self.reply(self.api.force_retire(body.reason).await, StatusCode::OK),
                Err(e) => error(&e),
            },
            (Route::Update, _) => not_allowed("GET, POST"),
            _ => not_allowed("POST"),
        }
    }

    async fn forward(&self, req: Request<Bytes>) -> Response<Bytes> {
        match self.forwarder.forward(req).await {
            Ok(resp) => resp,
            Err(e) => error(&ApiError {
                status: StatusCode::BAD_GATEWAY,
                code: "operator_port_unreachable",
                message: e.to_string(),
                extra: None,
            }),
        }
    }

    fn reply(&self, result: Result<Reply, ApiError>, ok: StatusCode) -> Response<Bytes> {
        match result {
            Ok(Reply::Update { update_id, phase }) => {
                json_response(ok, &json!({"update_id": update_id, "phase": phase}))
            }
            Ok(Reply::Status(body)) => json_response(ok, &*body),
            Ok(Reply::Retired {
                update_id,
                dropped_streams,
            }) => json_response(
                ok,
                &json!({"update_id": update_id, "dropped_streams": dropped_streams}),
            ),
            Err(e) => error(&e),
        }
    }
}

fn bad(code: &'static str, message: &str) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        code,
        message: message.to_owned(),
        extra: None,
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| bad("bad_request", &e.to_string()))
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Bytes> {
    // The bodies are plain structs and maps: serialization cannot fail.
    let mut bytes = serde_json::to_vec(body).unwrap_or_default();
    bytes.push(b'\n');
    let mut resp = Response::new(Bytes::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        "application/json".parse().expect("static value"),
    );
    resp
}

pub(super) fn error(e: &ApiError) -> Response<Bytes> {
    let mut body = json!({"error": e.code, "message": e.message});
    if let Some((key, value)) = &e.extra {
        body[*key] = json!(value);
    }
    json_response(e.status, &body)
}

fn not_allowed(allow: &'static str) -> Response<Bytes> {
    let mut resp = error(&ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        code: "method_not_allowed",
        message: format!("allowed: {allow}"),
        extra: None,
    });
    resp.headers_mut()
        .insert(ALLOW, allow.parse().expect("static value"));
    resp
}

fn denied_response(denied: Denied) -> Response<Bytes> {
    let message = match denied {
        Denied::RateLimited { .. } => "too many requests",
        Denied::MissingCredentials | Denied::BadScheme | Denied::BadToken => {
            "a valid operator token is required"
        }
        Denied::TokenInQuery => "send the token in the Authorization header, never in the URL",
        Denied::Malformed => "malformed credentials",
        Denied::TokenUnavailable => "the server has no operator token configured",
    };
    let mut resp = error(&ApiError {
        status: denied.status(),
        code: denied.code(),
        message: message.to_owned(),
        extra: None,
    });
    match denied {
        Denied::RateLimited { retry_after_s } => {
            resp.headers_mut().insert(RETRY_AFTER, retry_after_s.into());
        }
        Denied::MissingCredentials | Denied::BadScheme | Denied::BadToken => {
            resp.headers_mut()
                .insert(WWW_AUTHENTICATE, "Bearer".parse().expect("static value"));
        }
        _ => {}
    }
    resp
}
