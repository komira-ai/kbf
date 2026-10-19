//! [`ChildAdmin`] over a child's loopback ports, through [`child_http`](super::child_http).
//!
//! The child is a `kbf --mode frontend` that serves the admin calls on one port (`GET
//! /v1/admin/role`, `POST /v1/admin/{quiesce,resume,takeover,stand-down}`) and its operator
//! API on another. Where each slot's ports are is the caller's: two functions from a slot
//! to an address, one per port, so this module knows no port table.
//!
//! | trait method | call | port | deadline |
//! |---|---|---|---|
//! | `role` | `GET /v1/admin/role` | admin | [`Deadlines::call`] |
//! | `takeover` | `GET /v1/admin/role`, then `POST /v1/admin/takeover {"expect": generation}` | admin | the `limit` argument, both calls together |
//! | `quiesce` | `POST /v1/admin/quiesce` | admin | [`Deadlines::quiesce`] |
//! | `stand_down` | `POST /v1/admin/stand-down` | admin | [`Deadlines::call`] |
//! | `resume` | `POST /v1/admin/resume` | admin | [`Deadlines::call`] |
//! | `ready` | `GET /readyz` | operator | [`Deadlines::call`] |
//! | `open_counts` | `GET /v1/frontend/drain` | operator | [`Deadlines::call`] |
//! | `post_desired` | `POST /v1/update/desired {"commit": ...}` | operator | [`Deadlines::call`] |
//!
//! Each method is in the trait and, with a typed error, as `try_<name>`; the trait flattens
//! [`AdminError`] to the text of a [`PortError`], which is all the swap keeps.
//!
//! # What the child does not do that the trait promises
//!
//! The wire is the one the child documents (`docs/api.md`, "The admin calls"), and it is
//! narrower than the doc comments of [`ChildAdmin`]. Each gap is answered here without
//! hiding it; none is fixed here, since the fix is in the child.
//!
//! - **`takeover`'s `limit` has no field on the wire.** The body is `{"expect": N}` and
//!   anything else is refused. `limit` is the client-side deadline for the whole call,
//!   the `role` read that finds `N` included. A takeover that outlives it is
//!   [`AdminError::Timeout`]: the call may or may not have landed, and only a `role` read
//!   tells (the swap reads it).
//! - **`quiesce` blocks.** The child answers when it is quiesced, after up to 10 s, while the
//!   swap gives it [`Deadlines::quiesce`] (3 s by default). On a timeout the child may still
//!   finish the quiesce, or may drop it when the connection closes: the outcome is unknown,
//!   as for `takeover`. There is no `Quiescing` state on the wire, so [`RoleState::Quiescing`]
//!   is never reported: a child is `Active` until the call returns and `Quiesced` after it.
//! - **`resume` does not undo a stand-down, and does not re-take a lost generation.** The
//!   child resumes from `active` or `quiesced` while the store is still at the generation
//!   it took, and nothing is reloaded. After a stand-down, or once another writer took the
//!   generation, it refuses: [`AdminError::StoodDown`] and [`AdminError::Superseded`], so a
//!   rollback after the swap's stand-down of the old child ends as a failed rollback and
//!   not as a resumed child that is not serving.
//!
//! # `RoleInfo::holder`
//!
//! The trait says the store wins every disagreement, so `holder` is a fact about the store
//! and not about the role: the child holds the generation it took (`held_generation`), the
//! store is still at it (`generation`) and the store has not refused a write of the child
//! (`superseded`). A stood-down child that took the generation and has not been replaced
//! still holds it.
//!
//! | `held_generation` | `generation` | `superseded` | `holder` |
//! |---|---|---|---|
//! | none (a candidate) | any | any | `false` |
//! | `N` | `N` | `false` | `true` |
//! | `N` | `N` | `true` | `false` |
//! | `N` | another | any | `false` |
//! | `N` | none (the store cannot be read) | any | [`AdminError::Unreadable`] |
//!
//! `role` is `candidate`, `active`, `quiesced` or `stood-down`; another word is an answer
//! this client cannot read ([`AdminError::BadAnswer`]), and so is an `admin_protocol` other
//! than the one in this file ([`AdminError::Protocol`]).
//!
//! # Readiness
//!
//! `ready` asks the operator port's `GET /readyz` (the port the proxy's own poll of the
//! active child asks): `200` is ready, `503` is not, and any other status is an error, since
//! it is not an answer of `/readyz`. [`Readiness`] has three parts and only this one is
//! asked: `health` and `capabilities` stay `false`, the fail-closed value, so
//! [`Readiness::all`] is `false` for every child until the `grpc.health.v1` and
//! `GetCapabilities` probes exist. A swap built on this type does not get past its step 5
//! before then.
//!
//! # `post_desired`
//!
//! It goes to the operator port of the slot it is given, which is the new child's (the swap
//! posts after the bake, to the child that now serves). It is a write, so it carries the
//! gates of the operator API: the secret the caller supplies as headers, the operator token
//! (read from its source on every call, so a rotated token is used) as `Authorization:
//! Bearer`, and `Content-Type: application/json`; never `Origin`. The body is the commit
//! alone. A `409` is [`AdminError::NotOwn`] when its body says `"code": "not_own"` (a commit
//! other than the child's own build, which needs a rollback flag this body does not
//! carry), and [`AdminError::Conflict`] otherwise. The child's answer today has no `code`,
//! only an `error` text, so today every `409` is a [`AdminError::Conflict`]; the code is
//! what a distinct answer is keyed on, not the text. A `503` ("other writers kept racing")
//! is [`AdminError::Status`]: nothing here retries, since the client returns no response
//! headers and with them no `Retry-After`.
//!
//! # Credentials
//!
//! The admin calls carry the caller's secret headers. The child does not read them today
//! (the admin calls carry no credential), and a child that gates the port on them later
//! needs no change here. Reads of the operator port (`/readyz`, the drain read) carry
//! nothing: they are open, and a secret is sent only where it can be needed.
//!
//! Every call blocks the calling thread for up to its deadline: the swap runs on a thread of
//! its own.

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::CommitId;

use super::child_http::{Client, Headers, HttpError, Limits, Response};
use super::model::{OpenCounts, PortError, Readiness, RoleInfo, RoleState, Slot, Timing};
use super::ports::ChildAdmin;

/// The `admin_protocol` this client reads. Every `role` answer says its own.
const ADMIN_PROTOCOL: u32 = 1;

/// The most an answer may hold. A role view is a few hundred bytes.
const MAX_ANSWER: usize = 64 * 1024;

/// The `code` of a `409` from `POST /v1/update/desired` for a commit that is not the child's own.
const NOT_OWN: &str = "not_own";

/// The most characters kept of a child's `error` text.
const MAX_REASON: usize = 200;

/// How long the calls may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    /// Every call that does not wait for work: `role`, `stand_down`, `resume`, `ready`,
    /// `open_counts`, `post_desired`.
    pub call: Duration,
    /// `quiesce`: the swap's budget. The child may take longer; this is not its limit.
    pub quiesce: Duration,
}

impl Deadlines {
    /// The limits of a frontend host: [`Timing::frontend`]'s quiesce budget, and the 3 s the
    /// proxy gives its own readiness poll for any other call.
    pub fn frontend() -> Deadlines {
        Deadlines {
            call: Duration::from_secs(3),
            quiesce: Timing::frontend().quiesce,
        }
    }
}

/// Why a call failed. The first group is a call that did not reach an answer, the second a
/// child that answered and refused, the third a call that was not made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdminError {
    /// No answer inside the deadline. The call may have landed.
    #[error("{call}: no answer within the deadline (the call may have landed)")]
    Timeout { call: &'static str },
    /// The child was not reached, or what answered did not speak HTTP the way this client reads.
    #[error("{call}: {error}")]
    Transport {
        call: &'static str,
        error: HttpError,
    },
    /// A 2xx answer that is not JSON of the shape the call reads.
    #[error("{call}: the answer cannot be read: {why}")]
    BadAnswer { call: &'static str, why: String },
    /// The child speaks another version of the admin calls.
    #[error("{call}: the child speaks admin protocol {found}, this client {ADMIN_PROTOCOL}")]
    Protocol { call: &'static str, found: u32 },
    /// The child holds a generation and cannot read the store, so it cannot say whether it
    /// still holds it.
    #[error(
        "{call}: the child cannot read the state store, so it cannot say if it holds the generation"
    )]
    Unreadable { call: &'static str },
    /// `404`: the address does not serve this call (the child was not started with an admin
    /// listener, or this is not a kbf frontend).
    #[error("{call}: nothing serves this call at the address (404)")]
    NotFound { call: &'static str },
    /// `409`: the call does not apply to the child as it is.
    #[error("{call}: the call does not apply: {error}")]
    Conflict { call: &'static str, error: String },
    /// `409` from a child that stood down. A stand-down is never undone: the child cannot
    /// serve again, whatever the call.
    #[error("{call}: the child stood down, which is never undone; it cannot serve again")]
    StoodDown { call: &'static str },
    /// `409` from a child that is not stood down and does not hold the generation any more:
    /// another writer took it, and the child can only stand down.
    #[error("{call}: another writer holds the generation; the child can only stand down")]
    Superseded { call: &'static str },
    /// `500`: the transition began and failed, and the child cannot go on as it was (a
    /// takeover that took the generation and could not start ends the process).
    #[error("{call}: the child's transition failed: {error}")]
    Failed { call: &'static str, error: String },
    /// Any other status.
    #[error("{call}: the child answered {code}: {error}")]
    Status {
        call: &'static str,
        code: u16,
        error: String,
    },
    /// `takeover` needs the generation the store is at, to pass as `expect`, and the
    /// candidate's `role` says it cannot read it.
    #[error("takeover: the candidate cannot read the generation the store is at")]
    NoGeneration,
    /// `post_desired`: a `409` that says the commit is not the child's own release. Not a
    /// retry: the offer has to say it is a rollback, or go to a child that was built from it.
    #[error("post_desired: the commit is not the child's own release: {error}")]
    NotOwn { error: String },
    /// `post_desired`: the operator token could not be read, so nothing was sent.
    #[error("post_desired: the operator token cannot be read: {0}")]
    Token(io::ErrorKind),
}

impl From<AdminError> for PortError {
    fn from(error: AdminError) -> PortError {
        PortError(error.to_string())
    }
}

/// A word of `role`. `#[serde(rename_all)]` spells the wire's `stood-down`.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Role {
    Candidate,
    Active,
    Quiesced,
    StoodDown,
}

/// The fields of the child's `role` answer this client reads.
#[derive(Deserialize)]
struct View {
    role: Role,
    admin_protocol: u32,
    generation: Option<u64>,
    held_generation: Option<u64>,
    superseded: bool,
}

impl View {
    /// The swap's reading of the answer (see the table in the module doc).
    fn info(&self, call: &'static str) -> Result<RoleInfo, AdminError> {
        let holder = match (self.held_generation, self.generation) {
            (None, _) => false,
            (Some(held), Some(now)) => held == now && !self.superseded,
            (Some(_), None) => return Err(AdminError::Unreadable { call }),
        };
        let state = match self.role {
            Role::Candidate => RoleState::Candidate,
            Role::Active => RoleState::Active,
            Role::Quiesced => RoleState::Quiesced,
            Role::StoodDown => RoleState::StoodDown,
        };
        Ok(RoleInfo { holder, state })
    }
}

/// The body of a `409`: the reason, and the `role` answer when the refusal comes from the
/// admin calls (a refusal from the operator API has none).
#[derive(Default, Deserialize)]
struct Refusal {
    #[serde(default)]
    error: String,
    #[serde(flatten)]
    view: Option<View>,
}

/// The body of any refusal: the reason, and a machine-readable `code` when the child names one.
#[derive(Default, Deserialize)]
struct Reason {
    #[serde(default)]
    error: String,
    code: Option<String>,
}

/// `GET /v1/frontend/drain`, the two counts the swap waits on.
#[derive(Deserialize)]
struct Drain {
    open_executions: u64,
    queued_operations: u64,
}

/// A child's admin and operator ports, addressed by slot.
pub struct Admin {
    admin: Box<dyn Fn(Slot) -> SocketAddr + Send + Sync>,
    operator: Box<dyn Fn(Slot) -> SocketAddr + Send + Sync>,
    /// The secret headers, and the same with `Content-Type: application/json` (every admin
    /// POST has a JSON body).
    secret: Headers,
    secret_json: Headers,
    token: Box<dyn Fn() -> io::Result<Vec<u8>> + Send + Sync>,
    deadlines: Deadlines,
}

impl Admin {
    /// `admin` and `operator` say where a slot's two ports are; a call to an address that
    /// is not loopback is refused ([`HttpError::NotLoopback`]). `secret` is sent with the
    /// admin calls and `post_desired`. `token` yields the operator token on every
    /// `post_desired`; surrounding whitespace (the file's newline) is dropped.
    pub fn new(
        admin: impl Fn(Slot) -> SocketAddr + Send + Sync + 'static,
        operator: impl Fn(Slot) -> SocketAddr + Send + Sync + 'static,
        secret: Headers,
        token: impl Fn() -> io::Result<Vec<u8>> + Send + Sync + 'static,
        deadlines: Deadlines,
    ) -> Admin {
        Admin {
            admin: Box::new(admin),
            operator: Box::new(operator),
            secret_json: secret.clone().and(&Headers::json()),
            secret,
            token: Box::new(token),
            deadlines,
        }
    }

    pub fn try_role(&self, slot: Slot) -> Result<RoleInfo, AdminError> {
        self.admin_get("role", slot, "/v1/admin/role", self.deadlines.call)
            .and_then(|view| view.info("role"))
    }

    /// Read the generation the store is at, then take it. `limit` bounds both calls
    /// together, so the second gets what the first left.
    pub fn try_takeover(&self, slot: Slot, limit: Duration) -> Result<(), AdminError> {
        let began = Instant::now();
        let seen = self.admin_get("takeover", slot, "/v1/admin/role", limit)?;
        let expect = seen.generation.ok_or(AdminError::NoGeneration)?;
        let body = format!("{{\"expect\":{expect}}}");
        let left = limit.saturating_sub(began.elapsed());
        self.admin_post(
            "takeover",
            slot,
            "/v1/admin/takeover",
            left,
            body.as_bytes(),
        )
        .map(drop)
    }

    pub fn try_quiesce(&self, slot: Slot) -> Result<(), AdminError> {
        let deadline = self.deadlines.quiesce;
        self.admin_post("quiesce", slot, "/v1/admin/quiesce", deadline, b"")
            .map(drop)
    }

    pub fn try_stand_down(&self, slot: Slot) -> Result<(), AdminError> {
        let deadline = self.deadlines.call;
        self.admin_post("stand_down", slot, "/v1/admin/stand-down", deadline, b"")
            .map(drop)
    }

    pub fn try_resume(&self, slot: Slot) -> Result<(), AdminError> {
        let deadline = self.deadlines.call;
        self.admin_post("resume", slot, "/v1/admin/resume", deadline, b"")
            .map(drop)
    }

    pub fn try_ready(&self, slot: Slot) -> Result<Readiness, AdminError> {
        let addr = (self.operator)(slot);
        let answer = get(
            "ready",
            addr,
            self.deadlines.call,
            "/readyz",
            &Headers::new(),
        )?;
        let readyz = match answer.status {
            200 => true,
            503 => false,
            // Not an answer of `/readyz`: whatever is at the address, it is not the operator API.
            code => {
                return Err(AdminError::Status {
                    call: "ready",
                    code,
                    error: reason(&answer.body).error,
                });
            }
        };
        Ok(Readiness {
            readyz,
            health: false,
            capabilities: false,
        })
    }

    pub fn try_open_counts(&self, slot: Slot) -> Result<OpenCounts, AdminError> {
        let addr = (self.operator)(slot);
        let headers = Headers::new();
        get(
            "open_counts",
            addr,
            self.deadlines.call,
            "/v1/frontend/drain",
            &headers,
        )
        .and_then(|answer| judge("open_counts", answer))
        .and_then(|body| parse::<Drain>("open_counts", &body))
        .map(|drain| OpenCounts {
            open_executions: drain.open_executions,
            queued_operations: drain.queued_operations,
        })
    }

    pub fn try_post_desired(&self, slot: Slot, commit: &CommitId) -> Result<(), AdminError> {
        let headers = self.write_headers()?;
        let body = format!("{{\"commit\":\"{}\"}}", commit.as_str());
        let answer = post(
            "post_desired",
            (self.operator)(slot),
            self.deadlines.call,
            "/v1/update/desired",
            &headers,
            body.as_bytes(),
        )?;
        match answer.status {
            409 if reason(&answer.body).code.as_deref() == Some(NOT_OWN) => {
                Err(AdminError::NotOwn {
                    error: reason(&answer.body).error,
                })
            }
            _ => judge("post_desired", answer).map(drop),
        }
    }

    /// The secret, the operator token and the JSON content type: the gates of a write.
    fn write_headers(&self) -> Result<Headers, AdminError> {
        let token = (self.token)().map_err(|e| AdminError::Token(e.kind()))?;
        let mut bearer = b"Bearer ".to_vec();
        bearer.extend_from_slice(token.trim_ascii());
        self.secret
            .clone()
            .with("authorization", &bearer)
            .map(|headers| headers.and(&Headers::json()))
            .map_err(|error| failed("post_desired", error))
    }

    fn admin_get(
        &self,
        call: &'static str,
        slot: Slot,
        path: &str,
        deadline: Duration,
    ) -> Result<View, AdminError> {
        get(call, (self.admin)(slot), deadline, path, &self.secret)
            .and_then(|answer| view(call, answer))
    }

    fn admin_post(
        &self,
        call: &'static str,
        slot: Slot,
        path: &str,
        deadline: Duration,
        body: &[u8],
    ) -> Result<View, AdminError> {
        let addr = (self.admin)(slot);
        post(call, addr, deadline, path, &self.secret_json, body)
            .and_then(|answer| view(call, answer))
    }
}

impl ChildAdmin for Admin {
    fn role(&self, slot: Slot) -> Result<RoleInfo, PortError> {
        self.try_role(slot).map_err(PortError::from)
    }

    fn takeover(&self, slot: Slot, limit: Duration) -> Result<(), PortError> {
        self.try_takeover(slot, limit).map_err(PortError::from)
    }

    fn quiesce(&self, slot: Slot) -> Result<(), PortError> {
        self.try_quiesce(slot).map_err(PortError::from)
    }

    fn stand_down(&self, slot: Slot) -> Result<(), PortError> {
        self.try_stand_down(slot).map_err(PortError::from)
    }

    fn resume(&self, slot: Slot) -> Result<(), PortError> {
        self.try_resume(slot).map_err(PortError::from)
    }

    fn ready(&self, slot: Slot) -> Result<Readiness, PortError> {
        self.try_ready(slot).map_err(PortError::from)
    }

    fn open_counts(&self, slot: Slot) -> Result<OpenCounts, PortError> {
        self.try_open_counts(slot).map_err(PortError::from)
    }

    fn post_desired(&self, slot: Slot, commit: &CommitId) -> Result<(), PortError> {
        self.try_post_desired(slot, commit).map_err(PortError::from)
    }
}

fn limits(deadline: Duration) -> Limits {
    Limits {
        deadline,
        max_body: MAX_ANSWER,
    }
}

fn get(
    call: &'static str,
    addr: SocketAddr,
    deadline: Duration,
    path: &str,
    headers: &Headers,
) -> Result<Response, AdminError> {
    Client::new(addr, limits(deadline))
        .and_then(|client| client.get(path, headers))
        .map_err(|error| failed(call, error))
}

fn post(
    call: &'static str,
    addr: SocketAddr,
    deadline: Duration,
    path: &str,
    headers: &Headers,
    body: &[u8],
) -> Result<Response, AdminError> {
    Client::new(addr, limits(deadline))
        .and_then(|client| client.post(path, headers, body))
        .map_err(|error| failed(call, error))
}

/// A call that got no answer. A timeout is told apart: the call may have landed.
fn failed(call: &'static str, error: HttpError) -> AdminError {
    match error {
        HttpError::Timeout => AdminError::Timeout { call },
        error => AdminError::Transport { call, error },
    }
}

/// The body of a 2xx answer, or the error the status stands for.
fn judge(call: &'static str, answer: Response) -> Result<Vec<u8>, AdminError> {
    match answer.status {
        200..=299 => Ok(answer.body),
        404 => Err(AdminError::NotFound { call }),
        409 => Err(conflict(call, &answer.body)),
        500 => Err(AdminError::Failed {
            call,
            error: reason(&answer.body).error,
        }),
        code => Err(AdminError::Status {
            call,
            code,
            error: reason(&answer.body).error,
        }),
    }
}

/// A `409`: a child that stood down, one that lost the generation, or any other refusal.
fn conflict(call: &'static str, body: &[u8]) -> AdminError {
    let refusal: Refusal = serde_json::from_slice(body).unwrap_or_default();
    match refusal.view.and_then(|view| view.info(call).ok()) {
        Some(RoleInfo {
            state: RoleState::StoodDown,
            ..
        }) => AdminError::StoodDown { call },
        Some(RoleInfo {
            state: RoleState::Active | RoleState::Quiesced,
            holder: false,
        }) => AdminError::Superseded { call },
        _ => AdminError::Conflict {
            call,
            error: clip(refusal.error),
        },
    }
}

/// What a refusal says, with its text cut to [`MAX_REASON`] characters. A body that is not
/// JSON says nothing.
fn reason(body: &[u8]) -> Reason {
    let mut reason: Reason = serde_json::from_slice(body).unwrap_or_default();
    reason.error = clip(reason.error);
    reason
}

fn clip(text: String) -> String {
    text.chars().take(MAX_REASON).collect()
}

fn parse<T: DeserializeOwned>(call: &'static str, body: &[u8]) -> Result<T, AdminError> {
    serde_json::from_slice(body).map_err(|e| AdminError::BadAnswer {
        call,
        why: e.to_string(),
    })
}

/// The `role` answer of a call, if the call applied and the child speaks this protocol.
fn view(call: &'static str, answer: Response) -> Result<View, AdminError> {
    let view: View = judge(call, answer).and_then(|body| parse(call, &body))?;
    if view.admin_protocol == ADMIN_PROTOCOL {
        Ok(view)
    } else {
        Err(AdminError::Protocol {
            call,
            found: view.admin_protocol,
        })
    }
}
