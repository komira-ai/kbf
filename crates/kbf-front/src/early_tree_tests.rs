use super::*;

/// Asserts that a finding the Execute made is held, and that one it dropped without making is
/// not. Catches a dropped Execute (MISSING, refused, gone) read as a held one.
#[tokio::test]
async fn a_finding_is_held_only_when_the_execute_says_so() {
    let (held, finding) = inputs_held();
    held.held();
    assert!(finding.wait().await, "the Execute found every input held");

    let (dropped, finding) = inputs_held();
    drop(dropped);
    assert!(!finding.wait().await, "an Execute that ended otherwise");
}

/// Asserts that an Execute's side whose dispatcher has dropped its own still says held without
/// failing. Catches a send to a dropped receiver that panics the Execute.
#[test]
fn saying_held_to_no_one_is_harmless() {
    let (held, finding) = inputs_held();
    drop(finding);
    held.held();
}
