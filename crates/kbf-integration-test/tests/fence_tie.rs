//! The daemon's fence time and the scheduler's T are one number.
//!
//! The scheduler re-dispatches a worker's leases once it has heard nothing from the
//! worker for G, and its safety argument (`kbf_sched::fence`, `T + margin < G`) takes T
//! from `kbf_sched::fence::SELF_FENCE`. The daemon stops its self-fenced leases after
//! `kbf_daemon::config::FENCE_AFTER`. The two are separate literals, because `kbf-daemon`
//! does not depend on `kbf-sched` (`tests/layering.rs`). If the daemon's grew past the
//! scheduler's, the scheduler could place a lease again while the old daemon still ran
//! it. This crate links both, so it is where they are held equal.

use kbf_daemon::config::FENCE_AFTER;
use kbf_sched::fence::SELF_FENCE;

// The same pin at build time: a changed constant stops the crate compiling, not only
// its test failing.
const _: () = assert!(FENCE_AFTER.as_millis() == SELF_FENCE.as_millis());

/// Catches: `FENCE_AFTER` changed without `SELF_FENCE`, or the reverse. The scheduler's
/// safety condition is proved for `SELF_FENCE`; a daemon that fences later than that
/// runs a lease beside its re-dispatch, and one that fences earlier gives up work the
/// scheduler still counts as running.
#[test]
fn the_daemons_fence_time_is_the_schedulers_t() {
    assert_eq!(FENCE_AFTER, SELF_FENCE);
}
