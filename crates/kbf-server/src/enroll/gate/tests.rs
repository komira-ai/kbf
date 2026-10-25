//! The start gate on deny rows (design section 8.3).

use super::*;

/// The gate opens when deny rows are enforced or the operator accepts that they are not,
/// and not otherwise.
///
/// Catches: a listener that starts, with the promise "revocation works" unkept, because
/// nobody said they accept it.
#[test]
fn the_gate_opens_only_for_enforcement_or_an_explicit_acceptance() {
    assert_eq!(check(false, false), Err(DenyNotEnforced));
    assert_eq!(check(true, false), Ok(()));
    assert_eq!(check(false, true), Ok(()));
    assert_eq!(check(true, true), Ok(()));
}

/// This build enforces deny rows, so a start without the flag is accepted, and the flag
/// changes nothing.
///
/// Catches: the marker turned off again (a build that cannot enforce a revocation), and a
/// gate that still asks for an acceptance although revocation is enforced.
#[test]
fn this_build_starts_enroll_listen_without_the_acceptance() {
    const { assert!(DENY_ENFORCED) };
    assert_eq!(start_gate(false), Ok(()));
    assert_eq!(start_gate(true), Ok(()));
}
