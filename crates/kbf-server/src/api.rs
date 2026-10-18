//! The operator API: HTTP/JSON under `/v1`, on its own listener (`--api-listen`), for
//! the Fleet UI, scripts and clients (`docs/design/fleet-updates.md` section 11.4;
//! `docs/api.md`).
//!
//! - `GET /v1/nodes`: every node registered since the server started, with the newest
//!   software status each sent and where it is in placement ([`crate::fleet`]).
//! - `GET /v1/members`: the storage members of the current map, as the gRPC
//!   `Membership` service keeps them ([`crate::members::Members`]).
//! - `GET /v1/storage`: whether every blob is known to have its copies elsewhere, from the
//!   storage members' heartbeats ([`crate::storage_health`]); on a frontend with a state
//!   store.
//! - `POST /v1/nodes/{node}:cordon`, `:drain` and `:uncordon`: take a node out of
//!   placement, drain it, or return it ([`NodeAction`]). A drain's body may be
//!   `{"deadline_secs": N}`; without one the deadline is [`DRAIN_DEADLINE`]. The answer
//!   is the node as it is after the action.
//!
//! - `POST /v1/members/{id}` with `{"state": "active"|"draining"}`: sets a storage
//!   member's state ([`Members::set_state`]), the operator's promotion of a joiner.
//!
//! - `POST /v1/denied` with `{"entry": "node X"|"serial S"|"spki-sha256 H", "reason": "..."}`
//!   and `DELETE /v1/denied?entry=...`: add an entry to the storage deny list, or take it
//!   off ([`Members::deny`], [`Members::allow`]). Each runs a refresh before it answers, so
//!   the answer carries the generation of the map that holds the change. A `node` entry
//!   is a DNS name, written in lower case (a certificate's names are compared up to ASCII
//!   case); one that is an IP address or has a colon is `400`, since it would name nothing.
//!
//! - `GET /v1/frontend/drain`: whether the frontend is draining, the Execute and
//!   WaitExecution streams open and the operations queued ([`DrainStatus`]).
//!   `POST /v1/frontend/drain` begins the drain ([`crate::drain`]) and answers the same
//!   status: new Execute calls are refused, `/readyz` goes false, the REAPI listener is
//!   sent GOAWAY, open streams run until their action is done, and the server exits
//!   when they have. It is not undone: a drained frontend is restarted.
//!
//! - `/v1/enroll/...`: the enrollment tokens and the node registry ([`crate::enroll::api`]).
//!   Every one of these routes, a GET included, passes the gates below.
//!
//! Reads answer anyone who reaches the listener, except the enrollment routes. A write
//! must pass every gate of [`write_request`], because a loopback peer is not an operator:
//! every server host is also a worker whose build actions can reach loopback, a reverse proxy on the host
//! makes remote callers loopback, and a browser on the host can POST to it from any
//! page. The gates: a loopback peer, no `Origin` header (no browser page), the bearer
//! token of `--api-token-file` ([`ApiToken`]; without one, writes are off), and a JSON
//! content type. The roles of `fleet-updates-security.md` section S9 are planned.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, RawQuery, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kbf_map::{DenyEntry, Map, MemberId, MemberState};
use kbf_types::WorkerId;
use serde::{Deserialize, Serialize};

use crate::drain::{Drain, DrainStatus};
use crate::enroll::api::EnrollApi;
use crate::farm::{Farm, NodeAction};
use crate::members::{Members, MembershipError};
use crate::token::ApiToken;

/// The media type of every body this API returns, and of every write it accepts.
pub const JSON: &str = "application/json";

/// How long a drain waits for leases when its request names no deadline
/// (`drain_deadline` in `fleet-updates.md` section 4.1).
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// The path of the frontend's drain: `GET` reads it, `POST` begins it.
pub const FRONTEND_DRAIN: &str = "/v1/frontend/drain";

/// The path of `GET /v1/nodes`.
pub const NODES: &str = "/v1/nodes";

/// The path of `GET /v1/members`.
pub const MEMBERS: &str = "/v1/members";

/// The path of `POST /v1/denied` (add an entry) and `DELETE /v1/denied?entry=...`.
pub const DENIED: &str = "/v1/denied";

/// The path of `POST /v1/nodes/{node}:{verb}`, with `node` percent-encoded (every byte
/// but an ASCII letter, digit, `-`, `.`, `_` or `~`), so a node id that holds a `/`, a
/// `:` or a space still names one path segment.
#[must_use]
pub fn node_path(node: &str, verb: &str) -> String {
    let mut path = format!("{NODES}/");
    for byte in node.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            path.push(char::from(byte));
        } else {
            path.push_str(&format!("%{byte:02X}"));
        }
    }
    path.push(':');
    path.push_str(verb);
    path
}

/// What the handlers share: the farm, the token writes must present (`None`: writes
/// are off), the frontend's drain and the storage members (`None`: this server keeps
/// none, as without a state store).
struct ApiState {
    farm: Arc<Farm>,
    token: Option<ApiToken>,
    drain: Arc<Drain>,
    members: Option<Arc<Members>>,
}

/// The API's routes over `farm`; writes need `token`, and without one are refused.
/// `drain` is what `POST /v1/frontend/drain` begins; `members` is what
/// `GET /v1/members` reads and `POST /v1/members/{id}` changes (the same value the
/// `Membership` service registers into). `enroll` is what the `/v1/enroll/...` routes
/// call ([`crate::enroll::api`]); `None` when this server has no enrollment, which those
/// routes answer, behind the gate, with 404.
/// Writes read the peer address from
/// [`ConnectInfo`]: serve them with `into_make_service_with_connect_info`.
pub fn router(
    farm: Arc<Farm>,
    token: Option<ApiToken>,
    drain: Arc<Drain>,
    members: Option<Arc<Members>>,
    enroll: Option<Arc<EnrollApi>>,
) -> Router {
    let enrollment = crate::enroll::api::routes(token.clone(), enroll);
    Router::new()
        .route(NODES, get(nodes))
        .route(MEMBERS, get(members_view))
        .route("/v1/nodes/{target}", post(act))
        .route("/v1/members/{id}", post(set_member_state))
        .route(DENIED, post(deny_entry).delete(allow_entry))
        .route(FRONTEND_DRAIN, get(drain_status).post(begin_drain))
        .with_state(Arc::new(ApiState {
            farm,
            token,
            drain,
            members,
        }))
        .merge(enrollment)
}

/// The drain as it is now.
fn status(api: &ApiState) -> DrainStatus {
    DrainStatus {
        draining: api.drain.is_draining(),
        open_executions: api.farm.open_executions(),
        queued_operations: api.farm.queued_operations(),
    }
}

async fn drain_status(State(api): State<Arc<ApiState>>) -> Response {
    json(StatusCode::OK, &status(&api))
}

/// The body a frontend drain may carry: none, or `{}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NoBody {}

async fn begin_drain(
    State(api): State<Arc<ApiState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), peer, &headers) {
        return error(code, &why);
    }
    if !body.is_empty()
        && let Err(e) = serde_json::from_slice::<NoBody>(&body)
    {
        let why = format!("a frontend drain takes no body, or {{}}: {e}");
        return error(StatusCode::BAD_REQUEST, &why);
    }
    if api.drain.begin() {
        tracing::info!("frontend drain begun by the operator API");
    }
    json(StatusCode::OK, &status(&api))
}

async fn nodes(State(api): State<Arc<ApiState>>) -> Response {
    json(StatusCode::OK, &api.farm.nodes())
}

/// One storage member of the map.
#[derive(Debug, Serialize)]
struct MemberView {
    id: u64,
    addr: String,
    state: &'static str,
    weight: u32,
}

/// The storage members of the current map.
#[derive(Debug, Serialize)]
struct MembersView {
    generation: u64,
    members: Vec<MemberView>,
}

/// A member's state as `GET /v1/members` names it. Exhaustive: a new state fails to
/// compile here instead of reaching operators unnamed.
const fn state_name(state: MemberState) -> &'static str {
    match state {
        MemberState::Joining => "joining",
        MemberState::CatchingUp => "catching_up",
        MemberState::Active => "active",
        MemberState::Draining => "draining",
        MemberState::Down => "down",
    }
}

async fn members_view(State(api): State<Arc<ApiState>>) -> Response {
    let Some(members) = api.members.clone() else {
        return no_members();
    };
    // The store's calls block (its trait says so).
    let map = match tokio::task::spawn_blocking(move || members.current_map()).await {
        Ok(Ok(map)) => map,
        Ok(Err(e)) => {
            let code = match e {
                MembershipError::Store(_) => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return error(code, &e.to_string());
        }
        Err(e) => {
            let why = format!("membership task: {e}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, &why);
        }
    };
    // No map until the first member registers: generation 0 is "no map seen".
    let view = map.map_or(
        MembersView {
            generation: 0,
            members: Vec::new(),
        },
        |map| MembersView {
            generation: map.generation(),
            members: map
                .members()
                .iter()
                .map(|m| MemberView {
                    id: m.id.0,
                    addr: m.addr.clone(),
                    state: state_name(m.state),
                    weight: m.weight,
                })
                .collect(),
        },
    );
    json(StatusCode::OK, &view)
}

async fn act(
    State(api): State<Arc<ApiState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(target): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request = Write {
        peer,
        headers: &headers,
        target: &target,
        body: &body,
    };
    match write_request(api.token.as_ref(), &request) {
        Ok((node, action)) => match api.farm.place(&node, action) {
            Ok(view) => json(StatusCode::OK, &view),
            Err(unknown) => error(StatusCode::NOT_FOUND, &unknown.to_string()),
        },
        Err((code, why)) => error(code, &why),
    }
}

/// The body of `POST /v1/members/{id}`: the state to put the member in. `Down` is the
/// store's verdict and `joining`/`catching_up` are the registration's, so none of them
/// is accepted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberBody {
    state: MemberTarget,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MemberTarget {
    Active,
    Draining,
}

async fn set_member_state(
    State(api): State<Arc<ApiState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), peer, &headers) {
        return error(code, &why);
    }
    let Some(members) = api.members.clone() else {
        return no_members();
    };
    let Ok(id) = id.parse::<u64>() else {
        return error(StatusCode::NOT_FOUND, &format!("no member {id:?}"));
    };
    let want: MemberBody = match serde_json::from_slice(&body) {
        Ok(want) => want,
        Err(e) => {
            let why = format!("a member body is {{\"state\": \"active\"|\"draining\"}}: {e}");
            return error(StatusCode::BAD_REQUEST, &why);
        }
    };
    let (state, name) = match want.state {
        MemberTarget::Active => (MemberState::Active, "active"),
        MemberTarget::Draining => (MemberState::Draining, "draining"),
    };
    // The store's calls block (its trait says so).
    let set = tokio::task::spawn_blocking(move || members.set_state(MemberId(id), state)).await;
    member_answer(id, name, set)
}

/// The answer to a member write, from what the blocking call returned.
fn member_answer(
    id: u64,
    name: &str,
    set: Result<Result<Map, MembershipError>, tokio::task::JoinError>,
) -> Response {
    match set {
        Ok(Ok(map)) => {
            tracing::info!(member = id, state = name, "member state set");
            json(
                StatusCode::OK,
                &serde_json::json!({ "id": id, "state": name, "generation": map.generation() }),
            )
        }
        Ok(Err(e)) => error(member_failure(&e), &e.to_string()),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("membership task: {e}"),
        ),
    }
}

/// The status a failed member write answers with.
const fn member_failure(e: &MembershipError) -> StatusCode {
    match e {
        MembershipError::NotRegistered(_) => StatusCode::NOT_FOUND,
        MembershipError::NotSettable(_) => StatusCode::BAD_REQUEST,
        MembershipError::Store(_) | MembershipError::Contended => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The body of `POST /v1/denied`: the entry to refuse, spelled as a line of a deny list
/// file is, and why (kept with the entry; it is required, so a deny is never anonymous).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DenyBody {
    entry: String,
    reason: String,
}

/// The answer to a request that is malformed.
fn bad_request(why: &str) -> Response {
    error(StatusCode::BAD_REQUEST, why)
}

/// `text` as a deny entry, or the 400 that says what is wrong with it. A node id is
/// lowercased: a certificate's DNS names are compared with it up to ASCII case, so
/// `Mac-07` and `mac-07` are one entry, and a removal finds what a denial wrote.
fn entry_of(text: &str) -> Result<DenyEntry, (StatusCode, String)> {
    text.parse()
        .map(|entry| match entry {
            DenyEntry::Node(node) => DenyEntry::Node(node.to_ascii_lowercase()),
            other => other,
        })
        .map_err(|e| {
            let why = format!(
                "{text:?} is not a deny entry (serial <hex>, spki-sha256 <64 hex> or node <id>): {e}"
            );
            (StatusCode::BAD_REQUEST, why)
        })
}

async fn deny_entry(
    State(api): State<Arc<ApiState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), peer, &headers) {
        return error(code, &why);
    }
    let Some(members) = api.members.clone() else {
        return no_members();
    };
    let want: DenyBody = match serde_json::from_slice(&body) {
        Ok(want) => want,
        Err(e) => {
            let why = format!(
                "a deny body is {{\"entry\": \"node <id>\"|\"serial <hex>\"|\"spki-sha256 <hash>\", \
                 \"reason\": \"...\"}}: {e}"
            );
            return bad_request(&why);
        }
    };
    let entry = match entry_of(&want.entry) {
        Ok(entry) => entry,
        Err((code, why)) => return error(code, &why),
    };
    if want.reason.trim().is_empty() {
        return bad_request("a deny needs a reason");
    }
    // The store's calls block (its trait says so).
    let (listed, reason) = (entry.clone(), want.reason);
    let done = tokio::task::spawn_blocking(move || {
        members.deny(&listed, &reason)?;
        tracing::info!(entry = %listed, reason = ?reason, "deny list entry added");
        members.refresh()
    })
    .await;
    denied_answer(&entry, done)
}

async fn allow_entry(
    State(api): State<Arc<ApiState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), peer, &headers) {
        return error(code, &why);
    }
    let Some(members) = api.members.clone() else {
        return no_members();
    };
    let query = query.unwrap_or_default();
    let text = form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "entry")
        .map(|(_, value)| value.into_owned());
    let Some(text) = text else {
        return bad_request("a deny list removal names its entry: DELETE /v1/denied?entry=<entry>");
    };
    let entry = match entry_of(&text) {
        Ok(entry) => entry,
        Err((code, why)) => return error(code, &why),
    };
    // The store's calls block (its trait says so).
    let listed = entry.clone();
    let done = tokio::task::spawn_blocking(move || {
        if !members.allow(&listed)? {
            return Ok(None);
        }
        tracing::info!(entry = %listed, "deny list entry removed");
        members.refresh().map(Some)
    })
    .await;
    allowed_answer(&entry, done)
}

/// The answer to a removal: 404 for an entry that was not listed (`None`), else as for
/// any deny list write ([`denied_answer`]).
fn allowed_answer(
    entry: &DenyEntry,
    done: Result<Result<Option<Option<Map>>, MembershipError>, tokio::task::JoinError>,
) -> Response {
    match done {
        Ok(Ok(None)) => error(
            StatusCode::NOT_FOUND,
            &format!("{entry} is not on the deny list"),
        ),
        Ok(Ok(Some(map))) => denied_answer(entry, Ok(Ok(map))),
        Ok(Err(e)) => denied_answer(entry, Ok(Err(e))),
        Err(e) => denied_answer(entry, Err(e)),
    }
}

/// The answer to a deny list write, from what the blocking call returned: the entry as
/// the list spells it and the generation of the map after the refresh the call ran (`0`
/// before any member has registered, when there is no map yet). A failure after the row was
/// written (the store failing during the refresh) leaves the row, and the map follows it
/// at the next beat.
fn denied_answer(
    entry: &DenyEntry,
    done: Result<Result<Option<Map>, MembershipError>, tokio::task::JoinError>,
) -> Response {
    match done {
        Ok(Ok(map)) => json(
            StatusCode::OK,
            &serde_json::json!({
                "entry": entry.to_string(),
                "generation": map.map_or(0, |map| map.generation()),
            }),
        ),
        Ok(Err(e)) => error(denied_failure(&e), &e.to_string()),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("membership task: {e}"),
        ),
    }
}

/// The status a failed deny list write answers with.
const fn denied_failure(e: &MembershipError) -> StatusCode {
    match e {
        MembershipError::Invalid(_) => StatusCode::BAD_REQUEST,
        MembershipError::Store(_) | MembershipError::Contended => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The answer of a server that keeps no storage members.
pub(crate) fn no_members() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "this server keeps no storage members: it has no --state-store",
    )
}

/// A write as it arrived.
#[derive(Clone, Copy, Debug)]
pub struct Write<'a> {
    /// Who sent it.
    pub peer: SocketAddr,
    /// Its headers.
    pub headers: &'a HeaderMap,
    /// The last path segment: `{node}:{verb}`.
    pub target: &'a str,
    /// Its body.
    pub body: &'a [u8],
}

/// The body a drain may carry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainBody {
    deadline_secs: u64,
}

/// The gates every write passes, in order:
///
/// 1. the peer is loopback, so the token never crosses a network in clear (a remote
///    operator comes through a TLS-terminating proxy on the host): else 403;
/// 2. no `Origin` header: a browser sends one with every cross-origin POST, and a
///    page must not act with an operator's token or from the host: else 403;
/// 3. writes are on (`token` is given): else 403;
/// 4. `Authorization: Bearer <token>`, compared in constant time: else 401;
/// 5. `Content-Type: application/json` (parameters allowed), which a browser cannot
///    send cross-origin without a preflight the API never answers: else 415.
pub(crate) fn gate(
    token: Option<&ApiToken>,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, String)> {
    operator_gate(token, peer, headers)?;
    let content_type = headers.get(CONTENT_TYPE).map(|v| v.as_bytes());
    if !content_type.is_some_and(is_json) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("a write is sent as Content-Type: {JSON}"),
        ));
    }
    Ok(())
}

/// Gates 1 to 4 of [`gate`]: who may write. A write whose body is not JSON (a release
/// tarball) passes these and answers for its own content type.
pub(crate) fn operator_gate(
    token: Option<&ApiToken>,
    peer: SocketAddr,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, String)> {
    if !peer.ip().to_canonical().is_loopback() {
        return Err((
            StatusCode::FORBIDDEN,
            format!("writes are accepted only from the server's own host, not {peer}"),
        ));
    }
    if headers.contains_key(ORIGIN) {
        return Err((
            StatusCode::FORBIDDEN,
            "a write from a browser page (with an Origin header) is refused".to_owned(),
        ));
    }
    let Some(token) = token else {
        return Err((
            StatusCode::FORBIDDEN,
            "writes are off: the server was started without --api-token-file".to_owned(),
        ));
    };
    let presented = headers.get(AUTHORIZATION).map(|v| v.as_bytes());
    if !presented.is_some_and(|p| token.admits(p)) {
        return Err((
            StatusCode::UNAUTHORIZED,
            "a write needs Authorization: Bearer <the --api-token-file token>".to_owned(),
        ));
    }
    Ok(())
}

/// The node and action `write` names, if it passes every gate (see [`gate`]) and is
/// well formed; else the status and reason to answer with.
///
/// # Errors
/// The gates; then 404 for a target without a known verb, and 400 for a drain
/// body that is not `{"deadline_secs": N}`.
pub fn write_request(
    token: Option<&ApiToken>,
    write: &Write<'_>,
) -> Result<(WorkerId, NodeAction), (StatusCode, String)> {
    let Write {
        peer,
        headers,
        target,
        body,
    } = *write;
    gate(token, peer, headers)?;
    let unknown = || (StatusCode::NOT_FOUND, format!("no action {target:?}"));
    let (node, verb) = target.rsplit_once(':').ok_or_else(unknown)?;
    let action = match verb {
        "cordon" => NodeAction::Cordon,
        "uncordon" => NodeAction::Uncordon,
        "drain" if body.is_empty() => NodeAction::Drain(DRAIN_DEADLINE),
        "drain" => {
            let drain: DrainBody = serde_json::from_slice(body).map_err(|e| {
                let why = format!("a drain body is {{\"deadline_secs\": N}}: {e}");
                (StatusCode::BAD_REQUEST, why)
            })?;
            NodeAction::Drain(Duration::from_secs(drain.deadline_secs))
        }
        _ => return Err(unknown()),
    };
    Ok((WorkerId::new(node), action))
}

/// Whether a `Content-Type` value is `application/json`, in any case, with or without
/// parameters (`; charset=utf-8`).
fn is_json(value: &[u8]) -> bool {
    let essence = value.split(|&b| b == b';').next().unwrap_or_default();
    essence.trim_ascii().eq_ignore_ascii_case(JSON.as_bytes())
}

/// `value` as a JSON body. The views serialize plain strings, numbers and lists, which
/// cannot fail.
pub(crate) fn json(code: StatusCode, value: &impl serde::Serialize) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    (code, [(CONTENT_TYPE, JSON)], body).into_response()
}

/// An error answer; a 401 names the scheme it wants (RFC 9110 section 11.6.1).
pub(crate) fn error(code: StatusCode, why: &str) -> Response {
    let mut answer = json(code, &serde_json::json!({ "error": why }));
    if code == StatusCode::UNAUTHORIZED {
        let bearer = axum::http::HeaderValue::from_static("Bearer");
        answer.headers_mut().insert(WWW_AUTHENTICATE, bearer);
    }
    answer
}

#[cfg(test)]
mod tests {
    use kbf_map::{Member, Rule};
    use kbf_state::StoreError;

    use super::*;

    fn map() -> Map {
        let member = Member {
            id: MemberId(4),
            addr: "192.0.2.4:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
            boot: 0,
        };
        Map::new(9, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("a map")
    }

    async fn body(answer: Response) -> (StatusCode, serde_json::Value) {
        let code = answer.status();
        let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        (code, serde_json::from_slice(&bytes).expect("JSON"))
    }

    /// Catches a member write whose failure is answered with the wrong status (a client
    /// told to retry what is its own mistake, or the reverse), and an answer that does not
    /// carry the generation the change produced: a bad request, an unknown member, a
    /// store that is down or contended, an unreadable row and a panicked task each get
    /// their own, and the success names the member, its state and the map's generation.
    #[tokio::test]
    async fn a_member_write_answers_each_outcome_with_its_status() {
        let (code, ok) = body(member_answer(4, "active", Ok(Ok(map())))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            ok,
            serde_json::json!({"id": 4, "state": "active", "generation": 9})
        );
        let store = || MembershipError::Store(StoreError::Failed("down".to_owned()));
        let failures = [
            (
                MembershipError::NotRegistered(MemberId(4)),
                StatusCode::NOT_FOUND,
            ),
            (
                MembershipError::NotSettable(MemberState::Down),
                StatusCode::BAD_REQUEST,
            ),
            (store(), StatusCode::SERVICE_UNAVAILABLE),
            (MembershipError::Contended, StatusCode::SERVICE_UNAVAILABLE),
            (
                MembershipError::Corrupt("row".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (failure, want) in failures {
            let text = failure.to_string();
            let (code, answer) = body(member_answer(4, "active", Ok(Err(failure)))).await;
            assert_eq!(code, want, "{text}");
            assert_eq!(answer["error"], text);
        }
        let panicked = tokio::task::spawn_blocking(|| -> Result<Map, MembershipError> {
            panic!("the membership call panicked")
        })
        .await;
        let (code, answer) = body(member_answer(
            4,
            "active",
            Err(panicked.expect_err("a panic")),
        ))
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            answer["error"]
                .as_str()
                .unwrap_or_default()
                .contains("membership task")
        );
    }

    /// Catches a deny list write whose failure is answered with the wrong status or whose
    /// success loses the entry or the generation: the entry is named as stored and the map's
    /// generation is given (`0` before there is a map), a bound the store enforces is the
    /// caller's mistake (400), a store that is down or contended is to be retried (503), and an
    /// unreadable row and a panicked task are ours (500).
    #[tokio::test]
    async fn a_deny_write_answers_each_outcome_with_its_status() {
        let entry = DenyEntry::Serial("a1b".to_owned());
        let (code, ok) = body(denied_answer(&entry, Ok(Ok(Some(map()))))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            ok,
            serde_json::json!({"entry": "serial a1b", "generation": 9})
        );
        let (code, none) = body(denied_answer(&entry, Ok(Ok(None)))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            none,
            serde_json::json!({"entry": "serial a1b", "generation": 0})
        );
        let failures = [
            (
                MembershipError::Invalid("the deny list is full"),
                StatusCode::BAD_REQUEST,
            ),
            (
                MembershipError::Store(StoreError::Failed("down".to_owned())),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (MembershipError::Contended, StatusCode::SERVICE_UNAVAILABLE),
            (
                MembershipError::Corrupt("row".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (failure, want) in failures {
            let text = failure.to_string();
            let (code, answer) = body(denied_answer(&entry, Ok(Err(failure)))).await;
            assert_eq!(code, want, "{text}");
            assert_eq!(answer["error"], text);
        }
        let panicked = tokio::task::spawn_blocking(|| -> Result<Option<Map>, MembershipError> {
            panic!("the membership call panicked")
        })
        .await;
        let (code, answer) = body(denied_answer(&entry, Err(panicked.expect_err("a panic")))).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            answer["error"]
                .as_str()
                .unwrap_or_default()
                .contains("membership task")
        );
    }

    /// Catches a removal answered as a write when nothing was listed, or whose failure is
    /// lost: an entry that was not listed is 404 and names the entry as the list spells it;
    /// one that was is answered as every deny list write is (the entry and the generation); a
    /// failure after it was removed (the store failing during the refresh) and a panicked
    /// task keep their statuses.
    #[tokio::test]
    async fn a_removal_answers_each_outcome_with_its_status() {
        let entry = DenyEntry::Node("mac-07".to_owned());
        let (code, none) = body(allowed_answer(&entry, Ok(Ok(None)))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(none["error"], "node mac-07 is not on the deny list");
        let (code, ok) = body(allowed_answer(&entry, Ok(Ok(Some(Some(map())))))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            ok,
            serde_json::json!({"entry": "node mac-07", "generation": 9})
        );
        let (code, ok) = body(allowed_answer(&entry, Ok(Ok(Some(None))))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(ok["generation"], 0);
        let down = MembershipError::Store(StoreError::Failed("down".to_owned()));
        let (code, _) = body(allowed_answer(&entry, Ok(Err(down)))).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        let panicked =
            tokio::task::spawn_blocking(|| -> Result<Option<Option<Map>>, MembershipError> {
                panic!("the membership call panicked")
            })
            .await;
        let (code, _) = body(allowed_answer(&entry, Err(panicked.expect_err("a panic")))).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
