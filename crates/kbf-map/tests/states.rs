//! The names an operator reads for a member's state.

use kbf_map::MemberState;

// Catches: a state shown under another state's name, or under a name that is not the one the
// operator API and the metrics documents (`GET /v1/storage` shows it, and a script matches on
// it). Every state is named once, in lower case with underscores.
#[test]
fn every_state_has_its_own_operator_name() {
    let names = [
        (MemberState::Joining, "joining"),
        (MemberState::CatchingUp, "catching_up"),
        (MemberState::Active, "active"),
        (MemberState::Draining, "draining"),
        (MemberState::Down, "down"),
    ];
    for (state, name) in names {
        assert_eq!(state.name(), name, "{state:?}");
    }
}

/// Asserts that a state this build does not name is shown as `unknown`, whatever number it
/// came with, so an operator sees that the member is in a state this build cannot read.
#[test]
fn a_state_this_build_does_not_name_is_shown_as_unknown() {
    for number in [0, 6, 99, -1] {
        let state = MemberState::Unknown(number);
        assert_eq!(state.name(), "unknown", "{number}");
    }
}
