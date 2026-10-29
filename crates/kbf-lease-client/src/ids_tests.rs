use super::*;

/// Asserts that a lease id is read only in the form the server prints: a number pair with no
/// sign, no padding and no third part. It catches a parser that accepts `01-2` or `+1-2`,
/// which the server would answer with 404 for a lease that does exist under its canonical name.
#[test]
fn a_lease_id_is_read_in_the_canonical_form_only() {
    assert_eq!("3-12".parse(), Ok(LeaseId::new(3, 12)));
    assert_eq!("0-0".parse(), Ok(LeaseId::new(0, 0)));
    for text in [
        "", "3", "3-", "-3", "a-1", "1-b", "01-2", "1-02", "+1-2", "1-+2", "1-2-3", " 1-2", "1--2",
    ] {
        assert!(text.parse::<LeaseId>().is_err(), "{text:?}");
    }
    let id = LeaseId::new(4, 17);
    assert_eq!((id.term(), id.sequence()), (4, 17));
    assert_eq!(id.to_string(), "4-17");
}

/// Asserts that a lease id travels as a JSON string and that a malformed one fails to decode,
/// not to a default. It catches an id serialized as a pair or a number.
#[test]
fn a_lease_id_is_a_json_string() {
    assert_eq!(
        serde_json::to_string(&LeaseId::new(4, 17)).unwrap(),
        "\"4-17\""
    );
    assert_eq!(
        serde_json::from_str::<LeaseId>("\"4-17\"").unwrap(),
        LeaseId::new(4, 17)
    );
    assert!(serde_json::from_str::<LeaseId>("\"4/17\"").is_err());
    assert!(serde_json::from_str::<LeaseId>("417").is_err());
}

/// Asserts that a run id is refused when it could change the path it is put in: a slash, a
/// dot segment, a query or fragment character, whitespace, nothing, or too long. It catches
/// each condition of the check being dropped, one by one.
#[test]
fn a_run_id_is_always_one_safe_path_segment() {
    for text in ["run-4-1", "a", "A.b_c-9", &"x".repeat(RunId::MAX_LEN)] {
        assert_eq!(text.parse::<RunId>().unwrap().as_str(), text);
    }
    let too_long = "x".repeat(RunId::MAX_LEN + 1);
    for text in [
        "", ".", "..", "a/b", "a?b", "a#b", "a b", "a%2fb", "a\nb", "é", &too_long,
    ] {
        assert!(text.parse::<RunId>().is_err(), "{text:?}");
    }
    assert_eq!("run-4-1".parse::<RunId>().unwrap().to_string(), "run-4-1");
}

/// Asserts that a run id decodes through the same check, so a server (or something posing as
/// one) cannot hand back an id that later escapes a path.
#[test]
fn a_run_id_from_the_wire_is_checked_too() {
    let id: RunId = serde_json::from_str("\"run-4-1\"").unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"run-4-1\"");
    assert!(serde_json::from_str::<RunId>("\"../x\"").is_err());
    assert!(serde_json::from_str::<RunId>("1").is_err());
}
