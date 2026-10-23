//! One stream to a backend: bind through the gate (waiting in the hold queue if it holds),
//! dispatch to the pool, carry the bytes both ways, and dispatch again after a safe failure.
//!
//! The permit rule (design note section 5): a stream binds to the slot active when its
//! headers arrived (a `WaitExecution` naming the old child's term, to the old child) and keeps
//! the permit until both upstream halves are finished.
//!
//! In pool mode there is no gate: no hold and no term routing. A stream is placed on a member
//! of the pool, for as long as it lives, and a failure that is safe to try again (a refused
//! connection, a refused stream, a GOAWAY) places it again on a member other than those that
//! failed it.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use h2::{Reason, RecvStream};
use http::{HeaderMap, Method, Request, Response, Uri, Version};
use hyper::body::Incoming;
use tokio::sync::watch;

use crate::auth::Refusal as AuthRefusal;
use crate::body::{Activity, Down, Lifetime, ProxyBody, Source, StreamPermit, idle_expired, pump};
use crate::core::{Core, StreamCount, now};
use crate::hold::Wake;
use crate::identity::{Identity, forward_headers, strip_reserved};
use crate::metrics::Metrics;
use crate::pool::TargetPool;
use crate::replay::Failure;
use crate::reply::{refused, unavailable};
use crate::route::{Idle, Target, idle_of, is_wait_execution};
use crate::slot::{Bind, Pending, Permit};
use crate::term::{Probe, decided, probe};

/// How long the start of a `WaitExecution` request is waited for, to read the operation it
/// names. A client sends the message with its headers; one that sends it later is dispatched
/// without being read, to the active child, and loses nothing else.
pub(crate) const PEEK_FOR: Duration = Duration::from_secs(2);

/// Why a stream was refused before it reached a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    NoActive,
    QueueFull,
    Expired,
    Retries,
}

impl Refusal {
    const fn message(self) -> &'static str {
        match self {
            Self::NoActive => "no frontend is active",
            Self::QueueFull => "too many streams are waiting for a frontend",
            Self::Expired => "no frontend became active in time",
            Self::Retries => "the frontend did not take the stream",
        }
    }
}

/// Everything an attempt reads about its stream, the same for each attempt.
struct Stream {
    core: Arc<Core>,
    target: Target,
    path: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    source: Arc<Source>,
    activity: Arc<Activity>,
    count: Arc<StreamCount>,
    idle: Idle,
    grpc: bool,
    /// The term of the operation a `WaitExecution` names, if it names one: the stream binds
    /// to the child with that term while it lives, and to the active child otherwise.
    term: Option<u64>,
}

/// Serves a request that goes to a backend. `pending` counts the stream from accept.
/// `identity` is the verified certificate's, if the client presented one: the child is told
/// it in `x-kbf-*` headers, and nothing the client sent under those names.
pub(crate) async fn proxy_child(
    core: Arc<Core>,
    pending: Pending,
    target: Target,
    request: Request<Incoming>,
    identity: Option<Identity>,
    client: IpAddr,
) -> Response<ProxyBody> {
    let grpc = target != Target::Operator;
    let Some(count) = core.enter_stream() else {
        Metrics::inc(&core.metrics.streams_refused);
        return unavailable(grpc, "too many streams");
    };
    let (parts, body) = request.into_parts();
    let mut headers = parts.headers;
    // Strips every inbound `x-kbf-*`, then sets the secret and, with a certificate, the
    // identity and the client's address (the proxy's own reading of the connection, never a
    // header). A node id that cannot be a header value never reaches the child as one.
    if forward_headers(
        &mut headers,
        identity.as_ref(),
        Some(client),
        &core.security.secret,
    )
    .is_err()
    {
        return refused(grpc, AuthRefusal::PermissionDenied);
    }
    let path = parts.uri.path().to_owned();
    let source = Source::new(body);
    // Only a `WaitExecution` is read ahead; any other call is dispatched at its headers. A pool
    // has no old child to route it to, so nothing is read ahead there.
    let term = if core.upstreams.is_none() && is_wait_execution(&path) {
        wait_term(&source).await
    } else {
        None
    };
    let stream = Stream {
        idle: idle_of(&path),
        path,
        target,
        method: parts.method,
        uri: parts.uri,
        headers,
        source,
        activity: Activity::new(),
        count: Arc::new(count),
        grpc,
        term,
        core,
    };
    stream.run(pending).await
}

/// The term of the operation a `WaitExecution` names, read from the first message of its
/// request body (`crate::term`): `None` when that message is not whole within [`PEEK_FOR`]
/// or names nothing to route by. What was read stays in `source`, so the request is sent
/// upstream whole.
async fn wait_term(source: &Source) -> Option<u64> {
    let head = tokio::time::timeout(PEEK_FOR, source.peek(decided))
        .await
        .ok()?;
    match probe(&head) {
        Probe::Term(term) => Some(term),
        Probe::More | Probe::Skip => None,
    }
}

impl Stream {
    async fn run(&self, pending: Pending) -> Response<ProxyBody> {
        // Kept alive, never read: the stream is in a count for as long as it runs, across
        // every attempt, so it is never in no count between two permits.
        let _pending = pending;
        let mut attempts = 0;
        // The stream's first hold start, kept across attempts: a retry that is held again waits
        // out what is left of the first bound, not a new one.
        let mut since = None;
        // Pool mode: the members that failed this stream, which it is not placed on again.
        let mut failed: Vec<usize> = Vec::new();
        loop {
            let permit = match self.grant(&mut since, &failed).await {
                Ok(permit) => permit,
                Err(refusal) => return self.refuse(refusal),
            };
            let member = permit.member();
            attempts += 1;
            match self.attempt(permit, attempts).await {
                Ok(response) => return response,
                Err(failure) => {
                    self.note(member, failure, &mut failed);
                    if !self.source.may_retry(&self.path, failure, attempts) {
                        return self.refuse(Refusal::Retries);
                    }
                    if failure == Failure::Fenced {
                        // The re-dispatch is decided: the window closes for good. (A second
                        // fenced answer is refused by the attempt cap before the window matters.)
                        self.source.close_window();
                    }
                    Metrics::inc(&self.core.metrics.retries);
                }
            }
        }
    }

    fn refuse(&self, refusal: Refusal) -> Response<ProxyBody> {
        Metrics::inc(&self.core.metrics.unavailable);
        unavailable(self.grpc, refusal.message())
    }

    /// Binds the stream to a child: through the gate in slot mode, to a member of the pool that
    /// is not in `failed` in pool mode (none to take is `UNAVAILABLE`).
    async fn grant(
        &self,
        since: &mut Option<std::time::Instant>,
        failed: &[usize],
    ) -> Result<StreamPermit, Refusal> {
        match &self.core.upstreams {
            Some(members) => members
                .pick(self.target, failed)
                .map(StreamPermit::Member)
                .ok_or(Refusal::NoActive),
            None => admit(&self.core, self.term, since)
                .await
                .map(StreamPermit::Slot),
        }
    }

    /// Pool mode: the member that failed a stream is not taken for it again, and one that could
    /// not be connected to takes no stream until its probe passes. Slot mode: nothing.
    fn note(&self, member: Option<usize>, failure: Failure, failed: &mut Vec<usize>) {
        let (Some(members), Some(index)) = (&self.core.upstreams, member) else {
            return;
        };
        members.failed(index, failure == Failure::Connect);
        failed.push(index);
    }

    /// The upstream pool the permit's child is reached through.
    fn pool_of(&self, permit: &StreamPermit) -> Arc<TargetPool> {
        match permit {
            StreamPermit::Slot(permit) => {
                let (slot, generation) = (permit.slot(), permit.generation());
                self.core.pool.target(slot, generation, self.target)
            }
            StreamPermit::Member(permit) => self.core.pool.member(permit.base(), self.target),
        }
    }

    /// One dispatch. On a failure before the upstream's response head, or on a fenced head that
    /// may be dispatched again, it returns the failure.
    async fn attempt(
        &self,
        permit: StreamPermit,
        attempts: u8,
    ) -> Result<Response<ProxyBody>, Failure> {
        let pool = self.pool_of(&permit);
        let (reservation, mut send) = TargetPool::acquire(&pool).await?;
        let life = Lifetime::new(permit, reservation, Arc::clone(&self.count));
        let ended = self.source.bodiless();
        let mut upstream = Request::new(());
        *upstream.method_mut() = self.method.clone();
        *upstream.uri_mut() = self.uri.clone();
        *upstream.headers_mut() = self.headers.clone();
        *upstream.version_mut() = Version::HTTP_2;

        let mut pump_task = None;
        let head = async {
            let (response, stream) = send.send_request(upstream, ended)?;
            if !ended {
                pump_task = Some(tokio::spawn(pump(
                    Arc::clone(&self.source),
                    stream,
                    Arc::clone(&self.activity),
                    Arc::clone(&life),
                )));
            }
            response.await
        };
        let head = tokio::select! {
            head = head => head.map_err(|e| classify(&e)).and_then(|r| self.screen(r, attempts)),
            () = idle_expired(Arc::clone(&self.activity), self.idle) => Err(Failure::Other),
        };
        match head {
            Ok(response) => {
                // The upstream answered and the window is closed: the request may have had an
                // effect, or this is the fenced answer that no retry is left for.
                let (parts, recv) = response.into_parts();
                let body = Down::new(recv, life, Arc::clone(&self.activity), self.idle, self.grpc);
                let mut response = Response::from_parts(parts, ProxyBody::Up(Box::new(body)));
                strip_reserved(response.headers_mut());
                Ok(response)
            }
            Err(failure) => {
                if let Some(task) = pump_task {
                    // Wait for the pump to let go of the request and of its permit.
                    task.abort();
                    let _ = task.await;
                }
                Err(failure)
            }
        }
    }

    /// Reads the upstream's first response head before the retry window closes. A head that is
    /// not the fenced answer closes the window and goes on. The fenced answer (nothing
    /// committed) is a failure to dispatch again if [`Source::may_retry`] allows it, with the
    /// window left open for that decision; if not, the window closes and the answer goes on
    /// as the client's `UNAVAILABLE` (its marker is stripped with every other `x-kbf-*`).
    fn screen(
        &self,
        response: Response<RecvStream>,
        attempts: u8,
    ) -> Result<Response<RecvStream>, Failure> {
        let end = response.body().is_end_stream();
        if !self.source.on_first_response(response.headers(), end) {
            return Ok(response);
        }
        if self.source.may_retry(&self.path, Failure::Fenced, attempts) {
            return Err(Failure::Fenced);
        }
        self.source.close_window();
        Ok(response)
    }
}

/// How an h2 error before the response head tells the proxy whether it may dispatch again.
/// Only a GOAWAY the child sent proves the stream unprocessed (h2 fails streams above its
/// `last_stream_id` with a remote error). A GOAWAY the h2 library raised itself, on a protocol
/// error of the connection, also reaches streams the child already took.
fn classify(error: &h2::Error) -> Failure {
    if error.is_go_away() && error.is_remote() {
        Failure::GoAway
    } else if error.reason() == Some(Reason::REFUSED_STREAM) {
        Failure::RefusedStream
    } else {
        Failure::Other
    }
}

/// Binds the stream through the gate. While the gate holds it waits in the hold queue; a
/// release wakes it to bind again, and its bound answers it `UNAVAILABLE`.
///
/// A stream that names an operation of `term` binds to the slot whose child has that term if
/// that slot is open and not the active one (the old child during a swap), and otherwise to the
/// active slot. The slot is looked up at each bind, so a stream that waited out a hold binds
/// by the children as they are after it.
async fn admit(
    core: &Core,
    term: Option<u64>,
    since: &mut Option<std::time::Instant>,
) -> Result<Permit, Refusal> {
    // `since` is the first hold's start, kept across holds and across retries so chained holds
    // cannot keep one stream longer than one bound.
    loop {
        let prefer = term.and_then(|term| core.terms.slot_of(&core.slots, term));
        match core.slots.bind_preferring(prefer) {
            Bind::Bound(permit) => return Ok(permit),
            Bind::NoActive => return Err(Refusal::NoActive),
            Bind::Held { epoch, hold_max } => {
                let ticket = core.queue.admit(now(), hold_max, *since).map_err(|_| {
                    Metrics::inc(&core.metrics.hold_rejected);
                    Refusal::QueueFull
                })?;
                *since = Some(ticket.since());
                let mut bell = core.slots.subscribe();
                loop {
                    let released = core.slots.epoch() != epoch;
                    match ticket.wake(now(), released) {
                        Wake::Bind => break,
                        Wake::Expire => return Err(Refusal::Expired),
                        Wake::Wait => {
                            let deadline = tokio::time::Instant::from_std(ticket.deadline());
                            tokio::select! {
                                () = rung(&mut bell, epoch) => {}
                                () = tokio::time::sleep_until(deadline) => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Resolves when the doorbell no longer shows `epoch`: the hold the stream saw has ended.
async fn rung(bell: &mut watch::Receiver<u64>, epoch: u64) {
    // The doorbell lives as long as the gate, which the caller holds: it cannot close.
    let _ = bell.wait_for(|rung| *rung != epoch).await;
}
