//! What a key pair can reach, asked of a store with one signed request each.
//!
//! `provision` uses it to prove the scoped account reaches its test bucket and is refused a live
//! one; `verify-teardown` uses it to prove the account is gone and the test bucket is gone and
//! empty. The requests do not go through the request log: that log is the proof of what the
//! measurements sent, and a probe of a live bucket (refused by design) must not be in it.

use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore as _, ObjectStoreError, PageSize,
};

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

/// The state of a bucket as `verify-teardown` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BucketState {
    /// The store says there is no such bucket.
    Gone,
    /// The bucket exists: the objects on its first page and the multipart uploads open.
    Present {
        /// Objects on the first page of a listing (at most 1000).
        objects: usize,
        /// Multipart uploads on the first page of their listing (at most 1000).
        uploads: usize,
    },
    /// The state could not be read; the text says why.
    Unreadable(String),
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

/// The state of the bucket of `spec`, read with its key pair.
///
/// # Errors
/// [`BenchError::Store`] if the endpoint or the bucket name is not accepted.
pub async fn bucket_state(spec: &StoreSpec) -> Result<BucketState, BenchError> {
    let store = client(spec)?;
    let prefix = KeyPrefix::default();
    let listed = store.list(&prefix, None, PageSize::MAX).await;
    let page = match listed {
        Ok(page) => page,
        Err(ObjectStoreError::Service { code, .. }) if code == "NoSuchBucket" => {
            return Ok(BucketState::Gone);
        }
        Err(other) => return Ok(BucketState::Unreadable(describe(&other))),
    };
    let uploads = store
        .list_multipart_uploads(&prefix, None, PageSize::MAX)
        .await;
    Ok(match uploads {
        Ok(open) => BucketState::Present {
            objects: page.objects.len(),
            uploads: open.uploads.len(),
        },
        Err(other) => BucketState::Unreadable(describe(&other)),
    })
}
