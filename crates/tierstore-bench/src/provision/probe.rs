//! What a key pair can reach, asked of a store with one signed request each.
//!
//! `provision` uses it to prove the scoped account reaches its test bucket and is refused a live
//! one; `verify-teardown` uses it to prove the account is gone and the test bucket is gone and
//! empty. The requests do not go through the request log: that log is the proof of what the
//! measurements sent, and a probe of a live bucket (refused by design) must not be in it.

use std::time::SystemTime;

use objstore_s3::s3::{Credentials, S3Config, S3Store};
use objstore_s3::{
    ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore as _, ObjectStoreError, PageSize,
};

use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

use crate::cells::streamget::{self, EMPTY_SHA256, amz_date, authorization};
use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::s3::{Timeouts, error_kind};

/// The key a read probe asks for. It does not exist in any bucket.
const PROBE_KEY: &str = "tierstore-bench-probe-never-written";

/// What one request signed with a key pair got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// The store served the request (an absent key counts: the bucket was reached).
    Reached,
    /// The store refused with 403 and this code (`AccessDenied`).
    Denied(String),
    /// The store does not know the access key.
    UnknownKey,
    /// The store has no such bucket.
    NoSuchBucket,
    /// Anything else: the request failed in another way, named by its class.
    Failed(String),
}

/// What one listing of the bucket itself (its objects, its multipart uploads) came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    /// The listing worked: the entries on its first page (at most 1000).
    Count(usize),
    /// The store answered `NoSuchBucket`.
    NoSuchBucket,
    /// Any other failure, named by its status and code or its class.
    Failed(String),
}

/// The state of a bucket as `verify-teardown` reads it. Whether the bucket exists is decided by
/// the account listing alone (`ListBuckets`); the two readings of the bucket itself are what the
/// checks report beside it, and neither one says "removed".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketState {
    /// Whether the account listing names the bucket; the text says why the listing was not read.
    pub listed: Result<bool, String>,
    /// The first page of the bucket's objects.
    pub objects: Reading,
    /// The first page of the bucket's open multipart uploads.
    pub uploads: Reading,
}

/// A client of `spec`'s bucket with `spec`'s key pair. Does no I/O.
fn client(spec: &StoreSpec) -> Result<S3Store, BenchError> {
    let timeouts = Timeouts::default();
    S3Store::new(S3Config {
        endpoint: spec.endpoint.clone(),
        region: spec.region.clone(),
        bucket: spec.bucket.clone(),
        credentials: Credentials::new(
            spec.access_key_id.clone(),
            spec.secret_access_key.expose().to_owned(),
        ),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: timeouts.connect,
        request_timeout: timeouts.request,
        min_body_rate: None,
    })
    .map_err(BenchError::from)
}

/// A short, host-free text of an error: the status and code of a store's answer, or the class.
fn describe(error: &ObjectStoreError) -> String {
    match error {
        ObjectStoreError::Service { status, code, .. } => format!("status {status} {code}"),
        other => error_kind(other).to_owned(),
    }
}

/// Sorts an outcome into an [`Access`].
fn classify(outcome: Result<(), ObjectStoreError>) -> Access {
    match outcome {
        Ok(()) | Err(ObjectStoreError::NotFound(_)) => Access::Reached,
        Err(ObjectStoreError::Service { code, .. }) if code == "NoSuchBucket" => {
            Access::NoSuchBucket
        }
        Err(ObjectStoreError::Service { code, .. }) if code == "InvalidAccessKeyId" => {
            Access::UnknownKey
        }
        Err(ObjectStoreError::Service {
            status: 403, code, ..
        }) => Access::Denied(code),
        Err(other) => Access::Failed(describe(&other)),
    }
}

/// Lists the bucket of `spec` with its key pair.
///
/// # Errors
/// [`BenchError::Store`] if the endpoint or the bucket name is not accepted.
pub async fn can_list(spec: &StoreSpec) -> Result<Access, BenchError> {
    let store = client(spec)?;
    let listed = store
        .list(&KeyPrefix::default(), None, PageSize::MAX)
        .await
        .map(|_| ());
    Ok(classify(listed))
}

/// Reads one byte of a key that does not exist in `bucket`, signed with the key pair of `spec`
/// (whose own bucket is not used). A key that is not there still proves the read was allowed.
///
/// # Errors
/// [`BenchError::Store`] if the endpoint or the bucket name is not accepted.
///
/// # Panics
/// Never: the probe key is a constant that is a valid key.
pub async fn can_read(spec: &StoreSpec, bucket: &str) -> Result<Access, BenchError> {
    let other = StoreSpec {
        bucket: bucket.to_owned(),
        ..spec.clone()
    };
    let store = client(&other)?;
    let key = ObjectKey::new(PROBE_KEY).expect("the probe key is valid");
    let range = ByteRange::new(0, 1).expect("one byte is a range");
    let read = store.get_range(&key, range).await.map(|_| ());
    Ok(classify(read))
}

/// The text between `open` and `close` wherever they enclose it, in order.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once(open)
        && let Some((inside, next)) = after.split_once(close)
    {
        found.push(inside);
        rest = next;
    }
    found
}

/// The names of the buckets the key pair of `spec` is told of by `ListBuckets` (`GET /`), signed
/// with the same SigV4 as the streamed reads. The error text is host-free. An answer is a list
/// only if it opens and closes `ListAllMyBucketsResult`: a cut-off body must not read as a list
/// without the bucket.
///
/// `spec`'s endpoint must be one [`client`] accepts (it is parsed here).
pub async fn bucket_names(spec: &StoreSpec) -> Result<Vec<String>, String> {
    let url = reqwest::Url::parse(&spec.endpoint).expect("the client accepted the endpoint");
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let headers: Vec<(&str, String)> = vec![
        ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
        ("x-amz-date", amz_date(SystemTime::now())),
    ];
    let authorization = authorization(
        &streamget::Credentials {
            access_key_id: &spec.access_key_id,
            secret_access_key: spec.secret_access_key.expose(),
            region: &spec.region,
        },
        &host,
        "/",
        &headers,
    );
    let mut request = reqwest::Client::new()
        .get(format!("{}/", spec.endpoint.trim_end_matches('/')))
        .timeout(Timeouts::default().request);
    for (name, value) in &headers {
        request = request.header(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("a signed header value is visible ASCII"),
        );
    }
    let response = request
        .header(AUTHORIZATION, authorization)
        .send()
        .await
        .map_err(|_| "transport".to_owned())?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if status != 200 {
        return Err(match between(&body, "<Code>", "</Code>").first() {
            Some(code) => format!("status {status} {code}"),
            None => format!("status {status}"),
        });
    }
    if !(body.contains("<ListAllMyBucketsResult") && body.contains("</ListAllMyBucketsResult>")) {
        return Err("the answer is not a bucket list".to_owned());
    }
    let buckets = between(&body, "<Buckets>", "</Buckets>").concat();
    Ok(between(&buckets, "<Name>", "</Name>")
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// Sorts one listing of the bucket into a [`Reading`].
fn reading(outcome: Result<usize, ObjectStoreError>) -> Reading {
    match outcome {
        Ok(entries) => Reading::Count(entries),
        Err(ObjectStoreError::Service { code, .. }) if code == "NoSuchBucket" => {
            Reading::NoSuchBucket
        }
        Err(other) => Reading::Failed(describe(&other)),
    }
}

/// The state of the bucket of `spec`, read with its key pair: the account listing, which decides
/// whether the bucket exists, and a listing of its objects and of its open multipart uploads,
/// each read on its own (a refusal of one does not hide the other).
///
/// # Errors
/// [`BenchError::Store`] if the endpoint or the bucket name is not accepted.
pub async fn bucket_state(spec: &StoreSpec) -> Result<BucketState, BenchError> {
    bucket_state_and_names(spec).await.map(|(state, _)| state)
}

/// [`bucket_state`], and the account listing it decided from, so that a caller that needs to
/// ask more of that list does not ask for a second one.
///
/// # Errors
/// [`BenchError::Store`] if the endpoint or the bucket name is not accepted.
pub async fn bucket_state_and_names(
    spec: &StoreSpec,
) -> Result<(BucketState, Result<Vec<String>, String>), BenchError> {
    let store = client(spec)?;
    let prefix = KeyPrefix::default();
    let names = bucket_names(spec).await;
    let listed = names.clone().map(|names| names.contains(&spec.bucket));
    let objects = reading(
        store
            .list(&prefix, None, PageSize::MAX)
            .await
            .map(|page| page.objects.len()),
    );
    let uploads = reading(
        store
            .list_multipart_uploads(&prefix, None, PageSize::MAX)
            .await
            .map(|open| open.uploads.len()),
    );
    Ok((
        BucketState {
            listed,
            objects,
            uploads,
        },
        names,
    ))
}
