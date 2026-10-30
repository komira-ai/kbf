//! The grammars and tables of the OCI front: names, digests, tags, routes, ranges, manifest media
//! types, the prefix and the counters.

use hyper::header::HeaderValue;

use super::metrics::{Outcome, RouteKind};
use super::names::{Digest, Route, is_tag, route, valid_name};
use super::range::{self, Range};
use super::{OciMetrics, OciPrefix, header_value, manifest};

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Asserts that a digest is `sha256:` and 64 lowercase hex digits and nothing else, and reads
/// back as it was written.
#[test]
fn a_digest_is_sha256_and_sixty_four_lowercase_hex_digits() {
    let digest = Digest::parse(&format!("sha256:{HEX}")).expect("valid");
    assert_eq!(digest.hex(), HEX);
    assert_eq!(digest.text(), format!("sha256:{HEX}"));
    assert_eq!(digest, Digest::of_hex(HEX.to_owned()));
    let upper = format!("sha256:{}", HEX.to_uppercase());
    let short = format!("sha256:{}", &HEX[1..]);
    let long = format!("sha256:{HEX}0");
    let other = format!("sha512:{HEX}");
    let bare = HEX.to_owned();
    let not_hex = format!("sha256:{}g", &HEX[1..]);
    let dotdot = format!("sha256:../{}", &HEX[3..]);
    for bad in [
        upper,
        short,
        long,
        other,
        bare,
        not_hex,
        dotdot,
        String::new(),
    ] {
        assert_eq!(Digest::parse(&bad), None, "{bad}");
    }
}

/// Asserts the three routes are told from each other and from everything else, that a name may
/// hold slashes and a component called `blobs`, and that the segments after the name decide.
#[test]
fn paths_are_routed_by_their_last_two_segments() {
    let digest = format!("sha256:{HEX}");
    assert_eq!(route("/v2/"), Route::Ping);
    assert_eq!(
        route(&format!("/v2/library/ubuntu/manifests/{digest}")),
        Route::Manifest {
            name: "library/ubuntu",
            reference: &digest
        }
    );
    assert_eq!(
        route("/v2/a/blobs/manifests/latest"),
        Route::Manifest {
            name: "a/blobs",
            reference: "latest"
        }
    );
    assert_eq!(
        route(&format!("/v2/a/blobs/{digest}")),
        Route::Blob {
            name: "a",
            digest: &digest
        }
    );
    // An empty name is routed, and refused by the grammar.
    assert_eq!(
        route("/v2//blobs/x"),
        Route::Blob {
            name: "",
            digest: "x"
        }
    );
    for other in [
        "/",
        "/v2",
        "/v1/",
        "/v2/_catalog",
        "/v2/a/tags/list",
        "/v2/a/blobs/uploads/",
        "/v2/blobs/x",
        "/healthz",
        "",
    ] {
        assert_eq!(route(other), Route::Other, "{other}");
    }
}

/// Asserts the repository name grammar of the distribution specification.
#[test]
fn a_repository_name_is_lowercase_components_joined_by_separators() {
    let long = "a".repeat(255);
    let too_long = "a".repeat(256);
    for good in [
        "a",
        "a/b",
        "library/ubuntu",
        "a.b",
        "a_b",
        "a__b",
        "a-b",
        "a---b",
        "a0/b1.c_d__e-f",
        long.as_str(),
    ] {
        assert!(valid_name(good), "{good}");
    }
    for bad in [
        "",
        "A",
        "a/",
        "/a",
        "a//b",
        "a___b",
        "a_",
        "_a",
        "a.",
        "-a",
        "a..b",
        "a._b",
        "a_-b",
        "a b",
        "a%2fb",
        "a:b",
        "a@b",
        too_long.as_str(),
    ] {
        assert!(!valid_name(bad), "{bad}");
    }
}

/// Asserts the tag grammar: a word character first, then word characters, dots and dashes, at
/// most 128 in all.
#[test]
fn a_tag_starts_with_a_word_character() {
    let long = format!("a{}", "b".repeat(127));
    let too_long = format!("a{}", "b".repeat(128));
    for good in ["latest", "v1.2-3_x", "_x", "A", "9", long.as_str()] {
        assert!(is_tag(good), "{good}");
    }
    for bad in [
        "",
        ".x",
        "-x",
        "a:b",
        "a/b",
        "a b",
        "sha256:abc",
        too_long.as_str(),
    ] {
        assert!(!is_tag(bad), "{bad}");
    }
}

fn header(text: &'static str) -> HeaderValue {
    HeaderValue::from_static(text)
}

/// Asserts how one `Range` header is read for an object of 10 bytes.
#[test]
fn a_single_byte_range_is_read_and_clamped_to_the_object() {
    let ten = |text: &'static str| range::parse(Some(&header(text)), 10);
    let part = |start, end| Range::Partial { start, end };
    assert_eq!(range::parse(None, 10), Range::Whole);
    assert_eq!(ten("bytes=0-3"), part(0, 3));
    assert_eq!(ten("bytes=4-4"), part(4, 4));
    assert_eq!(ten("bytes=4-"), part(4, 9));
    assert_eq!(ten("bytes=4-99"), part(4, 9));
    assert_eq!(ten("bytes=-3"), part(7, 9));
    assert_eq!(ten("bytes=-99"), part(0, 9));
    assert_eq!(ten("bytes=10-"), Range::Unsatisfiable);
    assert_eq!(ten("bytes=10-12"), Range::Unsatisfiable);
    assert_eq!(ten("bytes=-0"), Range::Unsatisfiable);
    // A range that is not understood is ignored, as a server may.
    for ignored in [
        "bytes=0-1,3-4",
        "items=0-1",
        "bytes=",
        "bytes=3",
        "bytes=a-3",
        "bytes=3-b",
        "bytes=5-2",
        "bytes=-x",
        "0-3",
    ] {
        assert_eq!(ten(ignored), Range::Whole, "{ignored}");
    }
    let not_text = HeaderValue::from_bytes(&[0xff, 0xfe]).expect("opaque bytes");
    assert_eq!(range::parse(Some(&not_text), 10), Range::Whole);
}

/// Asserts that no range of an empty object holds a byte.
#[test]
fn no_range_of_an_empty_object_is_satisfiable() {
    for text in ["bytes=0-", "bytes=0-0", "bytes=-1"] {
        let value = HeaderValue::from_str(text).expect("a header");
        assert_eq!(
            range::parse(Some(&value), 0),
            Range::Unsatisfiable,
            "{text}"
        );
    }
}

/// Asserts the four manifest media types are served as declared.
#[test]
fn a_declared_manifest_media_type_is_served_as_it_is() {
    for declared in [
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
    ] {
        let body = format!(r#"{{"schemaVersion":2,"mediaType":"{declared}"}}"#);
        assert_eq!(manifest::media_type(body.as_bytes()), Ok(declared));
    }
}

/// Asserts that a manifest that says nothing, or something else, is served as an OCI manifest
/// only when it has the shape of one, as an index only when it has the shape of one, and is
/// refused otherwise.
#[test]
fn an_undeclared_manifest_is_served_by_its_shape_or_refused() {
    let shaped = r#"{"schemaVersion":2,"config":{},"layers":[]}"#;
    assert_eq!(
        manifest::media_type(shaped.as_bytes()),
        Ok(manifest::OCI_MANIFEST)
    );
    let other = r#"{"schemaVersion":2,"mediaType":"text/plain","config":{},"layers":[]}"#;
    assert_eq!(
        manifest::media_type(other.as_bytes()),
        Ok(manifest::OCI_MANIFEST)
    );
    let not_text = r#"{"schemaVersion":2,"mediaType":7,"config":{},"layers":[]}"#;
    assert_eq!(
        manifest::media_type(not_text.as_bytes()),
        Ok(manifest::OCI_MANIFEST)
    );
    let index = r#"{"schemaVersion":2,"manifests":[]}"#;
    assert_eq!(
        manifest::media_type(index.as_bytes()),
        Ok(manifest::OCI_INDEX)
    );
    for refused in [
        r#"{"schemaVersion":1,"config":{},"layers":[]}"#,
        r#"{"schemaVersion":2,"config":[],"layers":[]}"#,
        r#"{"schemaVersion":2,"config":{},"layers":{}}"#,
        r#"{"schemaVersion":2,"config":{}}"#,
        r#"{"schemaVersion":2,"manifests":{}}"#,
        r#"{"config":{},"layers":[]}"#,
        r#"{"schemaVersion":"2","config":{},"layers":[]}"#,
        "[]",
        "{}",
    ] {
        let why = manifest::media_type(refused.as_bytes()).expect_err(refused);
        assert!(why.contains("not a manifest"), "{refused}: {why}");
    }
    let why = manifest::media_type(b"\xff not json").expect_err("not JSON");
    assert!(why.contains("not JSON"), "{why}");
}

/// Asserts which prefixes are usable: a path of plain segments ending in `/`, outside what the
/// proxy keeps for itself.
#[test]
fn a_prefix_is_plain_segments_outside_the_reserved_ones() {
    for good in ["oci/", "a/b-c.d_e/", "_journal2/", "x/_journal/", "Z9/"] {
        assert_eq!(OciPrefix::new(good).expect(good).as_str(), good);
    }
    let cases = [
        ("", "end in /"),
        ("oci", "end in /"),
        ("/", "segments"),
        ("/oci/", "segments"),
        ("oci//x/", "segments"),
        ("a/../", "segments"),
        ("./", "segments"),
        ("a b/", "segments"),
        ("a%2f/", "segments"),
        ("_journal/", "overlaps"),
        ("_tiering/", "overlaps"),
        ("_journal/x/", "overlaps"),
        ("_tiering/oci/", "overlaps"),
    ];
    for (bad, why) in cases {
        let text = OciPrefix::new(bad).expect_err(bad).to_string();
        assert!(text.contains(why), "{bad:?}: {text}");
        assert!(text.contains("--oci-prefix"), "{text}");
    }
    let key = OciPrefix::new("oci/")
        .expect("valid")
        .key(&Digest::of_hex(HEX.to_owned()));
    assert_eq!(key, format!("oci/blobs/sha256/{HEX}"));
}

/// Asserts the counters count by route and outcome, add up bytes and mismatches, and render
/// every route and outcome, and that a name it does not know reads zero.
#[test]
fn the_counters_count_and_render() {
    let metrics = OciMetrics::default();
    metrics.request(RouteKind::Blob, Outcome::Ok);
    metrics.request(RouteKind::Blob, Outcome::Ok);
    metrics.request(RouteKind::Manifest, Outcome::Failed);
    metrics.request(RouteKind::Other, Outcome::NotAllowed);
    metrics.sent(7);
    metrics.sent(5);
    metrics.mismatch();
    assert_eq!(metrics.requests("blob", "ok"), 2);
    assert_eq!(metrics.requests("manifest", "failed"), 1);
    assert_eq!(metrics.requests("other", "not_allowed"), 1);
    assert_eq!(metrics.requests("ping", "ok"), 0);
    assert_eq!(metrics.requests("nothing", "ok"), 0);
    assert_eq!(metrics.requests("blob", "nothing"), 0);
    assert_eq!((metrics.bytes_sent(), metrics.mismatches()), (12, 1));
    let text = metrics.render();
    let blob_ok = r#"tierstore_oci_requests_total{route="blob",outcome="ok"} 2"#;
    let ping_ok = r#"tierstore_oci_requests_total{route="ping",outcome="ok"} 0"#;
    let ranged = r#"route="blob",outcome="range_not_satisfiable"} 0"#;
    for line in [
        blob_ok,
        ping_ok,
        ranged,
        "tierstore_oci_bytes_served_total 12\n",
        "tierstore_oci_digest_mismatches_total 1\n",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    // Four routes by seven outcomes, three counters' type lines, two totals.
    assert_eq!(text.lines().count(), 4 * 7 + 3 + 2);
}

/// Asserts a header value that cannot be one is replaced by an empty value, not a panic.
#[test]
fn text_that_is_not_a_header_value_becomes_an_empty_one() {
    assert_eq!(header_value("bytes */4"), "bytes */4");
    assert_eq!(header_value("a\nb"), "");
}
