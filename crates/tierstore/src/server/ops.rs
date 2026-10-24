//! The S3 operations: each is calls to the backend and the answer S3 gives.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use hyper::StatusCode;
use hyper::http::request::Parts;
use sha2::{Digest, Sha256};

use crate::journal::is_reserved;
use crate::{ByteRange, Error, Meta};

use super::Server;
use super::reply::{Answer, S3Error, answer, xml_answer};
use super::request::{Query, Target, header};
use super::time::http_date;
use super::uploads::Part;
use super::xml::{self, Listing};

/// The query parameters this front understands. A request with another is not served.
const SUPPORTED: [&str; 8] = [
    "continuation-token",
    "list-type",
    "max-keys",
    "partNumber",
    "prefix",
    "start-after",
    "uploadId",
    "uploads",
];

/// The query parameters that make a request part of a multipart upload.
const MULTIPART: [&str; 3] = ["partNumber", "uploadId", "uploads"];

/// The most keys in a page of a listing, and the number of parts of an upload.
const PAGE_LIMIT: usize = 1000;
const PART_LIMIT: u32 = 10_000;

/// What a `Range` header asks for, of an object of a given size.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Window {
    /// All of it: no range, or one that is not a single valid range.
    Whole,
    /// `len` bytes from `start`, the last of which is in the object.
    Part { start: u64, len: u64 },
    /// A range that starts at or past the end.
    Unsatisfiable,
}

/// The first byte and the last (perhaps past the end) of `bytes=<spec>`, if it is one range.
fn span(spec: &str, size: u64) -> Option<(u64, u64)> {
    let (first, last) = spec.split_once('-')?;
    let number = |text: &str| text.parse::<u64>().ok();
    match (first, last) {
        ("", count) => Some((size.saturating_sub(number(count)?), u64::MAX)),
        (first, "") => Some((number(first)?, u64::MAX)),
        (first, last) => {
            let (first, last) = (number(first)?, number(last)?);
            (first <= last).then_some((first, last))
        }
    }
}

/// What the `Range` header, if there is one, asks for of an object of `size` bytes.
pub(crate) fn window(range: Option<&str>, size: u64) -> Window {
    let spec = range.and_then(|text| text.strip_prefix("bytes="));
    match spec.and_then(|spec| span(spec, size)) {
        None => Window::Whole,
        Some((start, _)) if start >= size => Window::Unsatisfiable,
        Some((start, last)) => Window::Part {
            start,
            len: last.min(size - 1) - start + 1,
        },
    }
}

/// The headers that describe an object.
fn object_headers(meta: &Meta) -> Vec<(&'static str, String)> {
    vec![
        ("etag", format!("\"{}\"", meta.tag)),
        ("last-modified", http_date(meta.modified)),
        ("accept-ranges", "bytes".to_owned()),
        ("content-type", "application/octet-stream".to_owned()),
    ]
}

/// A client may not name the keys the proxy keeps for itself.
fn refuse_reserved(key: &str) -> Result<(), S3Error> {
    if is_reserved(key) {
        return Err(S3Error::from(Error::Reserved(key.to_owned())));
    }
    Ok(())
}

/// The number of keys a listing asked for, at most [`PAGE_LIMIT`].
fn page_size(query: &Query) -> Result<usize, S3Error> {
    let Some(text) = query.get("max-keys") else {
        return Ok(PAGE_LIMIT);
    };
    match text.parse::<usize>() {
        Ok(count) if count > 0 => Ok(count.min(PAGE_LIMIT)),
        _ => Err(S3Error::invalid("max-keys must be a positive whole number")),
    }
}

/// The token of the page that follows the page that ends with `key`.
fn token(key: &str) -> String {
    hex::encode(key)
}

/// The key a token stands for.
fn untoken(token: &str) -> Result<String, S3Error> {
    let bad = || S3Error::invalid("the continuation token is not one this server gave");
    let bytes = hex::decode(token).map_err(|_| bad())?;
    String::from_utf8(bytes).map_err(|_| bad())
}

/// Whether the header `name` of the request names `tag` among its entity tags (`*` names every
/// tag), if the request has the header.
fn names(parts: &Parts, name: &str, tag: &str) -> Option<bool> {
    let list = header(&parts.headers, name)?;
    let mut listed = list.split(',').map(|item| item.trim().trim_matches('"'));
    Some(listed.any(|item| ["*", tag].contains(&item)))
}

/// The answer that the conditions of a read give in place of the object, if they do:
/// `PreconditionFailed` when `If-Match` does not name the tag of the object, and 304 when
/// `If-None-Match` does. `None` when the read goes on. The conditions on the time are not
/// supported.
fn conditional(parts: &Parts, key: &str, meta: &Meta) -> Result<Option<Answer>, S3Error> {
    let since = ["if-modified-since", "if-unmodified-since"];
    if since.iter().any(|name| parts.headers.contains_key(*name)) {
        return Err(S3Error::unsupported("only If-Match and If-None-Match are supported"));
    }
    let if_match = names(parts, "if-match", &meta.tag);
    let if_none_match = names(parts, "if-none-match", &meta.tag);
    match (if_match, if_none_match) {
        (Some(false), _) => Err(S3Error::from(Error::PreconditionFailed(key.to_owned()))),
        (_, Some(true)) => {
            let headers = object_headers(meta);
            let not_modified = answer(StatusCode::NOT_MODIFIED, &headers, Bytes::new());
            Ok(Some(not_modified))
        }
        _ => Ok(None),
    }
}

/// Runs a call that changes the store to its end on a task of its own. The server drops the
/// future of a request when the client goes away, and a put dropped after it wrote the object
/// and before it wrote the journal entry would leave an object that nothing copies or evicts,
/// and a key that no put can take.
async fn detached<T, F>(call: F) -> Result<T, S3Error>
where
    T: Send + 'static,
    F: Future<Output = Result<T, Error>> + Send + 'static,
{
    let done = tokio::spawn(call).await;
    let finished = done.expect("a call to the backend does not panic");
    finished.map_err(S3Error::from)
}

/// The key a listing starts after: the one its continuation token stands for, else
/// `start-after`.
fn start_after(query: &Query) -> Result<Option<String>, S3Error> {
    match query.get("continuation-token") {
        Some(text) => untoken(text).map(Some),
        None => Ok(query.get("start-after").map(str::to_owned)),
    }
}

/// The parts a completion names, joined, if every one was uploaded with that ETag.
fn assemble(wanted: &[(u32, String)], staged: &BTreeMap<u32, Part>) -> Result<Bytes, S3Error> {
    let ascending = wanted.windows(2).all(|pair| pair[0].0 < pair[1].0);
    if !ascending {
        let text = "The parts must be listed in ascending order of their numbers.";
        return Err(S3Error::new(StatusCode::BAD_REQUEST, "InvalidPartOrder", text));
    }
    let mut object = BytesMut::new();
    for (number, tag) in wanted {
        let part = staged.get(number).filter(|part| part.tag == *tag);
        let Some(part) = part else {
            let text = "A part was not uploaded, or its ETag is not the one that was named.";
            return Err(S3Error::new(StatusCode::BAD_REQUEST, "InvalidPart", text));
        };
        object.extend_from_slice(&part.body);
    }
    Ok(object.freeze())
}

impl Server {
    /// Does what a request asks for, once it is signed and its body is read.
    pub(super) async fn dispatch(
        &self,
        parts: &Parts,
        target: Target,
        query: &Query,
        body: Bytes,
    ) -> Result<Answer, S3Error> {
        if !query.only(&SUPPORTED) {
            return Err(S3Error::unsupported("this query parameter is not supported"));
        }
        if header(&parts.headers, "x-amz-copy-source").is_some() {
            return Err(S3Error::unsupported("copying an object is not supported"));
        }
        let method = parts.method.as_str();
        match (method, target, query.has_any(&MULTIPART)) {
            ("PUT" | "HEAD", Target::Bucket, false) => {
                Ok(answer(StatusCode::OK, &[], Bytes::new()))
            }
            ("GET", Target::Bucket, false) => self.list_objects(query).await,
            ("GET", Target::Object(key), false) => self.get_object(parts, &key).await,
            ("HEAD", Target::Object(key), false) => self.head_object(parts, &key).await,
            ("PUT", Target::Object(key), false) => self.put_object(parts, &key, body).await,
            ("PUT", Target::Object(key), true) => self.upload_part(query, &key, body).await,
            ("DELETE", Target::Object(key), false) => self.delete_object(&key).await,
            ("DELETE", Target::Object(key), true) => self.abort_upload(query, &key),
            ("POST", Target::Object(key), true) if query.has("uploads") => self.start_upload(&key),
            ("POST", Target::Object(key), true) if query.has("uploadId") => {
                self.complete_upload(query, &key, body).await
            }
            _ => Err(S3Error::unsupported("this operation is not supported")),
        }
    }

    async fn get_object(&self, parts: &Parts, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let meta = self.backend.head(key).await?;
        if let Some(early) = conditional(parts, key, &meta)? {
            return Ok(early);
        }
        let mut headers = object_headers(&meta);
        match window(header(&parts.headers, "range"), meta.size) {
            Window::Whole => {
                let body = self.backend.get(key, None).await?;
                Ok(answer(StatusCode::OK, &headers, body))
            }
            Window::Part { start, len } => {
                let range = ByteRange::new(start, len);
                let body = self.backend.get(key, Some(range)).await?;
                let (last, size) = (start + len - 1, meta.size);
                headers.push(("content-range", format!("bytes {start}-{last}/{size}")));
                Ok(answer(StatusCode::PARTIAL_CONTENT, &headers, body))
            }
            Window::Unsatisfiable => Err(S3Error::invalid_range(meta.size)),
        }
    }

    async fn head_object(&self, parts: &Parts, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let meta = self.backend.head(key).await?;
        if let Some(early) = conditional(parts, key, &meta)? {
            return Ok(early);
        }
        let mut headers = object_headers(&meta);
        headers.push(("content-length", meta.size.to_string()));
        Ok(answer(StatusCode::OK, &headers, Bytes::new()))
    }

    async fn put_object(&self, parts: &Parts, key: &str, body: Bytes) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let if_match = header(&parts.headers, "if-match");
        match (if_match, header(&parts.headers, "if-none-match")) {
            (Some(tag), _) => self.replace_object(key, tag).await,
            (None, None | Some("*")) => {
                self.put_detached(key, body).await?;
                Ok(answer(StatusCode::OK, &[], Bytes::new()))
            }
            (None, Some(_)) => Err(S3Error::unsupported("only If-None-Match: * is supported")),
        }
    }

    /// `If-Match`: an object cannot be replaced, so the answer is that the condition does not
    /// hold, or that the replacement is not done.
    async fn replace_object(&self, key: &str, tag: &str) -> Result<Answer, S3Error> {
        let meta = self.backend.head(key).await?;
        if meta.tag != tag.trim_matches('"') {
            return Err(S3Error::from(Error::PreconditionFailed(key.to_owned())));
        }
        Err(S3Error::unsupported("objects are immutable and cannot be replaced"))
    }

    async fn delete_object(&self, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let (backend, key) = (Arc::clone(&self.backend), key.to_owned());
        detached(async move { backend.delete(&key).await }).await?;
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }

    /// Creates the object in the backend, and goes on to the end if the request is dropped.
    async fn put_detached(&self, key: &str, body: Bytes) -> Result<(), S3Error> {
        let (backend, key) = (Arc::clone(&self.backend), key.to_owned());
        detached(async move { backend.put(&key, body).await }).await
    }

    async fn list_objects(&self, query: &Query) -> Result<Answer, S3Error> {
        if query.get("list-type") != Some("2") {
            return Err(S3Error::unsupported("only ListObjectsV2 (list-type=2) is supported"));
        }
        let prefix = query.get("prefix").unwrap_or_default();
        let max_keys = page_size(query)?;
        let after = start_after(query)?;
        let after = after.as_deref();
        let listed = self.backend.list_page(prefix, after, max_keys + 1).await?;
        let page = &listed[..listed.len().min(max_keys)];
        let next = (listed.len() > max_keys).then(|| token(&page[max_keys - 1].key));
        let listing = Listing {
            bucket: &self.config.bucket,
            prefix,
            max_keys,
            objects: page,
            next,
        };
        Ok(xml_answer(StatusCode::OK, listing.to_xml()))
    }

    fn start_upload(&self, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = self.uploads.create(key, self.config.upload_ttl);
        let body = xml::initiate(&self.config.bucket, key, &id);
        Ok(xml_answer(StatusCode::OK, body))
    }

    async fn upload_part(&self, query: &Query, key: &str, body: Bytes) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId");
        let parse = |text: &str| text.parse::<u32>().ok();
        let number = query.get("partNumber").and_then(parse);
        let (Some(id), Some(number)) = (id, number) else {
            return Err(S3Error::invalid("an upload id and a part number are needed"));
        };
        if !(1..=PART_LIMIT).contains(&number) {
            return Err(S3Error::invalid("a part number is from 1 to 10000"));
        }
        let tag = hex::encode(Sha256::digest(&body));
        let etag = format!("\"{tag}\"");
        let part = Part { body, tag };
        let (limit, ttl) = (self.config.max_staged_bytes, self.config.upload_ttl);
        self.uploads.add_part(id, key, number, part, limit, ttl)?;
        Ok(answer(StatusCode::OK, &[("etag", etag)], Bytes::new()))
    }

    async fn complete_upload(
        &self,
        query: &Query,
        key: &str,
        body: Bytes,
    ) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId").unwrap_or_default();
        let wanted = xml::parse_complete(&body).filter(|parts| !parts.is_empty());
        let Some(wanted) = wanted else {
            let text = "The request body is not a list of parts.";
            return Err(S3Error::new(StatusCode::BAD_REQUEST, "MalformedXML", text));
        };
        let staged = self.uploads.parts(id, key);
        let staged = staged.ok_or_else(S3Error::no_such_upload)?;
        let object = assemble(&wanted, &staged)?;
        self.put_detached(key, object).await?;
        self.uploads.finish(id);
        let tags: String = wanted.iter().map(|(_, tag)| tag.as_str()).collect();
        let (digest, count) = (hex::encode(Sha256::digest(tags)), wanted.len());
        let etag = format!("\"{digest}-{count}\"");
        let body = xml::completed(&self.config.bucket, key, &etag);
        Ok(xml_answer(StatusCode::OK, body))
    }

    fn abort_upload(&self, query: &Query, key: &str) -> Result<Answer, S3Error> {
        let id = query.get("uploadId").unwrap_or_default();
        if !self.uploads.abort(id, key) {
            return Err(S3Error::no_such_upload());
        }
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }
}
