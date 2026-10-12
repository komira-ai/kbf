use super::*;

/// Decodes a `filename*` value of RFC 8187 (`UTF-8''<value-chars>`), refusing any
/// byte that is neither an `attr-char` nor a `%` and two hex digits.
fn decode_ext_value(value: &str) -> Result<String, String> {
    let encoded = value
        .strip_prefix("UTF-8''")
        .ok_or_else(|| format!("{value:?}: no UTF-8'' charset"))?;
    let bytes = encoded.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            let hex = encoded
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                .ok_or_else(|| format!("{value:?}: a bad escape at {i}"))?;
            out.push(u8::from_str_radix(hex, 16).expect("hex"));
            i += 3;
        } else if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            out.push(b);
            i += 1;
        } else {
            return Err(format!("{value:?}: {:?} is not an attr-char", b as char));
        }
    }
    String::from_utf8(out).map_err(|e| format!("{value:?}: {e}"))
}

/// The `filename*` of a `Content-Disposition` of `kind` with exactly one parameter;
/// an error if it is not that.
fn filename(kind: &str, header: &HeaderValue) -> Result<String, String> {
    let text = header.to_str().map_err(|e| e.to_string())?;
    let rest = text
        .strip_prefix(kind)
        .and_then(|r| r.strip_prefix("; filename*="))
        .ok_or_else(|| format!("{text:?}: not `{kind}; filename*=`"))?;
    decode_ext_value(rest)
}

/// Catches (mutant "splice the name raw", or `filename="<name>"`): a name with a
/// quote, a `;`, a line break, a `%` or non-ASCII that ends the value early, adds a
/// parameter, or does not decode back to itself; and a disposition with a filename
/// when no name is given.
#[test]
fn a_name_is_sent_only_percent_encoded_in_filename_star() {
    assert_eq!(content_disposition("attachment", None), "attachment");
    assert_eq!(content_disposition("inline", None), "inline");
    for name in [
        "lib.a",
        "a \"quoted\"; filename=evil.html",
        "line\r\nX-Injected: 1",
        "100%.txt",
        "caf\u{e9} \u{4e2d}\u{6587}.log",
        "dir/../file",
    ] {
        for kind in ["attachment", "inline"] {
            let header = content_disposition(kind, Some(name));
            assert_eq!(filename(kind, &header).as_deref(), Ok(name), "{header:?}");
        }
    }
    assert_eq!(
        content_disposition("attachment", Some("a b;\"\u{e9}")),
        "attachment; filename*=UTF-8''a%20b%3B%22%C3%A9"
    );
}

/// Catches: the checker above accepting what it must refuse, so the test above would
/// pass a raw splice.
#[test]
fn the_filename_checker_refuses_a_raw_name() {
    let raw = HeaderValue::from_static("attachment; filename*=UTF-8''a\"b;c");
    assert!(filename("attachment", &raw).is_err());
    let quoted = HeaderValue::from_static("attachment; filename=\"a\"");
    assert!(filename("attachment", &quoted).is_err());
    assert!(decode_ext_value("utf-8''a").is_err());
    assert!(decode_ext_value("UTF-8''%4").is_err());
    assert!(decode_ext_value("UTF-8''%zz").is_err());
    assert!(decode_ext_value("UTF-8''%FF").is_err());
    assert!(
        filename(
            "attachment",
            &HeaderValue::from_bytes(b"\xff").expect("obs-text")
        )
        .is_err()
    );
}

const HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Catches: accepting a digest form other than `<hash>-<size>` (the REAPI `/` form, an
/// uppercase or short hash, a signed or missing size, trailing text), which would let
/// two spellings name one blob.
#[test]
fn a_digest_is_lowercase_hex_dash_size() {
    let d = parse_digest(&format!("{HASH}-0")).expect("a digest");
    assert_eq!((d.hash_hex(), d.size_bytes), (HASH.to_owned(), 0));
    assert_eq!(
        parse_digest(&format!("{HASH}-142")).map(|d| d.size_bytes),
        Some(142)
    );
    for bad in [
        format!("{HASH}/142"),
        format!("{}-142", HASH.to_uppercase()),
        format!("{}-142", &HASH[1..]),
        format!("{HASH}-+142"),
        format!("{HASH}-"),
        format!("{HASH}-142-1"),
        format!("{HASH}-142 "),
        HASH.to_owned(),
        String::new(),
    ] {
        assert_eq!(parse_digest(&bad), None, "{bad:?}");
    }
}

/// Catches: a query decoded other than as a browser encodes it (`+`, `%2F`), an
/// empty `name` sent as `filename*=UTF-8''`, a key given twice read either way, and a
/// typo'd key or `inline` value ignored.
#[test]
fn the_query_is_decoded_and_strict() {
    assert_eq!(Query::parse(""), Ok(Query::default()));
    assert_eq!(
        Query::parse("instance=ci%2Fmain&name=my+file%3B.txt&inline=text"),
        Ok(Query {
            instance: "ci/main".to_owned(),
            name: Some("my file;.txt".to_owned()),
            inline: true,
        })
    );
    assert_eq!(Query::parse("instance=&name="), Ok(Query::default()));
    for (bad, why) in [
        ("instance=a&instance=b", "more than once"),
        ("name=a&name=", "more than once"),
        ("inlne=text", "unknown parameter"),
        ("inline=html", "the one form is inline=text"),
        ("inline=", "the one form is inline=text"),
    ] {
        let e = Query::parse(bad).expect_err(bad);
        assert!(e.contains(why), "{bad}: {e}");
    }
}

fn range(value: &str) -> Wanted {
    wanted(Some(&HeaderValue::from_str(value).expect("a header")), 10)
}

fn part(first: u64, last: u64) -> Wanted {
    Wanted::Part { first, last }
}

/// Catches: an off-by-one at either end (the last byte is included), a suffix or
/// open range read wrong, an end past the blob not cut to it, a start at the end
/// served instead of `416`, and a malformed or multi-range header refused instead of
/// ignored (RFC 9110 lets a server ignore it; it must not fail the read).
#[test]
fn one_byte_range_is_honoured_and_the_rest_ignored() {
    assert_eq!(wanted(None, 10), Wanted::Whole);
    for (value, want) in [
        ("bytes=0-0", part(0, 0)),
        ("bytes=2-5", part(2, 5)),
        ("bytes=2-", part(2, 9)),
        ("bytes=9-9", part(9, 9)),
        ("bytes=5-100", part(5, 9)),
        ("bytes=-3", part(7, 9)),
        ("bytes=-30", part(0, 9)),
        ("BYTES = 1 - 2", part(1, 2)),
        ("bytes=10-", Wanted::Unsatisfiable),
        ("bytes=10-20", Wanted::Unsatisfiable),
        ("bytes=-0", Wanted::Unsatisfiable),
        ("bytes=5-3", Wanted::Whole),
        ("bytes=0-1,3-4", Wanted::Whole),
        ("bytes=-", Wanted::Whole),
        ("bytes=+1-2", Wanted::Whole),
        ("bytes=1-x", Wanted::Whole),
        ("bytes=-x", Wanted::Whole),
        ("bytes=99999999999999999999-", Wanted::Whole),
        ("bytes=3", Wanted::Whole),
        ("items=0-1", Wanted::Whole),
        ("0-1", Wanted::Whole),
    ] {
        assert_eq!(range(value), want, "{value}");
    }
    let empty = HeaderValue::from_static("bytes=-3");
    assert_eq!(wanted(Some(&empty), 0), Wanted::Unsatisfiable);
    let zero = HeaderValue::from_static("bytes=0-");
    assert_eq!(wanted(Some(&zero), 0), Wanted::Unsatisfiable);
    let opaque = HeaderValue::from_bytes(b"bytes=\xff-1").expect("obs-text");
    assert_eq!(wanted(Some(&opaque), 10), Wanted::Whole);
}

/// Catches: a refusal answered with the wrong HTTP status (an authentication failure
/// as 403, a denial as 404, a store failure as 500), so a client cannot tell "who are
/// you" from "not you" from "try again".
#[test]
fn refusals_keep_their_meaning() {
    for (status, code) in [
        (Status::unauthenticated("u"), StatusCode::UNAUTHORIZED),
        (Status::permission_denied("p"), StatusCode::FORBIDDEN),
        (Status::invalid_argument("i"), StatusCode::BAD_REQUEST),
        (Status::not_found("n"), StatusCode::NOT_FOUND),
        (Status::unavailable("a"), StatusCode::SERVICE_UNAVAILABLE),
        (Status::internal("x"), StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let answer = refusal(&status);
        assert_eq!(answer.status(), code, "{status:?}");
        assert_eq!(answer.headers()[CONTENT_TYPE], crate::api::JSON);
    }
}
