//! One stream to a backend: bind through the gate (waiting in the hold queue if it holds),
//! dispatch to the pool, carry the bytes both ways, and dispatch again after a safe failure.
//!
//! The permit rule (design note section 5): a stream binds to the slot active when its
//! headers arrived and keeps the permit until both upstream halves are finished.

use std::sync::Arc;

use h2::{Reason, RecvStream};
use http::{HeaderMap, Method, Request, Response, Uri, Version};
use hyper::body::Incoming;
use tokio::sync::watch;

use crate::body::{Activity, Down, Lifetime, ProxyBody, Source, idle_expired, pump};
use crate::core::{Core, StreamCount, now};
use crate::hold::Wake;
use crate::identity::strip_reserved;
use crate::metrics::Metrics;
use crate::pool::TargetPool;
use crate::replay::Failure;
use crate::reply::unavailable;
use crate::route::{Idle, Target, idle_of};
use crate::slot::{Bind, Pending, Permit};

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
}

/// Serves a request that goes to a backend. `pending` counts the stream from accept.
pub(crate) async fn proxy_child(
    core: Arc<Core>,
    pending: Pending,
    target: Target,
    request: Request<Incoming>,
) -> Response<ProxyBody> {
    let grpc = target != Target::Operator;
    let Some(count) = core.enter_stream() else {
        Metrics::inc(&core.metrics.streams_refused);
        return unavailable(grpc, "too many streams");
    };
    let (parts, body) = request.into_parts();
    let mut headers = parts.headers;
    strip_reserved(&mut headers);
    let path = parts.uri.path().to_owned();
    let stream = Stream {
        idle: idle_of(&path),
        path,
        target,
        method: parts.method,
        uri: parts.uri,
        headers,
        source: Source::new(body),
        activity: Activity::new(),
        count: Arc::new(count),
        grpc,
        core,
    };
    stream.run(pending).await
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
        loop {
            let permit = match admit(&self.core, &mut since).await {
                Ok(permit) => permit,
                Err(refusal) => return self.refuse(refusal),
            };
            attempts += 1;
            match self.attempt(permit, attempts).await {
                Ok(response) => return response,
                Err(failure) => {
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

    /// One dispatch. On a failure before the upstream's response head, or on a fenced head that
    /// may be dispatched again, it returns the failure.
    async fn attempt(&self, permit: Permit, attempts: u8) -> Result<Response<ProxyBody>, Failure> {
        let pool = self
            .core
            .pool
            .target(permit.slot(), permit.generation(), self.target);
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
async fn admit(core: &Core, since: &mut Option<std::time::Instant>) -> Result<Permit, Refusal> {
    // `since` is the first hold's start, kept across holds and across retries so chained holds
    // cannot keep one stream longer than one bound.
    loop {
        match core.slots.bind() {
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
