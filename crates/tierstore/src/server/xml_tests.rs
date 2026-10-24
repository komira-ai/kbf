//! The XML bodies of S3.

use roxmltree::Document;

use crate::Listed;

use super::testing::texts;
use super::xml::{Listing, completed, error_body, escape, initiate, parse_complete};

fn listed(key: &str, size: u64, modified: u64) -> Listed {
    Listed {
        key: key.to_owned(),
        size,
        modified,
    }
}

/// Asserts that the five characters XML gives meaning to are escaped, and no other is.
#[test]
fn text_is_escaped() {
    assert_eq!(escape("a&b<c>d\"e'f"), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    assert_eq!(escape("plain/key-1_2.3~"), "plain/key-1_2.3~");
}

/// Asserts that an error body is a well-formed document with the code, the escaped message and
/// resource, and the request id.
#[test]
fn an_error_body_names_the_code_and_escapes_the_text() {
    let xml = error_body("NoSuchKey", "no <key> & such", "/b/k&1", "00AB");
    assert_eq!(texts(&xml, "Code"), ["NoSuchKey"]);
    assert_eq!(texts(&xml, "Message"), ["no <key> & such"]);
    assert_eq!(texts(&xml, "Resource"), ["/b/k&1"]);
    assert_eq!(texts(&xml, "RequestId"), ["00AB"]);
}

/// Asserts that a page that goes on has its token, its count and `IsTruncated` true, and
/// that each object has its escaped key, size and time.
#[test]
fn a_listing_that_goes_on_carries_the_next_token() {
    let objects = [listed("a&b", 3, 0), listed("c", 0, 1_369_353_600_123)];
    let listing = Listing {
        bucket: "bkt",
        prefix: "p/",
        max_keys: 2,
        objects: &objects,
        next: Some("6322".to_owned()),
    };
    let xml = listing.to_xml();
    assert_eq!(texts(&xml, "Name"), ["bkt"]);
    assert_eq!(texts(&xml, "Prefix"), ["p/"]);
    assert_eq!(texts(&xml, "KeyCount"), ["2"]);
    assert_eq!(texts(&xml, "MaxKeys"), ["2"]);
    assert_eq!(texts(&xml, "IsTruncated"), ["true"]);
    assert_eq!(texts(&xml, "NextContinuationToken"), ["6322"]);
    assert_eq!(texts(&xml, "Key"), ["a&b", "c"]);
    assert_eq!(texts(&xml, "Size"), ["3", "0"]);
    let times = ["1970-01-01T00:00:00.000Z", "2013-05-24T00:00:00.123Z"];
    assert_eq!(texts(&xml, "LastModified"), times);
}

/// Asserts that the last page has `IsTruncated` false and no token, and that an empty page is a
/// well-formed document with no object.
#[test]
fn the_last_page_has_no_token() {
    let listing = Listing {
        bucket: "bkt",
        prefix: "",
        max_keys: 1000,
        objects: &[],
        next: None,
    };
    let xml = listing.to_xml();
    assert_eq!(texts(&xml, "IsTruncated"), ["false"]);
    assert_eq!(texts(&xml, "KeyCount"), ["0"]);
    assert!(texts(&xml, "NextContinuationToken").is_empty());
    assert!(texts(&xml, "Contents").is_empty());
}

/// Asserts that the answers of a multipart upload have the root elements that kbf's client
/// checks, the upload id, and the escaped names.
#[test]
fn the_multipart_answers_have_the_elements_a_client_reads() {
    let started = initiate("bkt", "k&1", "abc-1");
    let doc = Document::parse(&started).expect("well-formed XML");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "InitiateMultipartUploadResult");
    assert_eq!(texts(&started, "UploadId"), ["abc-1"]);
    assert_eq!(texts(&started, "Key"), ["k&1"]);
    let done = completed("bkt", "k&1", "\"d-2\"");
    let doc = Document::parse(&done).expect("well-formed XML");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "CompleteMultipartUploadResult");
    assert_eq!(texts(&done, "ETag"), ["\"d-2\""]);
    assert_eq!(texts(&done, "Location"), ["/bkt/k&1"]);
}

/// Asserts that the parts of a completion are read in their order, with the quotes taken off
/// their ETags, whether the document has the S3 namespace and white space or not.
#[test]
fn a_completion_is_read_with_or_without_the_namespace() {
    let namespaced = "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
        <Part><PartNumber>1</PartNumber><ETag>\"aa\"</ETag></Part>\
        <Part><PartNumber>2</PartNumber><ETag>\"bb\"</ETag></Part></CompleteMultipartUpload>";
    let want = vec![(1, "aa".to_owned()), (2, "bb".to_owned())];
    assert_eq!(parse_complete(namespaced.as_bytes()), Some(want));
    let spaced = "<CompleteMultipartUpload>\n <Part>\n <PartNumber> 7 </PartNumber>\n \
        <ETag> cc </ETag>\n </Part>\n</CompleteMultipartUpload>";
    let want = vec![(7, "cc".to_owned())];
    assert_eq!(parse_complete(spaced.as_bytes()), Some(want));
    let none = "<CompleteMultipartUpload></CompleteMultipartUpload>";
    assert_eq!(parse_complete(none.as_bytes()), Some(Vec::new()));
}

/// Asserts that a body that is not a completion is refused: text that is not XML or not UTF-8,
/// another root element, and a part without a number, with a number that is not one, or without
/// an ETag.
#[test]
fn a_body_that_is_not_a_completion_is_refused() {
    let part = |inner: &str| {
        format!("<CompleteMultipartUpload><Part>{inner}</Part></CompleteMultipartUpload>")
    };
    assert_eq!(parse_complete(b"not xml"), None);
    assert_eq!(parse_complete(b"\xff\xfe"), None);
    assert_eq!(parse_complete(b"<Other/>"), None);
    let no_number = part("<ETag>x</ETag>");
    assert_eq!(parse_complete(no_number.as_bytes()), None);
    let bad_number = part("<PartNumber>one</PartNumber><ETag>x</ETag>");
    assert_eq!(parse_complete(bad_number.as_bytes()), None);
    let no_etag = part("<PartNumber>1</PartNumber>");
    assert_eq!(parse_complete(no_etag.as_bytes()), None);
}
