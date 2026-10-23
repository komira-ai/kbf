//! `Sha256Hex`: what is taken as a digest and what is refused.

use super::*;

/// A digest: 64 lowercase hex characters.
const DIGEST: &str = "d2a84f4b8b650937ec8f73cd8be2c74add5a911ba64df27458ed8229da804a26";

/// Asserts that 64 lowercase hex characters are taken as a digest, and that the digest reads
/// back as the same text, borrowed and as an owned `String`.
#[test]
fn a_digest_of_64_lowercase_hex_characters_is_taken_and_read_back() {
    let digest = Sha256Hex::try_from(DIGEST.to_owned()).expect("a digest");
    assert_eq!(digest.as_str(), DIGEST);
    assert_eq!(String::from(digest), DIGEST);
}

/// Asserts that text which is empty, one character short, one character long, upper case, or
/// holds a character outside `0-9a-f` is refused, and that the refusal names the rule.
#[test]
fn text_that_is_not_64_lowercase_hex_characters_is_refused() {
    let short = DIGEST[..63].to_owned();
    let long = format!("{DIGEST}0");
    let upper = DIGEST.to_uppercase();
    let not_hex = format!("g{}", &DIGEST[1..]);
    for bad in [&String::new(), &short, &long, &upper, &not_hex] {
        let err = Sha256Hex::try_from(bad.to_owned()).expect_err("refused");
        assert!(err.contains("lowercase hex"), "{err}");
    }
}

/// Asserts that a digest is a JSON string in both directions, and that a string which is not a
/// digest is refused when it is decoded.
#[test]
fn a_digest_is_a_json_string_that_is_checked_when_decoded() {
    let json = format!("\"{DIGEST}\"");
    let digest: Sha256Hex = serde_json::from_str(&json).expect("decodes");
    assert_eq!(digest.as_str(), DIGEST);
    assert_eq!(serde_json::to_string(&digest).expect("encodes"), json);
    assert!(serde_json::from_str::<Sha256Hex>("\"ABC\"").is_err());
}
