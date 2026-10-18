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

/// This build does not enforce deny rows, so a start without the flag is refused, with a
/// message that names the flag that accepts it.
///
/// Catches: the marker flipped without the change that makes it true (this test is
/// changed in the same commit as B3c), and an error message that does not tell the
/// operator what to do.
#[test]
fn this_build_refuses_enroll_listen_without_the_acceptance() {
    assert!(!DENY_ENFORCED, "B3c flips this and changes this test");
    let refused = start_gate(false).expect_err("refused");
    let text = refused.to_string();
    assert!(text.contains("--enroll-allow-unenforced-deny"), "{text}");
    assert!(text.contains("--enroll-listen"), "{text}");
    assert_eq!(start_gate(true), Ok(()));
}
