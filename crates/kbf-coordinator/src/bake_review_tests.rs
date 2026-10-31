//! Bake and canary, second pass: what a gap in the heartbeats does to a bake, what a silent
//! or pending host holds, and what keeps the canary gate closed.

use std::time::Duration;

use crate::bake_canary_tests::{beats, go, held, services, why};
use crate::decide::Hold;
use crate::fleet::Fleet;
use crate::testutil::beat;

/// A host that was silent for the down limit or more has not proven what it ran meanwhile:
/// when it comes back its count starts again, so a flapping host never finishes baking on
/// beats separated by gaps. Two beats, a gap of 85 s, then it needs three more.
#[test]
fn a_host_silent_past_the_down_limit_restarts_its_bake() {
    let s = services("bake_beats = 3", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    beats(&mut f, "a", &beat("2.0"), 100);
    beats(&mut f, "a", &beat("2.0"), 115);
    for (at, expect) in [
        (200, held(Hold::Baking)),
        (215, held(Hold::Baking)),
        (230, go()),
    ] {
        beats(&mut f, "a", &beat("2.0"), at);
        beats(&mut f, "b", &beat("1.0"), at);
        assert_eq!(why(&s, &f, "b", at), expect, "at {at}");
    }
}
