//! The most HTTP/2 streams one connection may have open on each listener: the
//! `--reapi-max-streams` and `--worker-max-streams` flags and the limits they become.
//!
//! The two listeners differ. A REAPI client (buck2) holds one `Execute` stream open per
//! outstanding action, idle almost the whole time, so its backlog must not wait in the
//! client behind a small cap. A daemon holds one persistent session stream and a few
//! short, busy transfer streams. The connection's flow-control window does not follow
//! either limit ([`kbf_front::CONNECTION_WINDOW_BYTES`]).

use kbf_front::{DEFAULT_REAPI_STREAMS, DEFAULT_WORKER_STREAMS};

/// The stream limits of the two listeners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    /// The most streams one connection to the REAPI listener may have open.
    pub reapi: u32,
    /// The most streams one connection to the worker listener may have open.
    pub worker: u32,
}

impl StreamLimits {
    /// The defaults of `--reapi-max-streams` and `--worker-max-streams`.
    pub const DEFAULT: Self = Self {
        reapi: DEFAULT_REAPI_STREAMS,
        worker: DEFAULT_WORKER_STREAMS,
    };
}

/// The stream-limit flags, part of the server's [`Args`](crate::config::Args).
#[derive(Clone, Copy, Debug, clap::Args)]
pub struct StreamArgs {
    /// The most HTTP/2 streams one connection to the REAPI listener may have open.
    /// buck2 keeps one Execute stream open per outstanding action, idle until the
    /// action finishes, so this bounds how large a backlog one buck2 invocation can
    /// show the farm. It does not change the connection's flow-control window, which
    /// is fixed (200 MiB). At least 1.
    #[arg(
        long,
        default_value_t = DEFAULT_REAPI_STREAMS,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub reapi_max_streams: u32,
    /// The most HTTP/2 streams one connection to the worker listener may have open: a
    /// daemon's persistent session stream and its short transfer streams. At least 1.
    #[arg(
        long,
        default_value_t = DEFAULT_WORKER_STREAMS,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub worker_max_streams: u32,
}

impl StreamArgs {
    /// The limits these flags name.
    pub fn limits(&self) -> StreamLimits {
        StreamLimits {
            reapi: self.reapi_max_streams,
            worker: self.worker_max_streams,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::StreamLimits;
    use crate::config::Args;

    /// Catches: a default other than 10000 for the REAPI listener or 200 for the
    /// worker listener; a flag that does not reach `Listeners` (mutant: `limits()`
    /// swapping the two, or returning the defaults); the flags crossed.
    #[test]
    fn the_flags_reach_the_listeners() {
        let defaults = Args::parse_from(["kbf-server"])
            .listeners()
            .expect("listeners");
        assert_eq!(defaults.streams, StreamLimits::DEFAULT);
        assert_eq!(defaults.streams.reapi, 10_000);
        assert_eq!(defaults.streams.worker, 200);
        let set = Args::parse_from([
            "kbf-server",
            "--reapi-max-streams=64000",
            "--worker-max-streams=17",
        ])
        .listeners()
        .expect("listeners");
        assert_eq!(
            set.streams,
            StreamLimits {
                reapi: 64_000,
                worker: 17
            }
        );
        let one = Args::parse_from(["kbf-server", "--worker-max-streams=3"])
            .listeners()
            .expect("listeners");
        assert_eq!(one.streams.reapi, 10_000, "an unset flag keeps its default");
        assert_eq!(one.streams.worker, 3);
    }

    /// Catches: a limit of 0 (no stream could ever open) or a value that is not a
    /// number accepted, on either flag.
    #[test]
    fn a_limit_below_one_is_refused() {
        for refused in [
            "--reapi-max-streams=0",
            "--worker-max-streams=0",
            "--reapi-max-streams=-1",
            "--worker-max-streams=many",
            "--reapi-max-streams=4294967296",
        ] {
            assert!(
                Args::try_parse_from(["kbf-server", refused]).is_err(),
                "{refused} accepted"
            );
        }
    }
}
