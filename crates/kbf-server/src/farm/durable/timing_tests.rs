//! The timing constants this module keeps for the store layer it still runs, against the same
//! constants of `js-store`, which the scheduler's rows will be written by.

use kbf_sched::fence::{LEADER_LEASE_MARGIN, SELF_FENCE};

use super::owner::{ROW_LIFE, RUN_LIFE};
use super::*;

/// Asserts that each timing constant of this module equals the one of `js-store`, and that the
/// two parts of the acknowledgement window are the sums `js-store` defines them as (it keeps
/// them private, so its window is the check).
///
/// Catches: a copy that drifts from the crate that will write the rows: a session row that
/// lives for one length in one store layer and another in the other, an acknowledgement
/// window the orphan sweep of one does not match, or a `done/` row kept for a different time.
#[test]
fn the_timing_constants_equal_those_of_js_store() {
    assert_eq!(ACK_WINDOW, js_store::ACK_WINDOW);
    assert_eq!(SESSION_RENEWAL, js_store::SESSION_RENEWAL);
    assert_eq!(DONE_FLOOR, js_store::DONE_FLOOR);
    assert_eq!(ORPHAN_WAIT, js_store::ORPHAN_WAIT);
    assert_eq!(OWNER_CACHE_TTL, js_store::OWNER_CACHE_TTL);
    assert_eq!(ID_BLOCK, js_store::ID_BLOCK);
    assert_eq!(ROW_LIFE, LEASE_GRACE + js_store::SESSION_RENEWAL);
    assert_eq!(RUN_LIFE, SELF_FENCE + 2 * LEADER_LEASE_MARGIN);
    assert_eq!(ROW_LIFE - RUN_LIFE, js_store::ACK_WINDOW);
}
