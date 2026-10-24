//! One stream to a backend: place it on a member of the pool, dispatch to the member's
//! connection, carry the bytes both ways, and place it again after a safe failure.
//!
//! A stream is placed on a member of the pool, for as long as it lives, and a failure that is
//! safe to try again (a refused connection, a refused stream, a GOAWAY) places it again on a
//! member other than those that failed it. The permit that holds the stream's place on the
//! member is kept until both upstream halves are finished.

use std::net::IpAddr;
use std::sync::Arc;

use h2::Reason;
use http::{HeaderMap, Method, Request, Response, Uri, Version};
use hyper::body::Incoming;

use crate::auth::Refusal as AuthRefusal;
use crate::body::{Activity, Down, Lifetime, ProxyBody, Source, idle_expired, pump};
use crate::core::{Core, Pending, StreamCount};
use crate::identity::{Identity, forward_headers, strip_reserved};
use crate::metrics::Metrics;
use crate::pool::TargetPool;
use crate::replay::Failure;
use crate::reply::{refused, unavailable};
use crate::route::{Idle, Target, idle_of};
use crate::upstreams::MemberPermit;

/// Why a stream was refused before it reached a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    NoActive,
    Retries,
}

impl Refusal {
    const fn message(self) -> &'static str {
        match self {
            Self::NoActive => "no frontend is active",
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
        // The members that failed this stream, which it is not placed on again.
        let mut failed: Vec<usize> = Vec::new();
        loop {
            let Some(permit) = self.core.upstreams.pick(self.target, &failed) else {
                return self.refuse(Refusal::NoActive);
            };
            let member = permit.index();
            attempts += 1;
            match self.attempt(permit).await {
                Ok(response) => return response,
                Err(failure) => {
                    self.note(member, failure, &mut failed);
                    if !self.source.may_retry(&self.path, failure, attempts) {
                        return self.refuse(Refusal::Retries);
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

    /// The member that failed a stream is not taken for it again, and one that could not be
    /// connected to takes no stream until its probe passes.
    fn note(&self, member: usize, failure: Failure, failed: &mut Vec<usize>) {
        self.core
            .upstreams
            .failed(member, failure == Failure::Connect);
        failed.push(member);
    }

    /// One dispatch. On a failure before the upstream's response head it returns the failure.
    async fn attempt(&self, permit: MemberPermit) -> Result<Response<ProxyBody>, Failure> {
        let pool = self.core.pool.member(permit.base(), self.target);
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
            head = head => head.map_err(|e| classify(&e)),
            () = idle_expired(Arc::clone(&self.activity), self.idle) => Err(Failure::Other),
        };
        match head {
            Ok(response) => {
                // The upstream answered: the request may have had an effect, so it is never
                // dispatched again.
                self.source.close_window();
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
