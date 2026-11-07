//! The timing constants of this module, against the values the rows written before this
//! module owned them were timed by.

use kbf_sched::fence::{LEADER_LEASE_MARGIN, SELF_FENCE};

use super::owner::{ROW_LIFE, RUN_LIFE};
use super::*;

/// The values `js_store` had for these constants. The rows the js core wrote were timed by
/// them, so the store layer here, which reads those rows, keeps the same lengths.
const JS_STORE_ACK_WINDOW: Duration = Duration::from_secs(30);
const JS_STORE_SESSION_RENEWAL: Duration = Duration::from_secs(20);
const JS_STORE_DONE_FLOOR: Duration = Duration::from_secs(600);
const JS_STORE_ORPHAN_WAIT: Duration = Duration::from_secs(25);
const JS_STORE_OWNER_CACHE_TTL: Duration = Duration::from_secs(2);
const JS_STORE_ID_BLOCK: u64 = 1024;

/// Asserts that each timing constant of this module equals the value `js_store` had, and that
/// the two parts of the acknowledgement window sum to it.
///
/// Catches: a constant that drifts from the lengths rows already in a store were written
/// with: a session row that lives for another length, an acknowledgement window the orphan
/// sweep does not match, or a `done/` row kept for a different time.
#[test]
fn the_timing_constants_equal_those_of_js_store() {
    assert_eq!(ACK_WINDOW, JS_STORE_ACK_WINDOW);
    assert_eq!(SESSION_RENEWAL, JS_STORE_SESSION_RENEWAL);
    assert_eq!(DONE_FLOOR, JS_STORE_DONE_FLOOR);
    assert_eq!(ORPHAN_WAIT, JS_STORE_ORPHAN_WAIT);
    assert_eq!(OWNER_CACHE_TTL, JS_STORE_OWNER_CACHE_TTL);
    assert_eq!(ID_BLOCK, JS_STORE_ID_BLOCK);
    assert_eq!(ROW_LIFE, LEASE_GRACE + JS_STORE_SESSION_RENEWAL);
    assert_eq!(RUN_LIFE, SELF_FENCE + 2 * LEADER_LEASE_MARGIN);
    assert_eq!(ROW_LIFE - RUN_LIFE, JS_STORE_ACK_WINDOW);
}
