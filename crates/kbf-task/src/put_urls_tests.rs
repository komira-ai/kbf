use super::*;
use kbf_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-put-urls-{name}"))
}

/// A key set of `packs` and `large` URL lists as JSON text.
fn json(packs: &[&str], large: &[&str]) -> String {
    let list = |urls: &[&str]| {
        urls.iter()
            .map(|u| format!("\"{u}\""))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(r#"{{"packs":[{}],"large":[{}]}}"#, list(packs), list(large))
}

fn refused(text: &str) -> PutUrlsError {
    PutUrls::parse(text).expect_err("accepted")
}

/// Catches the set being read in the wrong order or into the wrong list: slot i of each list
/// is the i-th URL of its array.
#[test]
fn a_key_set_parses_into_its_two_lists_in_order() {
    let urls = PutUrls::parse(&json(
        &["https://s/p0?sig=a", "http://s/p1?sig=b"],
        &["https://s/l0?sig=c"],
    ))
    .expect("parses");
    assert_eq!(urls.packs, ["https://s/p0?sig=a", "http://s/p1?sig=b"]);
    assert_eq!(urls.large, ["https://s/l0?sig=c"]);
}

/// Catches a run with no large objects being refused: an empty or an absent `large` is none.
#[test]
fn large_may_be_empty_or_absent() {
    let empty = PutUrls::parse(&json(&["https://s/p0"], &[])).expect("empty");
    assert!(empty.large.is_empty());
    let absent = PutUrls::parse(r#"{"packs":["https://s/p0"]}"#).expect("absent");
    assert!(absent.large.is_empty());
    assert_eq!(absent.packs, ["https://s/p0"]);
}

/// Catches an unknown field being ignored (a misspelt `large` would silently mean no large
/// objects).
#[test]
fn an_unknown_field_is_refused() {
    let text = r#"{"packs":["https://s/p0"],"larg":["https://s/l0"]}"#;
    assert_eq!(refused(text), PutUrlsError::UnknownField);
}

/// Catches a key set without pack 0 being accepted: the commit point needs a URL.
#[test]
fn packs_must_be_present_and_not_empty() {
    assert_eq!(refused(&json(&[], &[])), PutUrlsError::NoPacks);
    assert_eq!(
        refused(r#"{"large":[]}"#),
        PutUrlsError::Missing { field: "packs" }
    );
}

/// Catches a list longer than a manifest can name being accepted, and the limit being off by
/// one: 8 packs and 8 large are accepted, 9 of either is refused, naming the list and the limit.
#[test]
fn more_than_8_urls_are_refused() {
    let many = |n: usize| vec!["https://s/k"; n];
    PutUrls::parse(&json(&many(8), &many(8))).expect("8 each");
    assert_eq!(
        refused(&json(&many(9), &[])),
        PutUrlsError::TooMany {
            field: "packs",
            count: 9,
            max: 8
        }
    );
    let err = refused(&json(&["https://s/p0"], &many(9)));
    assert_eq!(
        err,
        PutUrlsError::TooMany {
            field: "large",
            count: 9,
            max: 8
        }
    );
    assert!(err.to_string().contains("more than the 8"), "{err}");
}

/// Catches an empty string, a URL of another scheme, a scheme with nothing after it and a URL
/// with whitespace being accepted, and names the list and slot of the bad one.
#[test]
fn a_url_must_be_an_http_url() {
    for bad in [
        "",
        "ftp://s/k",
        "s/k",
        "https:/s/k",
        "http://",
        "https://",
        "https://s/k j",
        "https://s/k\\n",
        "httpx://s",
        "héllo://s",
    ] {
        assert_eq!(
            refused(&json(&["https://s/p0", bad], &[])),
            PutUrlsError::BadUrl {
                field: "packs",
                slot: 1
            },
            "{bad:?}"
        );
        assert_eq!(
            refused(&json(&["https://s/p0"], &[bad])),
            PutUrlsError::BadUrl {
                field: "large",
                slot: 0
            },
            "{bad:?}"
        );
    }
    // The scheme is not case sensitive.
    PutUrls::parse(&json(&["HTTPS://s/p0", "Http://s/p1"], &[])).expect("case");
}

/// Catches a value of the wrong JSON type being accepted or panicking.
#[test]
fn the_wrong_json_types_are_refused() {
    assert_eq!(refused("[]"), PutUrlsError::NotAnObject);
    assert_eq!(refused("\"x\""), PutUrlsError::NotAnObject);
    assert_eq!(
        refused(r#"{"packs":"https://s/p0"}"#),
        PutUrlsError::NotStrings { field: "packs" }
    );
    assert_eq!(
        refused(r#"{"packs":["https://s/p0",7]}"#),
        PutUrlsError::NotStrings { field: "packs" }
    );
    assert_eq!(
        refused(r#"{"packs":["https://s/p0"],"large":null}"#),
        PutUrlsError::NotStrings { field: "large" }
    );
}

/// Catches text that is not JSON being accepted, and the line being wrong.
#[test]
fn text_that_is_not_json_is_refused_with_its_position() {
    let message = refused("{\n  \"packs\": [oops").to_string();
    assert!(message.contains("line 2"), "{message}");
    assert!(matches!(refused(""), PutUrlsError::Syntax { .. }));
}

/// Catches a credential reaching an error message: the URL, a wrong-typed value and an unknown
/// field name each carry the secret, and no message holds it.
#[test]
fn no_error_contains_the_input() {
    let secret = "very-secret";
    for text in [
        format!(r#"{{"packs":["ftp://{secret}"]}}"#),
        format!(r#"{{"packs":"{secret}"}}"#),
        format!(r#"{{"packs":["https://s/p0"],"{secret}":[]}}"#),
        format!(r#"{{"packs":["https://s/p0"],"large":["{secret}"]}}"#),
        format!(r#"{{"packs":[{secret}"#),
        format!(r#"["{secret}"]"#),
        format!(r#"{{"packs":["https://s/p0?{secret}", 1]}}"#),
    ] {
        let message = refused(&text).to_string();
        assert!(!message.contains(secret), "{message}");
    }
}

/// Catches the key set's Debug form printing a URL.
#[test]
fn debug_does_not_print_the_urls() {
    let urls = PutUrls::parse(&json(&["https://s/p0?sig=very-secret"], &[])).expect("parses");
    let text = format!("{urls:?}");
    assert!(!text.contains("very-secret"), "{text}");
    assert!(text.contains("packs"), "{text}");
}

/// Catches the file being ignored, deleted or read after the flag: the file wins, is left in
/// place, and a missing file is a read error naming the path and not the flag's value.
#[test]
fn the_file_is_read_and_left_in_place() {
    let dir = scratch("file");
    let path = dir.join("keys.json");
    std::fs::write(&path, json(&["https://s/from-file"], &[])).expect("write");
    let urls = PutUrls::from_sources(Some(&path), Some("not json"))
        .expect("reads")
        .expect("some");
    assert_eq!(urls.packs, ["https://s/from-file"]);
    assert!(path.exists());

    let missing = dir.join("absent.json");
    let err = PutUrls::from_sources(Some(&missing), None).unwrap_err();
    assert!(matches!(err, PutUrlsError::Read { .. }), "{err:?}");
    assert!(err.to_string().contains("absent.json"), "{err}");

    std::fs::write(&path, "{").expect("write");
    let err = PutUrls::from_sources(Some(&path), None).unwrap_err();
    assert!(matches!(err, PutUrlsError::Syntax { .. }), "{err:?}");
}

/// Catches the flag value being ignored, and "neither given" being an error: the caller falls
/// back to the v1 pack URL.
#[test]
fn the_flag_value_is_read_and_none_given_is_none() {
    let urls = PutUrls::from_sources(None, Some(&json(&["https://s/from-flag"], &[])))
        .expect("reads")
        .expect("some");
    assert_eq!(urls.packs, ["https://s/from-flag"]);
    assert_eq!(PutUrls::from_sources(None, None), Ok(None));
    assert!(PutUrls::from_sources(None, Some("{")).is_err());
}
