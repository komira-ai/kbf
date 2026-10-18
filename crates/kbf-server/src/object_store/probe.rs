//! The probe of a store: what `:verify` and a PUT with `verify` ask of it before anything
//! is written to the registry.
//!
//! The probe puts one small object under `<prefix>.kbf-probe/<random>`, reads it back,
//! optionally checks that a second write of the key is refused (when the connection says
//! `conditional_put`), and deletes it. It answers with the steps that passed and nothing
//! else: a store's own error text can carry anything, so it never leaves this module.
//! The probe proves that *this frontend* can reach the store with this key pair; each
//! storage node probes for itself before it opens the store.
//!
//! The frontend connects to an endpoint an operator typed, with the operator's key: an
//! operator-only request-forgery surface behind the API token. Every probe is bounded by
//! [`PROBE_BUDGET`] as a whole and by short connect and request limits, and the endpoint
//! must be a plain `http` origin ([`check_origin`]).

use std::num::NonZeroU64;
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use kbf_objstore::{ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_types::object_store_api::{Secret, Verified};
use tokio::runtime::Handle;

use crate::enroll::{OsRandom, Random};

/// The longest a whole probe may take, however the store behaves.
pub const PROBE_BUDGET: Duration = Duration::from_secs(20);

/// The signing region when a connection names none.
const DEFAULT_REGION: &str = "us-east-1";

/// What the probe writes.
const PROBE_BODY: &[u8] = b"kbf-probe";

/// Where a probe goes: a connection's endpoint and keys, and the bucket and prefix.
pub struct ProbeTarget<'a> {
    /// `http://host[:port]`.
    pub endpoint: &'a str,
    /// The signing region; empty for the default.
    pub region: &'a str,
    /// The access key id.
    pub access_key_id: &'a str,
    /// The secret access key.
    pub secret: &'a Secret,
    /// The bucket.
    pub bucket: &'a str,
    /// The key prefix; empty for none.
    pub prefix: &'a str,
    /// Whether the store is declared to refuse a second write of a key.
    pub conditional_put: bool,
    /// The slowest request body rate.
    pub min_put_rate: Option<NonZeroU64>,
}

/// Something that can probe a store. Blocking: the API runs it on the blocking pool.
pub trait Probe: Send + Sync {
    /// Probes `target`. The answer says which steps passed; a store that cannot be
    /// reached, or a target that cannot even be built, is every step `false`.
    fn probe(&self, target: &ProbeTarget<'_>) -> Verified;
}

/// Why an endpoint or a bucket is refused before any probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OriginFault {
    /// `https`: the S3 client does not speak TLS yet, so a registration would be accepted
    /// and then fail on every node.
    #[error("the store client does not speak TLS yet; use an http endpoint")]
    Tls,
    /// Not `http://host[:port]` with no credentials, path or query.
    #[error("the endpoint is not http://host[:port]")]
    Endpoint,
    /// Not a bucket name.
    #[error("the bucket name is not valid")]
    Bucket,
    /// Not a key prefix.
    #[error("the prefix is not valid")]
    Prefix,
}

/// Checks `endpoint`, `bucket` and `prefix` by building the store configuration the nodes
/// will build (no I/O), so the frontend refuses what every node would refuse. The error
/// is a closed fault: the store client's message quotes the endpoint, which may hold a
/// password somebody put in the URL.
///
/// # Errors
/// The first of the three that is refused.
pub fn check_origin(endpoint: &str, bucket: &str, prefix: &str) -> Result<(), OriginFault> {
    // Stub (red first): everything is accepted.
    let _ = (endpoint, bucket, prefix);
    if false
        && endpoint
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        return Err(OriginFault::Tls);
    }
    KeyPrefix::new(prefix).map_err(|_| OriginFault::Prefix)?;
    match store(&ProbeTarget {
        endpoint,
        region: "",
        access_key_id: "",
        secret: &Secret::new(String::new()),
        bucket,
        prefix,
        conditional_put: false,
        min_put_rate: None,
    }) {
        Ok(_) => Ok(()),
        Err(S3ConfigError::Bucket(_)) => Err(OriginFault::Bucket),
        Err(_) => Err(OriginFault::Endpoint),
    }
}

fn store(target: &ProbeTarget<'_>) -> Result<S3Store, S3ConfigError> {
    S3Store::new(S3Config {
        endpoint: target.endpoint.to_owned(),
        region: if target.region.is_empty() {
            DEFAULT_REGION.to_owned()
        } else {
            target.region.to_owned()
        },
        bucket: target.bucket.to_owned(),
        credentials: Credentials::new(target.access_key_id, target.secret.expose()),
        capabilities: Capabilities {
            conditional_put: target.conditional_put,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        min_body_rate: target.min_put_rate,
    })
}

/// The probe over the S3 client, run on a tokio runtime the caller names.
pub struct S3Probe {
    runtime: Handle,
    random: Box<dyn Random>,
    budget: Duration,
}

impl S3Probe {
    /// A probe that drives the S3 client on `runtime` (the caller is on the blocking pool,
    /// not on a runtime thread).
    #[must_use]
    pub fn new(runtime: Handle) -> Self {
        Self::with(runtime, Box::new(OsRandom), PROBE_BUDGET)
    }

    /// [`S3Probe::new`] with the source of the probe key's name and the whole-probe budget.
    #[must_use]
    pub fn with(runtime: Handle, random: Box<dyn Random>, budget: Duration) -> Self {
        Self {
            runtime,
            random,
            budget,
        }
    }

    /// `<prefix>.kbf-probe/<16 hex>`, or `None` when the prefix cannot make a key.
    fn key(&self, prefix: &str) -> Option<ObjectKey> {
        let mut bytes = [0u8; 8];
        self.random.fill(&mut bytes).ok()?;
        let dir = KeyPrefix::new(prefix).ok()?;
        let dir = if dir.as_str().is_empty() || dir.as_str().ends_with('/') {
            dir
        } else {
            KeyPrefix::new(format!("{}/", dir.as_str())).ok()?
        };
        dir.key(&format!(".kbf-probe/{}", hex::encode(bytes))).ok()
    }
}

impl Probe for S3Probe {
    fn probe(&self, target: &ProbeTarget<'_>) -> Verified {
        // Stub (red first): nothing is proven.
        let _ = (target, &self.runtime, &self.budget);
        Verified::default()
    }
}

#[cfg(test)]
mod tests;
