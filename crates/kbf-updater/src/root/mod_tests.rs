//! The root part's own helpers, in this process.

use super::kept_after;

/// Catches a refusal that hides what failed: when an order's run failed and the declaration it
/// installed could not be put back, the refusal is `internal` and says both, the run's token and
/// reason first.
#[test]
fn a_declaration_that_could_not_be_put_back_is_said_with_the_run_that_failed() {
    assert_eq!(
        kept_after("runner", "the record is gone", "disk full"),
        (
            "internal",
            "runner: the record is gone; the declaration it installed is kept: disk full"
                .to_owned()
        )
    );
}
