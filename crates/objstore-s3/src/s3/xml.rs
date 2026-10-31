//! The S3 XML bodies kbf reads: `ListBucketResult` (ListObjectsV2), `Error`, and the
//! answers of a multipart upload; and the one it writes, `CompleteMultipartUpload`.

use roxmltree::{Document, Node};

use super::parts::{OpenUpload, UploadsPage};
use super::tagged::{StampedObject, StampedPage};
use super::times;
use crate::types::Token;
use crate::{ListPage, ListToken, ObjectInfo, ObjectKey, ObjectStoreError};

/// The code and message of an S3 `<Error>` body; empty when the body is not one (a HEAD
/// answer has no body).
pub(crate) fn error_code_message(body: &[u8]) -> (String, String) {
    let Ok(text) = std::str::from_utf8(body) else {
        return (String::new(), String::new());
    };
    let Ok(doc) = Document::parse(text) else {
        return (String::new(), text.chars().take(200).collect());
    };
    let root = doc.root_element();
    (
        child_text(root, "Code").unwrap_or_default().to_owned(),
        child_text(root, "Message").unwrap_or_default().to_owned(),
    )
}

/// An object of a ListObjectsV2 answer, with the text of the fields only a stamped listing
/// reads.
struct Entry {
    key: ObjectKey,
    size: u64,
    modified: Option<String>,
}

/// A ListObjectsV2 answer as a [`ListPage`].
pub(crate) fn list_page(body: &[u8]) -> Result<ListPage, ObjectStoreError> {
    let (entries, next) = list_result(body)?;
    let objects = entries
        .into_iter()
        .map(|entry| ObjectInfo {
            key: entry.key,
            size: entry.size,
        })
        .collect();
    Ok(ListPage { objects, next })
}

/// A ListObjectsV2 answer as a [`StampedPage`]: every object must carry a readable
/// `<LastModified>`.
pub(crate) fn stamped_page(body: &[u8]) -> Result<StampedPage, ObjectStoreError> {
    let (entries, next) = list_result(body)?;
    let mut objects = Vec::with_capacity(entries.len());
    for entry in entries {
        let modified = entry.modified.as_deref().and_then(times::iso8601_millis);
        let modified_millis = modified.ok_or_else(|| {
            ObjectStoreError::Protocol(format!(
                "ListObjectsV2 answer: {}: missing or bad <LastModified>",
                entry.key
            ))
        })?;
        objects.push(StampedObject {
            key: entry.key,
            size: entry.size,
            modified_millis,
        });
    }
    Ok(StampedPage { objects, next })
}

/// The objects and the next token of a ListObjectsV2 answer.
fn list_result(body: &[u8]) -> Result<(Vec<Entry>, Option<ListToken>), ObjectStoreError> {
    let bad = |what: String| ObjectStoreError::Protocol(format!("ListObjectsV2 answer: {what}"));
    let text = std::str::from_utf8(body).map_err(|e| bad(e.to_string()))?;
    let doc = Document::parse(text).map_err(|e| bad(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(bad(format!("root element is <{}>", root.tag_name().name())));
    }
    let mut entries = Vec::new();
    for c in root.children().filter(|n| is_named(*n, "Contents")) {
        let key = child_text(c, "Key").ok_or_else(|| bad("<Contents> without <Key>".into()))?;
        let key = ObjectKey::new(key).map_err(|e| bad(format!("listed key: {e}")))?;
        let size = child_text(c, "Size")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad(format!("{key}: missing or bad <Size>")))?;
        let modified = child_text(c, "LastModified").map(str::to_owned);
        entries.push(Entry {
            key,
            size,
            modified,
        });
    }
    let truncated = match child_text(root, "IsTruncated") {
        Some("true") => true,
        Some("false") | None => false,
        Some(other) => return Err(bad(format!("<IsTruncated> is {other:?}"))),
    };
    let next = match (truncated, child_text(root, "NextContinuationToken")) {
        (true, Some(t)) if !t.is_empty() => Some(ListToken(Token::Continue(t.to_owned()))),
        (true, _) => return Err(bad("truncated without <NextContinuationToken>".into())),
        (false, _) => None,
    };
    Ok((entries, next))
}

/// The `UploadId` of a CreateMultipartUpload answer.
pub(crate) fn upload_id(body: &[u8]) -> Result<String, ObjectStoreError> {
    let bad =
        |what: String| ObjectStoreError::Protocol(format!("CreateMultipartUpload answer: {what}"));
    let text = std::str::from_utf8(body).map_err(|e| bad(e.to_string()))?;
    let doc = Document::parse(text).map_err(|e| bad(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "InitiateMultipartUploadResult" {
        return Err(bad(format!("root element is <{}>", root.tag_name().name())));
    }
    match child_text(root, "UploadId") {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(bad("no <UploadId>".into())),
    }
}

/// A ListMultipartUploads answer as an [`UploadsPage`]: every upload must carry a key, an
/// id and a readable `<Initiated>`, and a truncated answer must say where it goes on.
pub(crate) fn uploads_page(body: &[u8]) -> Result<UploadsPage, ObjectStoreError> {
    let bad =
        |what: String| ObjectStoreError::Protocol(format!("ListMultipartUploads answer: {what}"));
    let text = std::str::from_utf8(body).map_err(|e| bad(e.to_string()))?;
    let doc = Document::parse(text).map_err(|e| bad(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListMultipartUploadsResult" {
        return Err(bad(format!("root element is <{}>", root.tag_name().name())));
    }
    let mut uploads = Vec::new();
    for entry in root.children().filter(|n| is_named(*n, "Upload")) {
        let key = child_text(entry, "Key").ok_or_else(|| bad("<Upload> without <Key>".into()))?;
        let key = ObjectKey::new(key).map_err(|e| bad(format!("listed key: {e}")))?;
        let id = child_text(entry, "UploadId").filter(|id| !id.is_empty());
        let id = id.ok_or_else(|| bad(format!("{key}: no <UploadId>")))?;
        let initiated = child_text(entry, "Initiated").and_then(times::iso8601_millis);
        let initiated_millis =
            initiated.ok_or_else(|| bad(format!("{key}: missing or bad <Initiated>")))?;
        uploads.push(OpenUpload {
            key,
            upload_id: id.to_owned(),
            initiated_millis,
        });
    }
    let truncated = match child_text(root, "IsTruncated") {
        Some("true") => true,
        Some("false") | None => false,
        Some(other) => return Err(bad(format!("<IsTruncated> is {other:?}"))),
    };
    let next = if truncated {
        let key = child_text(root, "NextKeyMarker").filter(|k| !k.is_empty());
        let key = key.ok_or_else(|| bad("truncated without <NextKeyMarker>".into()))?;
        let key = ObjectKey::new(key).map_err(|e| bad(format!("next key marker: {e}")))?;
        let id = child_text(root, "NextUploadIdMarker").unwrap_or_default();
        Some((key, id.to_owned()))
    } else {
        None
    };
    Ok(UploadsPage { uploads, next })
}

/// The body of CompleteMultipartUpload: each part's number and ETag, in order.
pub(crate) fn complete_request(etags: &[String]) -> String {
    complete_document((1..).zip(etags.iter().map(String::as_str)))
}

/// The body of CompleteMultipartUpload for parts a caller numbers itself: each part's number
/// and ETag, in the order given.
pub(crate) fn complete_parts_request(parts: &[(u32, String)]) -> String {
    complete_document(parts.iter().map(numbered))
}

/// A part a caller numbered, as a number and an ETag.
fn numbered(part: &(u32, String)) -> (u64, &str) {
    (u64::from(part.0), part.1.as_str())
}

/// A CompleteMultipartUpload document naming these parts, a number and an ETag each.
fn complete_document<'a>(parts: impl Iterator<Item = (u64, &'a str)>) -> String {
    let mut out =
        String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for (number, etag) in parts {
        out.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
            escape(etag)
        ));
    }
    out.push_str("</CompleteMultipartUpload>");
    out
}

/// A CompleteMultipartUpload answer of 200: success only if it is a
/// `CompleteMultipartUploadResult`, and then the `ETag` of the object it names, quotes
/// included, if it names one. S3 answers 200 and then an `<Error>` when the completion fails
/// after the answer started.
pub(crate) fn complete_answer(body: &[u8]) -> Result<Option<String>, ObjectStoreError> {
    let text = std::str::from_utf8(body).unwrap_or_default();
    let doc = Document::parse(text).ok();
    let root = doc.as_ref().map(Document::root_element);
    let name = root.map(|root| root.tag_name().name().to_owned());
    match name.as_deref() {
        Some("CompleteMultipartUploadResult") => {
            let etag = root.and_then(|root| child_text(root, "ETag"));
            Ok(etag.map(|etag| etag.trim().to_owned()))
        }
        Some("Error") => {
            let (code, message) = error_code_message(body);
            Err(ObjectStoreError::Service {
                status: 200,
                code,
                message,
            })
        }
        _ => Err(ObjectStoreError::Protocol(format!(
            "CompleteMultipartUpload answered 200 with {:?}",
            text.chars().take(200).collect::<String>()
        ))),
    }
}

/// `text` with the characters XML gives meaning to escaped.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Whether `node` is an element named `name`, in any namespace.
fn is_named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|n| is_named(*n, name))
        .map(|n| n.text().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns="http://s3.amazonaws.com/doc/2006-03-01/""#;

    /// Catches: a parser that drops the continuation token (so listing stops after one
    /// page), loses a key or size, or depends on the S3 namespace being absent.
    #[test]
    fn truncated_page() {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult {NS}>\
             <Name>b</Name><Prefix>p/</Prefix><KeyCount>2</KeyCount><MaxKeys>2</MaxKeys>\
             <IsTruncated>true</IsTruncated><NextContinuationToken>tok/+=</NextContinuationToken>\
             <Contents><Key>p/a</Key><Size>3</Size><ETag>\"x\"</ETag></Contents>\
             <Contents><Key>p/b</Key><Size>0</Size></Contents></ListBucketResult>"
        );
        let page = list_page(body.as_bytes()).unwrap();
        assert_eq!(page.next, Some(ListToken(Token::Continue("tok/+=".into()))));
        let got: Vec<_> = page
            .objects
            .iter()
            .map(|o| (o.key.as_str(), o.size))
            .collect();
        assert_eq!(got, [("p/a", 3), ("p/b", 0)]);
    }

    /// Catches: a last page read as truncated, an empty listing refused, and a
    /// truncated answer with no token accepted (which would end a listing early).
    #[test]
    fn last_empty_and_broken_pages() {
        let last = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(
            list_page(last.as_bytes()).unwrap(),
            ListPage {
                objects: vec![],
                next: None
            }
        );
        let broken = "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>";
        assert!(matches!(
            list_page(broken.as_bytes()),
            Err(ObjectStoreError::Protocol(_))
        ));
        assert!(list_page(b"<Error><Code>X</Code></Error>").is_err());
        assert!(list_page(b"not xml").is_err());
    }

    /// Catches: an upload id lost or taken from the wrong element (every part would be
    /// sent to an upload that does not exist), a completion that names parts out of
    /// order or unescaped, and a 200 completion whose body is an `<Error>` taken for
    /// success (an object reported written that the store does not hold).
    #[test]
    fn multipart_bodies() {
        let created = format!(
            "<?xml version=\"1.0\"?><InitiateMultipartUploadResult {NS}><Bucket>b</Bucket>\
             <Key>k</Key><UploadId>abc.def-1</UploadId></InitiateMultipartUploadResult>"
        );
        assert_eq!(upload_id(created.as_bytes()).unwrap(), "abc.def-1");
        assert!(upload_id(b"<InitiateMultipartUploadResult/>").is_err());
        let empty =
            b"<InitiateMultipartUploadResult><UploadId></UploadId></InitiateMultipartUploadResult>";
        assert!(upload_id(empty).is_err(), "an empty upload id");
        assert!(upload_id(b"\xff\xfe").is_err(), "not UTF-8");
        assert!(upload_id(b"<unclosed").is_err(), "not XML");
        assert!(upload_id(b"<Error><UploadId>x</UploadId></Error>").is_err());
        let body = complete_request(&["\"a1\"".into(), "b&2".into()]);
        assert!(
            body.contains(
                "<Part><PartNumber>1</PartNumber><ETag>\"a1\"</ETag></Part>\
                 <Part><PartNumber>2</PartNumber><ETag>b&amp;2</ETag></Part>"
            ),
            "{body}"
        );
        let done = format!(
            "<CompleteMultipartUploadResult {NS}><ETag>\"x-2\"</ETag></CompleteMultipartUploadResult>"
        );
        let etag = complete_answer(done.as_bytes()).unwrap();
        assert_eq!(etag.as_deref(), Some("\"x-2\""));
        let bare = format!("<CompleteMultipartUploadResult {NS}/>");
        assert_eq!(complete_answer(bare.as_bytes()).unwrap(), None);
        let late = b"<Error><Code>InternalError</Code><Message>m</Message></Error>";
        let late = complete_answer(late).err().map(|e| e.to_string());
        assert_eq!(late.as_deref(), Some("store answered 200 InternalError: m"));
        assert!(matches!(
            complete_answer(b""),
            Err(ObjectStoreError::Protocol(_))
        ));
    }

    /// Asserts that a completion names the parts by the numbers the caller gave, in the order
    /// given and not by position, with the ETags escaped.
    #[test]
    fn a_completion_names_parts_by_their_own_numbers() {
        let body = complete_parts_request(&[(2, "\"a\"".into()), (5, "b&c".into())]);
        assert!(
            body.contains(
                "<Part><PartNumber>2</PartNumber><ETag>\"a\"</ETag></Part>\
                 <Part><PartNumber>5</PartNumber><ETag>b&amp;c</ETag></Part>"
            ),
            "{body}"
        );
        assert!(body.starts_with("<CompleteMultipartUpload "), "{body}");
        assert!(body.ends_with("</CompleteMultipartUpload>"), "{body}");
    }

    /// Catches: an error body read without its code, which callers match on.
    #[test]
    fn error_body() {
        let body = "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code>\
                    <Message>The specified key does not exist.</Message></Error>";
        assert_eq!(
            error_code_message(body.as_bytes()),
            (
                "NoSuchKey".into(),
                "The specified key does not exist.".into()
            )
        );
        assert_eq!(error_code_message(b""), (String::new(), String::new()));
    }

    /// Catches: a non-UTF-8 error body that panics or leaks lossy bytes into the code
    /// (callers match on the code), and prose from a proxy dropped instead of kept (up
    /// to 200 characters) as the message an operator reads.
    #[test]
    fn error_body_that_is_not_xml_keeps_its_prose_and_not_utf8_gives_nothing() {
        assert_eq!(
            error_code_message(b"\xff\xfe"),
            (String::new(), String::new())
        );
        let prose = "x".repeat(300);
        let (code, message) = error_code_message(prose.as_bytes());
        assert_eq!(code, "");
        assert_eq!(message, "x".repeat(200));
    }

    /// Catches: a listing entry with a bad size, a bad `IsTruncated`, a key kbf would
    /// refuse, or an empty continuation token taken as a page (a listing that never
    /// advances), each accepted instead of refused as a protocol error.
    #[test]
    fn listing_answers_that_break_the_contract_are_protocol_errors() {
        let why_bytes = |body: &[u8]| {
            let err = list_page(body).unwrap_err();
            assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err:?}");
            err.to_string()
        };
        let why = |body: &str| why_bytes(body.as_bytes());
        assert!(why("<ListBucketResult><Contents><Key>a</Key><Size>x</Size></Contents></ListBucketResult>")
            .contains("a: missing or bad <Size>"));
        assert!(
            why("<ListBucketResult><Contents><Key>a</Key></Contents></ListBucketResult>")
                .contains("missing or bad <Size>")
        );
        assert!(
            why("<ListBucketResult><Contents><Size>1</Size></Contents></ListBucketResult>")
                .contains("<Contents> without <Key>")
        );
        assert!(why("<ListBucketResult><Contents><Key></Key><Size>1</Size></Contents></ListBucketResult>")
            .contains("listed key"));
        assert!(
            why("<ListBucketResult><IsTruncated>maybe</IsTruncated></ListBucketResult>")
                .contains("<IsTruncated> is \"maybe\"")
        );
        assert!(
            why("<ListBucketResult><IsTruncated>true</IsTruncated>\
             <NextContinuationToken></NextContinuationToken></ListBucketResult>")
            .contains("truncated without <NextContinuationToken>")
        );
        assert!(why("<Other/>").contains("root element is <Other>"));
        assert!(why_bytes(b"\xff").contains("ListObjectsV2 answer:"));
    }

    /// Catches: a pretty-printed answer (whitespace text between elements) whose text
    /// nodes are taken for elements, so a key or the token is read from the wrong node.
    #[test]
    fn a_pretty_printed_listing_reads_the_same_as_a_compact_one() {
        let body = "<ListBucketResult>\n  <IsTruncated>true</IsTruncated>\n  \
                    <NextContinuationToken>t</NextContinuationToken>\n  \
                    <Contents>\n    <Key>p/a</Key>\n    <Size>7</Size>\n  </Contents>\n\
                    </ListBucketResult>";
        let page = list_page(body.as_bytes()).unwrap();
        assert_eq!(page.next, Some(ListToken(Token::Continue("t".into()))));
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].key.as_str(), "p/a");
        assert_eq!(page.objects[0].size, 7);
    }

    /// Asserts that a stamped page carries each object's key, size and time of last write in
    /// milliseconds, and the next token as a plain page does.
    #[test]
    fn a_stamped_page_carries_the_times_of_last_write() {
        let body = format!(
            "<ListBucketResult {NS}><IsTruncated>true</IsTruncated>\
             <NextContinuationToken>t2</NextContinuationToken>\
             <Contents><Key>p/a</Key><LastModified>2009-10-12T17:50:30.000Z</LastModified>\
             <ETag>\"x\"</ETag><Size>3</Size></Contents>\
             <Contents><Key>p/b</Key><LastModified>1970-01-01T00:00:01.500Z</LastModified>\
             <Size>0</Size></Contents></ListBucketResult>"
        );
        let page = stamped_page(body.as_bytes()).unwrap();
        assert_eq!(page.next, Some(ListToken(Token::Continue("t2".into()))));
        let got: Vec<_> = page
            .objects
            .iter()
            .map(|o| (o.key.as_str(), o.size, o.modified_millis))
            .collect();
        assert_eq!(got, [("p/a", 3, 1_255_369_830_000), ("p/b", 0, 1_500)]);
        assert_eq!(list_page(body.as_bytes()).unwrap().objects.len(), 2);
    }

    /// Asserts that a stamped page is a protocol error, naming the key, when an object has no
    /// `<LastModified>` or one that is not a time, and that an answer that is not a listing
    /// is refused as it is for a plain page.
    #[test]
    fn a_stamped_page_without_readable_times_is_a_protocol_error() {
        for modified in ["", "<LastModified>yesterday</LastModified>"] {
            let body = format!(
                "<ListBucketResult><Contents><Key>p/a</Key>{modified}<Size>1</Size>\
                 </Contents></ListBucketResult>"
            );
            let err = stamped_page(body.as_bytes()).unwrap_err();
            assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err:?}");
            let why = "p/a: missing or bad <LastModified>";
            assert!(err.to_string().contains(why), "{err}");
        }
        assert!(stamped_page(b"not xml").is_err());
    }

    /// Catches: an uploads page that loses the id or the time of an upload, reads a truncated
    /// answer as the last page (so a janitor stops after one page), reads a document that is
    /// not a listing of uploads, or depends on the namespace.
    #[test]
    fn an_uploads_page_carries_keys_ids_times_and_the_markers_of_the_next_page() {
        let body = format!(
            "<ListMultipartUploadsResult {NS}><Bucket>b</Bucket><IsTruncated>true</IsTruncated>\
             <NextKeyMarker>p/b</NextKeyMarker><NextUploadIdMarker>u2</NextUploadIdMarker>\
             <Upload><Key>p/a</Key><UploadId>u1</UploadId>\
             <Initiated>2009-10-12T17:50:30.000Z</Initiated></Upload>\
             <Upload><Key>p/b</Key><UploadId>u2</UploadId>\
             <Initiated>2009-10-12T17:50:31.500Z</Initiated></Upload>\
             </ListMultipartUploadsResult>"
        );
        let page = uploads_page(body.as_bytes()).unwrap();
        let got: Vec<_> = page
            .uploads
            .iter()
            .map(|u| (u.key.as_str(), u.upload_id.as_str(), u.initiated_millis))
            .collect();
        let want = [
            ("p/a", "u1", 1_255_369_830_000),
            ("p/b", "u2", 1_255_369_831_500),
        ];
        assert_eq!(got, want);
        let next = page.next.unwrap();
        assert_eq!((next.0.as_str(), next.1.as_str()), ("p/b", "u2"));

        let last = "<ListMultipartUploadsResult><IsTruncated>false</IsTruncated>\
                    </ListMultipartUploadsResult>";
        let page = uploads_page(last.as_bytes()).unwrap();
        assert_eq!((page.uploads.len(), page.next), (0, None));
    }

    /// Catches: an uploads page that accepts an answer that breaks the contract: a truncated
    /// page with no key marker, an upload without a key, an id or a readable time, a flag
    /// that is not true or false, and a document of another kind.
    #[test]
    fn uploads_answers_that_break_the_contract_are_protocol_errors() {
        let at = "<Initiated>2009-10-12T17:50:30.000Z</Initiated>";
        let wrap = |inner: &str| {
            format!("<ListMultipartUploadsResult>{inner}</ListMultipartUploadsResult>")
        };
        let whole = format!("<Key>k</Key><UploadId>u</UploadId>{at}");
        let broken = [
            wrap("<IsTruncated>true</IsTruncated>"),
            wrap("<IsTruncated>maybe</IsTruncated>"),
            wrap(&format!("<Upload><UploadId>u</UploadId>{at}</Upload>")),
            wrap(&format!("<Upload><Key>k</Key>{at}</Upload>")),
            wrap(&format!(
                "<Upload><Key>k</Key><UploadId></UploadId>{at}</Upload>"
            )),
            wrap("<Upload><Key>k</Key><UploadId>u</UploadId></Upload>"),
            wrap("<Upload><Key>k</Key><UploadId>u</UploadId><Initiated>soon</Initiated></Upload>"),
            wrap(&format!(
                "<IsTruncated>true</IsTruncated><NextKeyMarker></NextKeyMarker>\
                 <Upload>{whole}</Upload>"
            )),
            "<ListBucketResult></ListBucketResult>".to_owned(),
            "not xml".to_owned(),
        ];
        for body in broken {
            let err = uploads_page(body.as_bytes()).unwrap_err();
            assert!(
                matches!(err, ObjectStoreError::Protocol(_)),
                "{body}: {err:?}"
            );
        }
        assert!(uploads_page(&[0xff, 0xfe]).is_err());
        let fine = wrap(&format!("<Upload>{whole}</Upload>"));
        assert_eq!(uploads_page(fine.as_bytes()).unwrap().uploads.len(), 1);
    }
}
