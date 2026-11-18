//! The deadline of a client's upload into the packs: the `grpc-timeout` the client sent, less a
//! margin, from when the handler began.
//!
//! A write to the store waits out the store's `SlowDown` and waits for one of the frontend's
//! store-write slots (`crate::store_writes`), for as long as it takes. What bounds that is the
//! client: an upload whose deadline passes is ended DEADLINE_EXCEEDED, which drops its write,
//! its wait and its slot, so back-pressure is latency for the client and never UNAVAILABLE.
//! A call without a `grpc-timeout` has no deadline and waits for the store.
//!
//! The handler ends the call before the client's own deadline, by [`margin`]: the server's
//! transport ends a call whose `grpc-timeout` has passed as CANCELLED, and the handler's
//! answer must reach the client before that.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tonic::Status;
use tonic::codegen::http::HeaderMap;
use tonic::metadata::MetadataMap;

/// The header a gRPC client names its deadline in.
const GRPC_TIMEOUT: &str = "grpc-timeout";

/// The most a handler ends a call before the client's deadline.
pub(crate) const MOST_MARGIN: Duration = Duration::from_secs(1);

/// The most digits of a `grpc-timeout` value (the gRPC specification's `TimeoutValue`).
const MOST_DIGITS: usize = 8;

/// The deadline of a call whose request carries `metadata`, counted from now.
pub(crate) fn of_metadata(metadata: &MetadataMap) -> Option<Instant> {
    let value = metadata.get(GRPC_TIMEOUT)?;
    after(parse(value.as_bytes())?)
}

/// The deadline of a call whose request carries `headers`, counted from now.
pub(crate) fn of_headers(headers: &HeaderMap) -> Option<Instant> {
    let value = headers.get(GRPC_TIMEOUT)?;
    after(parse(value.as_bytes())?)
}

/// The time the handler of a call with the timeout `timeout` ends it: now, and the timeout less
/// its [`margin`]. `None` for a timeout too long for the clock (no deadline).
fn after(timeout: Duration) -> Option<Instant> {
    Instant::now().checked_add(timeout - margin(timeout))
}

/// How long before a timeout of `timeout` the handler ends the call: a tenth of it, at most
/// [`MOST_MARGIN`].
pub(crate) fn margin(timeout: Duration) -> Duration {
    (timeout / 10).min(MOST_MARGIN)
}

/// The timeout a `grpc-timeout` value names: one to eight digits and a unit, `H`, `M`, `S`, `m`
/// (milliseconds), `u` (microseconds) or `n` (nanoseconds). `None` for any other value.
pub(crate) fn parse(value: &[u8]) -> Option<Duration> {
    let (&unit, digits) = value.split_last()?;
    if digits.is_empty() || digits.len() > MOST_DIGITS || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Eight digits at most: the number fits a u64, and so does it in hours, as seconds.
    let count = digits
        .iter()
        .fold(0_u64, |n, d| n * 10 + u64::from(d - b'0'));
    match unit {
        b'H' => Some(Duration::from_secs(count * 3600)),
        b'M' => Some(Duration::from_secs(count * 60)),
        b'S' => Some(Duration::from_secs(count)),
        b'm' => Some(Duration::from_millis(count)),
        b'u' => Some(Duration::from_micros(count)),
        b'n' => Some(Duration::from_nanos(count)),
        _ => None,
    }
}

/// The answer of `work`, or DEADLINE_EXCEEDED when `deadline` passes first (which drops
/// `work`, and with it any write it has at the store). With no deadline, `work` runs to its end.
///
/// # Errors
/// DEADLINE_EXCEEDED, or the error of `work`.
pub(crate) async fn within<T>(
    deadline: Option<Instant>,
    work: impl Future<Output = Result<T, Status>>,
) -> Result<T, Status> {
    let Some(at) = deadline else {
        return work.await;
    };
    match tokio::time::timeout_at(at, work).await {
        Ok(answer) => answer,
        Err(_) => {
            tracing::warn!("an upload's deadline passed before the store took its blobs: ended");
            Err(Status::deadline_exceeded(
                "the call's deadline (grpc-timeout) passed before the store took its blobs",
            ))
        }
    }
}

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod tests;
