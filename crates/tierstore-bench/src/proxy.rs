//! The fault proxy: a loopback HTTP proxy in front of the cold store (and, for the clock-skew
//! cell, the hot store) that adds the faults the cells need without touching the store or a
//! clock: added latency per request, a bandwidth cap, refused connections, a blackhole, and
//! rewriting of `Date` and `Last-Modified` in answers.
//!
//! To fill: `spawn(config) -> ProxyHandle` serving on the loopback address, where
//! `ProxyHandle::set_mode(FaultMode)` changes the fault between passes, `request_count()` counts
//! the requests that went through (for the cells that count cold reads), and `stop()` ends it.
//! The verb `fault-proxy` is `spawn` and wait for SIGTERM. In [`FaultMode::Pass`] it adds
//! nothing: every proxied arm is run once in pass mode as a control, so the proxy's own cost is
//! known.

use std::net::SocketAddr;
use std::str::FromStr;

use crate::error::BenchError;

/// What the proxy does to a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultMode {
    /// Forward unchanged.
    Pass,
    /// Wait this many milliseconds before forwarding each request.
    Latency {
        /// Added milliseconds per request.
        ms: u64,
    },
    /// Limit the bytes per second of a request body and an answer body.
    Bandwidth {
        /// Bytes per second.
        bytes_per_sec: u64,
    },
    /// Refuse the connection.
    Refuse,
    /// Accept the connection and never answer.
    Blackhole,
    /// Shift the `Date` and `Last-Modified` of every answer by this many seconds.
    DateSkew {
        /// Seconds; negative is earlier.
        secs: i64,
    },
}

/// Why a mode was not understood.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("mode {0:?}: expected pass, refuse, blackhole, latency=MS, bandwidth=BYTES or skew=SECS")]
pub struct ModeError(String);

impl FromStr for FaultMode {
    type Err = ModeError;

    /// `pass`, `refuse`, `blackhole`, `latency=<ms>`, `bandwidth=<bytes per second>`,
    /// `skew=<seconds>`.
    fn from_str(s: &str) -> Result<Self, ModeError> {
        let bad = || ModeError(s.to_owned());
        match s.split_once('=') {
            None => match s {
                "pass" => Ok(FaultMode::Pass),
                "refuse" => Ok(FaultMode::Refuse),
                "blackhole" => Ok(FaultMode::Blackhole),
                _ => Err(bad()),
            },
            Some(("latency", n)) => Ok(FaultMode::Latency {
                ms: n.parse().map_err(|_| bad())?,
            }),
            Some(("bandwidth", n)) => Ok(FaultMode::Bandwidth {
                bytes_per_sec: n.parse().map_err(|_| bad())?,
            }),
            Some(("skew", n)) => Ok(FaultMode::DateSkew {
                secs: n.parse().map_err(|_| bad())?,
            }),
            Some(_) => Err(bad()),
        }
    }
}

/// What to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    /// The loopback address to listen on.
    pub listen: SocketAddr,
    /// The store's endpoint, `http://host:port`.
    pub upstream: String,
    /// The mode to start in.
    pub mode: FaultMode,
}

/// Serves until told to stop. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_config: ProxyConfig) -> Result<(), BenchError> {
    Err(BenchError::NotYet("fault-proxy"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a mode that does not parse back from the text a flag gives it.
    #[test]
    fn every_mode_parses_from_its_flag_text() {
        let cases = [
            ("pass", FaultMode::Pass),
            ("refuse", FaultMode::Refuse),
            ("blackhole", FaultMode::Blackhole),
            ("latency=500", FaultMode::Latency { ms: 500 }),
            (
                "bandwidth=10485760",
                FaultMode::Bandwidth {
                    bytes_per_sec: 10_485_760,
                },
            ),
            ("skew=-40", FaultMode::DateSkew { secs: -40 }),
        ];
        for (text, mode) in cases {
            assert_eq!(text.parse::<FaultMode>(), Ok(mode), "{text}");
        }
    }

    /// Catches a malformed mode being taken for a fault, which would run an arm unfaulted under
    /// a faulted label.
    #[test]
    fn a_malformed_mode_is_refused() {
        for text in [
            "",
            "slow",
            "latency",
            "latency=fast",
            "bandwidth=-1",
            "skew=1.5",
            "x=1",
        ] {
            assert_eq!(
                text.parse::<FaultMode>(),
                Err(ModeError(text.to_owned())),
                "{text}"
            );
        }
    }
}
