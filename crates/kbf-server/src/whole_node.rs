//! `--whole-node-max` and its tuning flags: whether the farm's scheduler runs
//! whole-machine requests with the whole-node drain and caps of the scheduler library
//! (`Scheduler::with_whole_node`), and with which settings.
//!
//! Off unless `--whole-node-max` is given, and then the scheduler is built as it always
//! was. The settings are the farm's, not a pool's: kbf has one scheduler per farm, and its
//! workers are not grouped into pools, so the caps count every worker that serves
//! whole-machine leases together (the scheduler library's own rule: workers of every
//! platform count toward one ceiling). The tuning flags start from the library's defaults
//! for a mixed pool, which is what a kbf farm is, one that also runs ordinary work.

use clap::builder::RangedU64ValueParser;
use kbf_sched::WholeNodeConfig;

/// The whole-node flags, part of the server's [`Args`](crate::config::Args).
#[derive(Clone, Debug, clap::Args)]
pub struct WholeNodeFlags {
    /// Turns on the whole-node drain and caps for whole-machine requests (`kbf-lease`
    /// `whole_machine`), and names the most workers that may be whole at once: held by a
    /// whole-machine lease or reserved for a queued request (a request that fits nowhere
    /// reserves the worker that will empty soonest, and nothing newer is placed there).
    /// The ceiling is also at most the workers that serve whole-machine leases less
    /// `--whole-node-keep-for-ordinary`, and falls as workers leave. Left out, the
    /// scheduler books whole-machine requests as it always has, and the other
    /// `--whole-node-*` flags are refused. A request's reservation is given up at its action's
    /// timeout, and a client may send none: without `--whole-node-drain-deadline-secs` the
    /// reservation of such a request is never given up (it ends when its worker is empty), so
    /// set that flag when clients may send no timeout
    #[arg(
        long,
        value_name = "WORKERS",
        value_parser = RangedU64ValueParser::<usize>::new().range(1..)
    )]
    pub whole_node_max: Option<usize>,
    /// The most workers reserved for a queued whole-machine request that still run other
    /// leases at once: the idle capacity a drain may cost. Default 1
    #[arg(
        long,
        value_name = "WORKERS",
        default_value_t = 1,
        hide_default_value = true,
        requires = "whole_node_max",
        value_parser = RangedU64ValueParser::<usize>::new().range(1..)
    )]
    pub whole_node_max_draining: usize,
    /// The fewest workers that carry neither a reservation nor a whole-machine lease, so
    /// ordinary work always has them. 0 lets whole-machine requests take every worker
    #[arg(
        long,
        value_name = "WORKERS",
        default_value_t = 1,
        hide_default_value = true,
        requires = "whole_node_max"
    )]
    pub whole_node_keep_for_ordinary: usize,
    /// Lets a reservation start on the last worker an older ordinary request that waits
    /// could run on. Off, the request waits for another worker, so a whole-machine request
    /// never takes the only worker such a request has
    #[arg(long, requires = "whole_node_max")]
    pub whole_node_allow_last_capable: bool,
    /// How long, in seconds, a whole-machine request waits before it reserves a worker;
    /// until then it takes only a worker with no lease. 0, the default, reserves at once
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 0,
        hide_default_value = true,
        requires = "whole_node_max"
    )]
    pub whole_node_drain_after_secs: u64,
    /// How long, in seconds, a reservation may drain its worker before it is given up
    /// (nothing running is stopped): the worker is barred to the request for
    /// `--whole-node-reserve-backoff-secs` and another is reserved. Left out, the
    /// request's longest wall time, the `timeout` of its action; an action that names
    /// none has no wall time, and without this flag its reservation is never given up
    #[arg(
        long,
        value_name = "SECONDS",
        requires = "whole_node_max",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub whole_node_drain_deadline_secs: Option<u64>,
    /// How long, in seconds, a worker stays barred to a request after the request gave
    /// its reservation up. Default 300
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 300,
        hide_default_value = true,
        requires = "whole_node_max",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub whole_node_reserve_backoff_secs: u64,
    /// The reservations a request gives up before its next one has the request's longest
    /// wall time as its deadline, and so ends in a grant. Default 2
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = 2,
        hide_default_value = true,
        requires = "whole_node_max",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub whole_node_max_reserve_attempts: u32,
}

impl WholeNodeFlags {
    /// The settings these flags name; `None` when `--whole-node-max` is left out. A setting
    /// the flags do not carry is the library's default for a mixed pool.
    #[must_use]
    pub fn config(&self) -> Option<WholeNodeConfig> {
        let max = self.whole_node_max?;
        Some(WholeNodeConfig {
            max_draining: self.whole_node_max_draining,
            keep_for_ordinary: self.whole_node_keep_for_ordinary,
            allow_last_capable: self.whole_node_allow_last_capable,
            drain_after_ms: secs_to_ms(self.whole_node_drain_after_secs),
            drain_deadline_ms: self.whole_node_drain_deadline_secs.map(secs_to_ms),
            reserve_backoff_ms: secs_to_ms(self.whole_node_reserve_backoff_secs),
            max_reserve_attempts: self.whole_node_max_reserve_attempts,
            ..WholeNodeConfig::mixed(max)
        })
    }
}

fn secs_to_ms(secs: u64) -> u64 {
    secs.saturating_mul(1_000)
}

#[cfg(test)]
mod tests;
