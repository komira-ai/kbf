//! The S3 operations: each is calls to the backend and the answer S3 gives.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use hyper::StatusCode;
use hyper::body::Body;
use hyper::header::HeaderMap;
use hyper::http::request::Parts;

use crate::journal::is_reserved;
use crate::{ByteRange, Error, Meta};

use super::Server;
use super::handover::hand_over;
use super::reply::{Answer, S3Error, answer, xml_answer};
use super::request::{BoxError, Check, Query, Reader, Refused, Target, header, pieces};
use super::time::http_date;
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

/// Refuses what this front does not do whatever the call is: a query parameter it does not
/// know, and a copy.
fn refuse_unsupported(parts: &Parts, query: &Query) -> Result<(), S3Error> {
    if !query.only(&SUPPORTED) {
        return Err(S3Error::unsupported(
            "this query parameter is not supported",
        ));
    }
    if header(&parts.headers, "x-amz-copy-source").is_some() {
        return Err(S3Error::unsupported("copying an object is not supported"));
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
        return Err(S3Error::unsupported(
            "only If-Match and If-None-Match are supported",
        ));
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
/// future of a request when the client goes away, and a delete dropped after it removed the
/// object from one tier would leave it in the other. The completion of a multipart upload is
/// carried to its end too, so that the parts it joins are not left half joined. (A put does
/// the same on its own, see `put_streamed`.)
async fn detached<T, F>(call: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: Future<Output = Result<T, Error>> + Send + 'static,
{
    let done = tokio::spawn(call).await;
    done.expect("a call to the backend does not panic")
}

/// Whether a put only creates, which is what it does when it has no `If-Match` and its
/// `If-None-Match` is absent or `*`: the body of such a put is streamed to the backend.
fn creates_only(headers: &HeaderMap) -> bool {
    let conditions = (
        header(headers, "if-match"),
        header(headers, "if-none-match"),
    );
    matches!(conditions, (None, None | Some("*")))
}

/// The answer to a failed call on a multipart upload: where the backend has no such upload, it
/// is `NoSuchUpload`, not `NoSuchKey`.
fn upload_error(error: Error) -> S3Error {
    match error {
        Error::NotFound(_) => S3Error::no_such_upload(),
        other => S3Error::from(other),
    }
}

/// The key a listing starts after: the one its continuation token stands for, else
/// `start-after`.
fn start_after(query: &Query) -> Result<Option<String>, S3Error> {
    match query.get("continuation-token") {
        Some(text) => untoken(text).map(Some),
        None => Ok(query.get("start-after").map(str::to_owned)),
    }
}

impl Server {
    /// Does what a request asks for, once it is signed. The body of a multipart part and that
    /// of a put that only creates are streamed to the backend as they arrive; every other call
    /// reads its body first.
    pub(super) async fn dispatch<B>(
        &self,
        parts: &Parts,
        target: Target,
        query: &Query,
        reader: Reader<B>,
        check: Check,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        refuse_unsupported(parts, query)?;
        let method = parts.method.as_str();
        let multipart = query.has_any(&MULTIPART);
        if let ("PUT", Target::Object(key), true) = (method, &target, multipart) {
            let declared = header(&parts.headers, "content-length");
            let len = declared.and_then(|text| text.parse().ok());
            return self.upload_part(query, key, len, reader, check).await;
        }
        if let ("PUT", Target::Object(key), false) = (method, &target, multipart)
            && creates_only(&parts.headers)
        {
            let declared = header(&parts.headers, "content-length");
            let len = declared.and_then(|text| text.parse().ok());
            return self.put_streamed(key, len, reader, check).await;
        }
        let body = reader.read_all(check).await?;
        match (method, target, multipart) {
            ("PUT" | "HEAD", Target::Bucket, false) => {
                Ok(answer(StatusCode::OK, &[], Bytes::new()))
            }
            ("GET", Target::Bucket, false) => self.list_objects(query).await,
            ("GET", Target::Object(key), false) => self.get_object(parts, &key).await,
            ("HEAD", Target::Object(key), false) => self.head_object(parts, &key).await,
            ("PUT", Target::Object(key), false) => self.put_conditional(parts, &key).await,
            ("DELETE", Target::Object(key), false) => self.delete_object(&key).await,
            ("DELETE", Target::Object(key), true) => self.abort_upload(query, &key).await,
            ("POST", Target::Object(key), true) if query.has("uploads") => {
                self.start_upload(&key).await
            }
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

    /// A put that is not a plain create (see [`creates_only`]): `If-Match`, which is refused
    /// because an object cannot be replaced, or an `If-None-Match` other than `*`.
    async fn put_conditional(&self, parts: &Parts, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        match header(&parts.headers, "if-match") {
            Some(tag) => self.replace_object(key, tag).await,
            None => Err(S3Error::unsupported("only If-None-Match: * is supported")),
        }
    }

    /// Creates the object from the body of the request as it arrives, create-only. The body is
    /// not held: the pieces are checked against the hashes of the request as they pass, and go
    /// on through a short queue to the backend's streamed put, which makes the object visible
    /// only after the last piece. A body that is refused (a hash that does not match, a body
    /// over the limit, a stalled or cut one) ends the stream with an error, so the backend
    /// stores nothing, and `refused` has the answer to send. `len` is the `Content-Length` of
    /// the request, if it has one.
    ///
    /// The backend's call runs on a task of its own, so that a client that goes away after
    /// the whole body was handed over leaves the put to be carried to its end, with the
    /// journal entry; a client that goes away before that ends the stream with an error, and
    /// nothing is stored.
    async fn put_streamed<B>(
        &self,
        key: &str,
        len: Option<u64>,
        reader: Reader<B>,
        check: Check,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        refuse_reserved(key)?;
        let refused = Refused::default();
        let (carry, body) = hand_over(pieces(reader, check, refused.clone()));
        let (backend, name) = (Arc::clone(&self.backend), key.to_owned());
        let mut storing = tokio::spawn(async move { backend.put_stream(&name, len, body).await });
        let early = tokio::select! {
            biased;
            done = &mut storing => Some(done),
            () = carry => None,
        };
        let stored = match early {
            Some(done) => done,
            None => storing.await,
        };
        let stored = stored.expect("a call to the backend does not panic");
        if let Some(error) = refused.take() {
            return Err(error);
        }
        stored.map_err(S3Error::from)?;
        Ok(answer(StatusCode::OK, &[], Bytes::new()))
    }

    /// `If-Match`: an object cannot be replaced, so the answer is that the condition does not
    /// hold, or that the replacement is not done.
    async fn replace_object(&self, key: &str, tag: &str) -> Result<Answer, S3Error> {
        let meta = self.backend.head(key).await?;
        if meta.tag != tag.trim_matches('"') {
            return Err(S3Error::from(Error::PreconditionFailed(key.to_owned())));
        }
        Err(S3Error::unsupported(
            "objects are immutable and cannot be replaced",
        ))
    }

    async fn delete_object(&self, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let (backend, key) = (Arc::clone(&self.backend), key.to_owned());
        let deleted = detached(async move { backend.delete(&key).await }).await;
        deleted.map_err(S3Error::from)?;
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }

    async fn list_objects(&self, query: &Query) -> Result<Answer, S3Error> {
        if query.get("list-type") != Some("2") {
            return Err(S3Error::unsupported(
                "only ListObjectsV2 (list-type=2) is supported",
            ));
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

    async fn start_upload(&self, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let created = self.backend.create_upload(key).await;
        let id = created.map_err(upload_error)?;
        let body = xml::initiate(&self.config.bucket, key, &id);
        Ok(xml_answer(StatusCode::OK, body))
    }

    /// Stores a part from the body of the request as it arrives: the backend reads the pieces
    /// of the body from a stream, and the body is checked against its hashes as the pieces pass,
    /// so a part of any size is stored without being held. A part that is refused (a hash that
    /// does not match, a body that is too large or stalls) is not stored. `len` is the
    /// `Content-Length` of the request, if it has one, which the backend is told: a store that
    /// must know the length of a part before it is sent is written to as the part arrives.
    async fn upload_part<B>(
        &self,
        query: &Query,
        key: &str,
        len: Option<u64>,
        reader: Reader<B>,
        check: Check,
    ) -> Result<Answer, S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        refuse_reserved(key)?;
        let id = query.get("uploadId");
        let parse = |text: &str| text.parse::<u32>().ok();
        let number = query.get("partNumber").and_then(parse);
        let (Some(id), Some(number)) = (id, number) else {
            return Err(S3Error::invalid(
                "an upload id and a part number are needed",
            ));
        };
        if !(1..=PART_LIMIT).contains(&number) {
            return Err(S3Error::invalid("a part number is from 1 to 10000"));
        }
        let refused = Refused::default();
        let body = pieces(reader, check, refused.clone());
        let backend = &self.backend;
        let storing = backend.upload_part_with_len(key, id, number, len, body);
        let stored = storing.await;
        if let Some(error) = refused.take() {
            return Err(error);
        }
        let etag = stored.map_err(upload_error)?;
        let headers = [("etag", format!("\"{etag}\""))];
        Ok(answer(StatusCode::OK, &headers, Bytes::new()))
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
        let ascending = wanted.windows(2).all(|pair| pair[0].0 < pair[1].0);
        if !ascending {
            let text = "The parts must be listed in ascending order of their numbers.";
            return Err(S3Error::new(
                StatusCode::BAD_REQUEST,
                "InvalidPartOrder",
                text,
            ));
        }
        let (backend, name, upload) = (Arc::clone(&self.backend), key.to_owned(), id.to_owned());
        let call = async move { backend.complete_upload(&name, &upload, &wanted).await };
        let etag = detached(call).await.map_err(upload_error)?;
        let etag = format!("\"{etag}\"");
        let body = xml::completed(&self.config.bucket, key, &etag);
        Ok(xml_answer(StatusCode::OK, body))
    }

    async fn abort_upload(&self, query: &Query, key: &str) -> Result<Answer, S3Error> {
        refuse_reserved(key)?;
        let id = query.get("uploadId").unwrap_or_default();
        let aborted = self.backend.abort_upload(key, id).await;
        aborted.map_err(upload_error)?;
        Ok(answer(StatusCode::NO_CONTENT, &[], Bytes::new()))
    }
}
