//! The real [`FleetReader`]: the three reads rule R1 judges, made of the operator port of
//! the child that is active now, over [`child_http`](crate::proxy::child_http).
//!
//! | read | route | what it carries |
//! |---|---|---|
//! | daemons | `GET /v1/nodes` | nothing: an open read |
//! | storage members | `GET /v1/members` | nothing: an open read |
//! | enrolled nodes | `GET /v1/enroll/nodes` | the proxy's secret, `Authorization: Bearer <operator token>` and `Content-Type: application/json`; never `Origin` |
//!
//! All three are routes of the operator API (`docs/api.md`), so one address serves them. The
//! enrollment port of a slot is the nodes' own wire for enrolling and is not asked. The
//! enrollment route passes every gate of a write even to be read (`docs/api.md`, "Who may
//! write"): a request that carries less is refused `401`, `403` or `415`, and that is
//! [`Unreadable`], so the gate refuses the update. The secret and the token go where a gate
//! needs them and nowhere else.
//!
//! What [`ChildFleet`] promises, each of which the [`FleetReader`] contract asks and a test
//! pins:
//! - **The active child is named at every read**, by a closure the caller gives, and not
//!   when the reader is built: the active child's port changes after every swap. One read
//!   asks it once, so the three answers of one snapshot come from one child. None active is
//!   [`Unreadable`] in every field, never an empty fleet.
//! - **One deadline for the three requests together.** A request is given what is left of
//!   it, so a child that is slow on the first leaves the third less, and one that answers
//!   nothing costs the deadline once. A spent budget fails the rest at once. The caller
//!   keeps it under the time the actor's callers wait ([`ActorTiming::deadline`]).
//! - **A blocking client**, since `read` runs on the actor's own thread.
//! - **A failed read is a value**: a refused connection, a timeout, an answer over the cap
//!   or one that is not HTTP is [`Unreadable`] for that read, and the other two are still
//!   made. What a status and a body mean is [`fleet`](super::fleet)'s: a `404` from the
//!   enrollment route is an empty enrolled set, any other status but `200` is unreadable,
//!   and an envelope without `oldest_protocol` or `protocol_unknown` is unreadable.
//! - **Loopback only**: [`child_http`](crate::proxy::child_http) refuses any other address,
//!   so a mistaken port table cannot send the secret or the token off the host.
//!
//! The token comes from a closure and is read at every enrollment read, so a rotated file is
//! used and a file that cannot be read makes that one read unreadable. It is a closure, as
//! for [`admin::Admin`](crate::proxy::admin::Admin), and not
//! [`auth::TokenSource`](super::auth::TokenSource): that one hands the API's own bearer check
//! a [`Secret`](super::auth::Secret), which gives no bytes back to put in a header.
//!
//! Nothing constructs a [`ChildFleet`] yet: the proxy verb wires it with the update API.
//!
//! [`ActorTiming::deadline`]: super::actor::ActorTiming::deadline

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hyper::StatusCode;

use crate::proxy::child_http::{Client, Headers, Limits};
use crate::proxy::model::Slot;

use super::fleet::{Enrolled, FleetReader, FleetSnapshot, Population, Unreadable};

const NODES: &str = "/v1/nodes";
const MEMBERS: &str = "/v1/members";
const ENROLLED: &str = "/v1/enroll/nodes";

/// The most an answer may hold. A node entry with its software survey is a few KiB, so this
/// is thousands of nodes; a child that sends more is not read.
pub(super) const MAX_ANSWER: usize = 8 * 1024 * 1024;

/// Reads the fleet from the operator port of the active child.
pub struct ChildFleet {
    active: Box<dyn Fn() -> Option<Slot> + Send>,
    operator: Box<dyn Fn(Slot) -> SocketAddr + Send>,
    secret: Headers,
    token: Box<dyn Fn() -> io::Result<Vec<u8>> + Send>,
    deadline: Duration,
}

impl ChildFleet {
    /// `active` names the slot of the child that serves now (`None` between children, or
    /// before the first); `operator` says where a slot's operator port is, and a call to an
    /// address that is not loopback fails. `secret` is the proxy's header, sent with the
    /// enrollment read. `token` yields the operator token on every enrollment read;
    /// surrounding whitespace (the file's newline) is dropped. `deadline` bounds the three
    /// reads together.
    pub fn new(
        active: impl Fn() -> Option<Slot> + Send + 'static,
        operator: impl Fn(Slot) -> SocketAddr + Send + 'static,
        secret: Headers,
        token: impl Fn() -> io::Result<Vec<u8>> + Send + 'static,
        deadline: Duration,
    ) -> ChildFleet {
        ChildFleet {
            active: Box::new(active),
            operator: Box::new(operator),
            secret,
            token: Box::new(token),
            deadline,
        }
    }

    /// `GET path` of `addr` with what is left of the deadline that began at `began`: the
    /// status and body of the answer, whatever the status.
    fn get(
        &self,
        addr: SocketAddr,
        began: Instant,
        path: &str,
        headers: &Headers,
    ) -> Result<(StatusCode, Vec<u8>), Unreadable> {
        let limits = Limits {
            deadline: self.deadline.saturating_sub(began.elapsed()),
            max_body: MAX_ANSWER,
        };
        let answer = Client::new(addr, limits)
            .and_then(|client| client.get(path, headers))
            .map_err(|error| Unreadable(format!("GET {path}: {error}")))?;
        // The client reads only final statuses (200 to 599), so the fallback is not reached;
        // it is a status that is not a 200 and not a 404.
        let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
        Ok((status, answer.body))
    }

    /// The headers of the enrollment read: the secret, the operator token as a bearer
    /// credential, and the JSON content type.
    fn enrollment_headers(&self) -> Result<Headers, Unreadable> {
        let unreadable = |why: &str| Unreadable(format!("GET {ENROLLED}: {why}"));
        let token = (self.token)().map_err(|e| {
            unreadable(&format!("the operator token cannot be read ({})", e.kind()))
        })?;
        let mut bearer = b"Bearer ".to_vec();
        bearer.extend_from_slice(token.trim_ascii());
        self.secret
            .clone()
            .with("authorization", &bearer)
            .map(|headers| headers.and(&Headers::json()))
            .map_err(|_| unreadable("the operator token cannot be sent as a header"))
    }
}

impl FleetReader for ChildFleet {
    fn read(&self) -> FleetSnapshot {
        let began = Instant::now();
        let Some(slot) = (self.active)() else {
            let none = || Unreadable("no child is active".to_owned());
            return FleetSnapshot {
                nodes: Err(none()),
                members: Err(none()),
                enrolled: Err(none()),
            };
        };
        let addr = (self.operator)(slot);
        let open = Headers::new();
        FleetSnapshot {
            nodes: self
                .get(addr, began, NODES, &open)
                .and_then(|(status, body)| Population::from_nodes(status, &body)),
            members: self
                .get(addr, began, MEMBERS, &open)
                .and_then(|(status, body)| Population::from_members(status, &body)),
            enrolled: self
                .enrollment_headers()
                .and_then(|headers| self.get(addr, began, ENROLLED, &headers))
                .and_then(|(status, body)| Enrolled::from_registry(status, &body)),
        }
    }
}
