//! What keeps the enroll listener from being a free CPU burner
//! (`docs/design/install-and-enrollment.md`, sections 5.2 and 5.10): a request body of at
//! most [`MAX_REQUEST_BYTES`], a rate limit per source address, a cap on the calls in
//! flight, and (in `kbf-tls`'s `Incoming`) a handshake deadline.
//!
//! [`GuardLayer`] is put on the listener's server. It answers a refused call with the
//! [`RefusalReason`] status and never lets it reach the service:
//!
//! - **Rate limit.** A token bucket per source address, in memory: `burst` calls at once,
//!   refilled at `per_minute`. IPv6 sources are bucketed by their /64, an IPv4-mapped IPv6
//!   address by the IPv4 address. The table is bounded ([`MAX_SOURCES`]); sources that
//!   have spent nothing recently are dropped to make room, and when every slot is a
//!   source that is spending, a new source is refused. **It resets when the process
//!   restarts**, so a handover gives every source a full bucket again.
//! - **In flight.** At most `in_flight` calls run at once, from all sources; the next is
//!   refused with the same `rate limited`.
//! - **Body.** The request message is decoded under [`MAX_REQUEST_BYTES`] (set on the
//!   gRPC server, which checks the length the message declares before it reads the body);
//!   tonic answers a longer one `OUT_OF_RANGE`, which the guard turns into
//!   [`RefusalReason::MalformedRequest`] so a caller sees one of the closed set.
//!
//! The source address is the TCP peer. Behind an L4 proxy every call has the proxy's
//! address, and the limit is then the proxy's one bucket.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use kbf_types::RefusalReason;
use tonic::codegen::{Service, http};
use tower_layer::Layer;

use super::service::status_of;

/// The largest request body the listener decodes: a token, a node id and at most two
/// CSRs of 2 KiB each fit with room to spare.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// The most sources the rate limiter tracks at once.
pub const MAX_SOURCES: usize = 4096;

/// One token, in the micro-tokens the buckets count in.
const TOKEN: u128 = 1_000_000;

/// The numbers the guard enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Calls per minute per source, once its burst is spent.
    pub per_minute: u32,
    /// Calls a source may make at once.
    pub burst: u32,
    /// Calls in flight, from all sources.
    pub in_flight: u32,
}

impl Limits {
    /// A node enrolls once and renews every few days, so these are generous for a fleet
    /// behind one address and still bound a flood.
    pub const DEFAULT: Self = Self {
        per_minute: 60,
        burst: 30,
        in_flight: 64,
    };
}

struct Bucket {
    micro_tokens: u128,
    at: Instant,
}

/// The per-source token buckets.
pub struct RateLimiter {
    per_minute: u128,
    capacity: u128,
    sources: Mutex<BTreeMap<IpAddr, Bucket>>,
}

/// The address a bucket is kept under: an IPv6 address stands for its /64, an IPv4-mapped
/// one for the IPv4 address.
fn source_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & !u128::from(u64::MAX))),
        },
    }
}

impl RateLimiter {
    /// A limiter for `limits`.
    #[must_use]
    pub fn new(limits: &Limits) -> Self {
        Self {
            per_minute: u128::from(limits.per_minute),
            capacity: u128::from(limits.burst) * TOKEN,
            sources: Mutex::new(BTreeMap::new()),
        }
    }

    /// What `bucket` holds at `now`: what it held plus the refill since, up to a full one.
    fn refilled(&self, bucket: &Bucket, now: Instant) -> u128 {
        let elapsed = now.saturating_duration_since(bucket.at).as_micros();
        (bucket.micro_tokens + elapsed * self.per_minute / 60).min(self.capacity)
    }

    /// Whether a call from `ip` at `now` is within the limit; takes a token if so.
    pub fn admit(&self, ip: IpAddr, now: Instant) -> bool {
        let source = source_of(ip);
        let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
        if !sources.contains_key(&source) && sources.len() >= MAX_SOURCES {
            // A source whose bucket is full has spent nothing in a while: forget it.
            let full = self.capacity;
            sources.retain(|_, bucket| self.refilled(bucket, now) < full);
            if sources.len() >= MAX_SOURCES {
                return false;
            }
        }
        let bucket = sources.entry(source).or_insert(Bucket {
            micro_tokens: self.capacity,
            at: now,
        });
        let have = self.refilled(bucket, now);
        bucket.at = now;
        if have >= TOKEN {
            bucket.micro_tokens = have - TOKEN;
            true
        } else {
            bucket.micro_tokens = have;
            false
        }
    }

    /// How many sources are tracked.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.sources
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// A place in the in-flight count, given back when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    /// A place, if fewer than `cap` are taken.
    fn take(count: &Arc<AtomicUsize>, cap: usize) -> Option<Self> {
        if count.fetch_add(1, Ordering::SeqCst) >= cap {
            count.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(count)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The guard of the enroll listener's server (`Server::layer`).
#[derive(Clone)]
pub struct GuardLayer {
    limiter: Arc<RateLimiter>,
    in_flight: Arc<AtomicUsize>,
    cap: usize,
}

impl GuardLayer {
    /// A guard enforcing `limits`, with its own table of sources.
    #[must_use]
    pub fn new(limits: &Limits) -> Self {
        Self {
            limiter: Arc::new(RateLimiter::new(limits)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            cap: usize::try_from(limits.in_flight).unwrap_or(usize::MAX),
        }
    }
}

impl<S> Layer<S> for GuardLayer {
    type Service = Guard<S>;

    fn layer(&self, inner: S) -> Guard<S> {
        Guard {
            inner,
            layer: self.clone(),
        }
    }
}

/// The service [`GuardLayer`] wraps around the listener's routes.
#[derive(Clone)]
pub struct Guard<S> {
    inner: S,
    layer: GuardLayer,
}

/// The `grpc-status` header value of tonic's `OUT_OF_RANGE`, which it answers a message
/// over the decoding limit with.
const OUT_OF_RANGE: &str = "11";

/// A refusal as the trailers-only response a gRPC client reads.
fn refused<R: Default>(reason: RefusalReason) -> http::Response<R> {
    status_of(reason).into_http()
}

/// The peer's address, or the unspecified one when the connection does not tell it.
fn peer_ip<B>(request: &http::Request<B>) -> IpAddr {
    request
        .extensions()
        .get::<kbf_tls::ConnInfo>()
        .and_then(|info| info.tcp.remote_addr)
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |addr| addr.ip())
}

impl<S, B, R> Service<http::Request<B>> for Guard<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    R: Default + Send + 'static,
{
    type Response = http::Response<R>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let ip = peer_ip(&request);
        let admitted = self.layer.limiter.admit(ip, Instant::now());
        let slot = Slot::take(&self.layer.in_flight, self.layer.cap);
        let (true, Some(slot)) = (admitted, slot) else {
            tracing::info!(
                reason = RefusalReason::RateLimited.name(),
                "enrollment call refused"
            );
            return std::future::ready(Ok(refused(RefusalReason::RateLimited))).boxed();
        };
        let call = self.inner.call(request);
        async move {
            let answer = call.await;
            drop(slot);
            answer.map(|response| {
                let too_big = response
                    .headers()
                    .get("grpc-status")
                    .is_some_and(|status| status == OUT_OF_RANGE);
                if too_big {
                    refused(RefusalReason::MalformedRequest)
                } else {
                    response
                }
            })
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests;
