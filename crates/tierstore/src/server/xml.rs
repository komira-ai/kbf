//! The XML bodies of S3: the error body, the answers of ListObjectsV2 and of a multipart
//! upload, and the one body that is read, `CompleteMultipartUpload`.

use roxmltree::{Document, Node};

use crate::Listed;

use super::time::iso8601;

/// The namespace of the documents S3 sends.
const NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

const PROLOG: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>";

/// `text` with the characters XML gives meaning to escaped.
pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn element(name: &str, text: &str) -> String {
    format!("<{name}>{text}</{name}>")
}

/// The body of an error answer. `resource` is the path of the request.
pub(crate) fn error_body(code: &str, message: &str, resource: &str, request_id: &str) -> String {
    let (message, resource) = (escape(message), escape(resource));
    format!(
        "{PROLOG}<Error><Code>{code}</Code><Message>{message}</Message>\
         <Resource>{resource}</Resource><RequestId>{request_id}</RequestId></Error>"
    )
}

/// One page of a ListObjectsV2 answer.
pub(crate) struct Listing<'a> {
    pub(crate) bucket: &'a str,
    pub(crate) prefix: &'a str,
    pub(crate) max_keys: usize,
    pub(crate) objects: &'a [Listed],
    /// The continuation token of the next page, if the listing goes on.
    pub(crate) next: Option<String>,
}

impl Listing<'_> {
    pub(crate) fn to_xml(&self) -> String {
        let (bucket, prefix) = (escape(self.bucket), escape(self.prefix));
        let (count, max_keys) = (self.objects.len(), self.max_keys);
        let truncated = self.next.is_some();
        let mut xml = format!("{PROLOG}<ListBucketResult xmlns=\"{NAMESPACE}\">");
        xml.push_str(&element("Name", &bucket));
        xml.push_str(&element("Prefix", &prefix));
        xml.push_str(&format!(
            "<KeyCount>{count}</KeyCount><MaxKeys>{max_keys}</MaxKeys>"
        ));
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        xml.push_str(&next_token(self.next.as_deref()));
        for object in self.objects {
            let key = escape(&object.key);
            let (size, modified) = (object.size, iso8601(object.modified));
            xml.push_str(&format!(
                "<Contents><Key>{key}</Key><LastModified>{modified}</LastModified>\
                 <Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>"
            ));
        }
        xml.push_str("</ListBucketResult>");
        xml
    }
}

/// The element that carries the token of the next page, or nothing for the last page.
fn next_token(next: Option<&str>) -> String {
    next.map(|token| element("NextContinuationToken", &escape(token)))
        .unwrap_or_default()
}

/// The answer of CreateMultipartUpload.
pub(crate) fn initiate(bucket: &str, key: &str, upload_id: &str) -> String {
    let (bucket, key) = (escape(bucket), escape(key));
    format!(
        "{PROLOG}<InitiateMultipartUploadResult xmlns=\"{NAMESPACE}\"><Bucket>{bucket}</Bucket>\
         <Key>{key}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
    )
}

/// The answer of CompleteMultipartUpload.
pub(crate) fn completed(bucket: &str, key: &str, etag: &str) -> String {
    let (bucket, key, etag) = (escape(bucket), escape(key), escape(etag));
    format!(
        "{PROLOG}<CompleteMultipartUploadResult xmlns=\"{NAMESPACE}\">\
         <Location>/{bucket}/{key}</Location><Bucket>{bucket}</Bucket><Key>{key}</Key>\
         <ETag>{etag}</ETag></CompleteMultipartUploadResult>"
    )
}

/// The text of the first child of `node` named `name`, in any namespace.
fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|child| child.tag_name().name() == name)
        .and_then(|child| child.text())
}

/// The parts a `CompleteMultipartUpload` body names, in its order: the number of each and its
/// ETag without the quotes. `None` if the body is not that document.
pub(crate) fn parse_complete(body: &[u8]) -> Option<Vec<(u32, String)>> {
    let text = std::str::from_utf8(body).ok()?;
    let doc = Document::parse(text).ok()?;
    let root = doc.root_element();
    (root.tag_name().name() == "CompleteMultipartUpload").then_some(())?;
    root.children()
        .filter(|node| node.tag_name().name() == "Part")
        .map(|part| {
            let number = child_text(part, "PartNumber")?.trim().parse::<u32>().ok()?;
            let tag = child_text(part, "ETag")?.trim().trim_matches('"');
            Some((number, tag.to_owned()))
        })
        .collect()
}
