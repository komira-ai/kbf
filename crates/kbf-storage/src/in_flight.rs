//! The bytes a process holds in memory for requests in flight, and the most it has held
//! at once: the exported `kbf_bytes_in_flight` and `kbf_bytes_in_flight_peak` gauges.
//!
//! One count per process, shared by everything that buffers a body or a blob for a
//! request: the frontend's ByteStream `Write` (the receive buffer of a small blob, each
//! message of a large one) and `Read` windows, the cache's whole-blob reads, and the
//! storage node's `Write` (its buffered blob, each message in hand, the copy sent on to a
//! replica). It is the exact oracle for memory-bound tests: process RSS read from outside
//! cannot resolve a few MiB over a runtime, and a lazily committed `vec![0; declared]`
//! never raises it, but the count of a buffer starts when the buffer is allocated.
//!
//! A count is tied to the buffer, not to a code path: [`counted`] wraps bytes in an owner
//! that gives its count back when the last [`Bytes`] sharing them (a clone, a slice) is
//! dropped, so a path that keeps pieces instead of passing them on (a blob collected
//! before it is written) is counted at what it keeps. A buffer a path builds itself is
//! counted with a [`Held`] guard, which gives the bytes back when it is dropped, however
//! the request ended (finished, failed, or cancelled).
//!
//! The peak is since process start, never reset. Recording is relaxed atomics: one add
//! and one max when bytes are taken, one subtract when they are given back, per buffer
//! (per message, not per byte). [`render`] appends the Prometheus text.
//!
//! **Not counted:** the segment writer's own buffer (bounded by the write config), the
//! object store's and the write tier's internals, and the copies tonic makes encoding or
//! decoding a message. **The daemon seam:** the daemon holds bytes through the same
//! [`process`] gauge ([`counted`], [`Held`]) and has only to [`render`] it on a
//! `--metrics-listen` listener, which it does not have yet.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;

/// Bytes held now and the most held at once.
#[derive(Debug, Default)]
pub struct Gauge {
    current: AtomicU64,
    peak: AtomicU64,
}

impl Gauge {
    /// A gauge holding nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }

    /// The bytes held now.
    #[must_use]
    pub fn current(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// The most bytes held at once since the gauge was made.
    #[must_use]
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Counts `bytes` held until the returned guard is dropped.
    #[must_use = "the bytes are given back when the guard is dropped"]
    pub fn hold(&self, bytes: u64) -> Held<'_> {
        self.take(bytes);
        Held { gauge: self, bytes }
    }

    /// Starts the peak over from what is held now: for a test that measures one step in
    /// a process that has done others. The exported peak is never reset.
    pub fn reset_peak(&self) {
        self.peak.store(self.current(), Ordering::Relaxed);
    }

    fn take(&self, bytes: u64) {
        let now = self.current.fetch_add(bytes, Ordering::Relaxed) + bytes;
        // A compare-and-swap loop on most targets.
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    fn give(&self, bytes: u64) {
        self.current.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Bytes counted in a [`Gauge`] until this is dropped.
#[derive(Debug)]
pub struct Held<'a> {
    gauge: &'a Gauge,
    bytes: u64,
}

impl Held<'_> {
    /// Counts `bytes` more held.
    pub fn add(&mut self, bytes: u64) {
        self.gauge.take(bytes);
        self.bytes += bytes;
    }

    /// The bytes this guard counts.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.gauge.give(self.bytes);
    }
}

static PROCESS: Gauge = Gauge::new();

/// The process's gauge, the one `/metrics` exports.
#[must_use]
pub fn process() -> &'static Gauge {
    &PROCESS
}

/// Counts `bytes` in the process gauge until the guard is dropped.
#[must_use = "the bytes are given back when the guard is dropped"]
pub fn hold(bytes: u64) -> Held<'static> {
    PROCESS.hold(bytes)
}

/// `owner`'s bytes, counted in the process gauge from now until the last [`Bytes`] over
/// them is dropped, for a buffer whose allocation is already counted by `held` (its
/// guard moves into the owner, so nothing is counted twice).
#[must_use]
pub fn counted_with<T>(owner: T, held: Held<'static>) -> Bytes
where
    T: AsRef<[u8]> + Send + 'static,
{
    Bytes::from_owner(Counted { owner, _held: held })
}

/// `owner`'s bytes ([`counted_with`]), counted at their length.
#[must_use]
pub fn counted<T>(owner: T) -> Bytes
where
    T: AsRef<[u8]> + Send + 'static,
{
    let held = hold(owner.as_ref().len() as u64);
    counted_with(owner, held)
}

/// Bytes and the count of them; the count is given back when the bytes are freed.
struct Counted<T> {
    owner: T,
    // Dropped with `owner`.
    _held: Held<'static>,
}

impl<T: AsRef<[u8]>> AsRef<[u8]> for Counted<T> {
    fn as_ref(&self) -> &[u8] {
        self.owner.as_ref()
    }
}

/// Appends the process gauge's two families to `out` in the Prometheus text format.
pub fn render(out: &mut String) {
    render_gauge(&PROCESS, out);
}

/// Appends `gauge`'s two families to `out`.
pub fn render_gauge(gauge: &Gauge, out: &mut String) {
    let (current, peak) = (gauge.current(), gauge.peak());
    let _ = write!(
        out,
        "# HELP kbf_bytes_in_flight Bytes this process holds in memory for requests in \
         flight: receive buffers, messages in hand and read windows\n\
         # TYPE kbf_bytes_in_flight gauge\n\
         kbf_bytes_in_flight {current}\n\
         # HELP kbf_bytes_in_flight_peak The most bytes held at once since the process \
         started (kbf_bytes_in_flight, never reset)\n\
         # TYPE kbf_bytes_in_flight_peak gauge\n\
         kbf_bytes_in_flight_peak {peak}\n"
    );
}

#[cfg(test)]
mod tests;
