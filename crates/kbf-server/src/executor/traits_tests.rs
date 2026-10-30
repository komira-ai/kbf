use super::RejectReason;

/// Catches a reject reason with no words (it is what an operator reads in the log when a
/// task's pack is refused), or two reasons with the same.
#[test]
fn every_reject_reason_says_something_different() {
    let all = [
        RejectReason::Missing,
        RejectReason::WrongKey,
        RejectReason::TooLarge,
        RejectReason::Malformed,
        RejectReason::HashMismatch,
        RejectReason::BadManifest,
        RejectReason::Incomplete,
        RejectReason::Expired,
        RejectReason::Claimed,
    ];
    let words: std::collections::BTreeSet<String> = all.iter().map(ToString::to_string).collect();
    assert_eq!(words.len(), all.len());
    assert!(words.iter().all(|w| !w.is_empty()));
}
