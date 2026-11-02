//! The `last_good` part of a confirm.

use super::request::LocalRequest;

const VERSION: &str = "0.1.7+gbbbbbbbbbbbb-3";
const GOOD: &str = "0.1.5+gaaaaaaaaaaaa-2";

/// A confirm carries the coordinator's last good release when the line names it, and none
/// otherwise.
///
/// Catches: the `last_good` part dropped, or the two versions swapped.
#[test]
fn a_confirm_may_name_the_last_good_release() {
    let line = format!("confirm kbf={VERSION} last_good={GOOD}");
    match LocalRequest::parse(&line).unwrap() {
        LocalRequest::Confirm { version, last_good } => {
            assert_eq!(version.as_str(), VERSION);
            assert_eq!(last_good.unwrap().as_str(), GOOD);
        }
        other => panic!("{other:?}"),
    }
    match LocalRequest::parse(&format!("confirm kbf={VERSION}")).unwrap() {
        LocalRequest::Confirm { last_good, .. } => assert!(last_good.is_none()),
        other => panic!("{other:?}"),
    }
}

/// A last_good that is not a version, or a line with the part misplaced or repeated, is refused:
/// nothing runs on it.
#[test]
fn a_bad_last_good_is_refused() {
    let bad = [
        format!("confirm kbf={VERSION} last_good="),
        format!("confirm kbf={VERSION} last_good=-1"),
        format!("confirm kbf={VERSION} last_good=/tmp/kbf"),
        format!("confirm kbf={VERSION} last_good={GOOD} "),
        format!("confirm kbf={VERSION}  last_good={GOOD}"),
        format!("confirm kbf={VERSION} last_good={GOOD} last_good={GOOD}"),
        format!("confirm kbf={VERSION} last_good"),
        format!("switch kbf={VERSION} last_good={GOOD}"),
    ];
    for text in bad {
        assert!(LocalRequest::parse(&text).is_err(), "{text:?}");
    }
}
