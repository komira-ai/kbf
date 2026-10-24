//! The proxy's own counters, in Prometheus text. The proxy counts its own streams. `GET
//! /metrics` is these series followed by the series of each member of the pool
//! ([`crate::upstreams::Upstreams::render`]), so a scrape of the public port sees both.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Counters only the serving code moves.
#[derive(Debug, Default)]
pub(crate) struct Metrics {
    /// Streams that panicked: the stream was reset (or the update answered 500), nothing else.
    pub(crate) panics: AtomicU64,
    /// Streams refused at header time because `max_streams` were in flight.
    pub(crate) streams_refused: AtomicU64,
    /// Streams answered `UNAVAILABLE` by the proxy itself, for whatever reason.
    pub(crate) unavailable: AtomicU64,
    /// Upstream connections opened.
    pub(crate) upstream_dials: AtomicU64,
    /// Upstream connections that failed to open.
    pub(crate) upstream_dial_failed: AtomicU64,
    /// Streams dispatched again after a safe failure.
    pub(crate) retries: AtomicU64,
    /// Reloads of the TLS files that failed (the old configuration stayed).
    pub(crate) tls_reload_failed: AtomicU64,
    /// TLS handshakes that failed or ran out of time.
    pub(crate) handshake_failed: AtomicU64,
    /// Connections closed because a verified certificate names no usable identity.
    pub(crate) identity_refused: AtomicU64,
    /// Calls refused by the auth rules (a missing or wrong credential, a wrong role, a limit).
    pub(crate) auth_refused: AtomicU64,
}

impl Metrics {
    pub(crate) fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// The text of `GET /metrics`.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |name: &str, kind: &str, help: &str, value: u64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
            );
        };
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        line(
            "kbf_proxy_panics_total",
            "counter",
            "Streams that panicked.",
            get(&self.panics),
        );
        line(
            "kbf_proxy_streams_refused_total",
            "counter",
            "Streams refused at header time at the stream limit.",
            get(&self.streams_refused),
        );
        line(
            "kbf_proxy_unavailable_total",
            "counter",
            "Streams the proxy answered UNAVAILABLE itself.",
            get(&self.unavailable),
        );
        line(
            "kbf_proxy_upstream_dials_total",
            "counter",
            "Upstream connections opened.",
            get(&self.upstream_dials),
        );
        line(
            "kbf_proxy_upstream_dial_failed_total",
            "counter",
            "Upstream connections that failed to open.",
            get(&self.upstream_dial_failed),
        );
        line(
            "kbf_proxy_retries_total",
            "counter",
            "Streams dispatched again after a safe failure.",
            get(&self.retries),
        );
        line(
            "kbf_proxy_tls_reload_failed_total",
            "counter",
            "Reloads of the TLS files that failed; the old configuration stayed.",
            get(&self.tls_reload_failed),
        );
        line(
            "kbf_proxy_tls_handshake_failed_total",
            "counter",
            "TLS handshakes that failed or ran out of time.",
            get(&self.handshake_failed),
        );
        line(
            "kbf_proxy_identity_refused_total",
            "counter",
            "Connections closed for a certificate that names no usable identity.",
            get(&self.identity_refused),
        );
        line(
            "kbf_proxy_auth_refused_total",
            "counter",
            "Calls refused for a missing or wrong credential, a wrong role or a limit.",
            get(&self.auth_refused),
        );
        out
    }
}
