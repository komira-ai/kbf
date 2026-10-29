//! Names: an estimator key as a profile name, a worker as a host.

use std::sync::atomic::{AtomicU64, Ordering};

use js_types::HostId;
use js_usage::ProfileKey;
use kbf_estimator::{Basis, Cfg, Key};
use kbf_types::WorkerId;
use sha2::{Digest, Sha256};

/// Starts every hashed key, so no other use of SHA-256 can collide with a profile name.
const PROFILE_DOMAIN: &[u8] = b"kbf-usage/profile/v1";

/// Starts every hashed worker id.
const HOST_DOMAIN: &[u8] = b"kbf-usage/host/v1";

/// Bytes of the hash a fallback host name keeps (32 hex digits).
const HOST_HASH_BYTES: usize = 16;

fn text_field(hash: &mut Sha256, text: &str) {
    hash.update((text.len() as u64).to_be_bytes());
    hash.update(text.as_bytes());
}

/// The name of `key`'s profile: the SHA-256 of all five parts of the key, as 64 lowercase
/// hex digits (which satisfy the profile grammar, first byte included). Equal keys have
/// equal names, and a key that differs in the instance, the platform class, the basis, the
/// shape or the configuration (present or not) has another name. The submit and the lease
/// paths both name a profile with this function.
#[must_use]
pub fn profile_name(key: &Key) -> ProfileKey {
    let mut hash = Sha256::new();
    hash.update(PROFILE_DOMAIN);
    text_field(&mut hash, key.instance().as_str());
    text_field(&mut hash, key.platform().as_str());
    hash.update([match key.basis() {
        Basis::Cgroup => b'c',
        Basis::MacFootprint => b'm',
    }]);
    hash.update(key.shape().0.to_be_bytes());
    match key.cfg() {
        None => hash.update([0u8]),
        Some(Cfg(cfg)) => {
            hash.update([1u8]);
            hash.update(cfg.to_be_bytes());
        }
    }
    ProfileKey::parse(&hex::encode(hash.finalize()))
        .expect("64 lowercase hex digits are a profile key")
}

/// Names the recorder's host for a worker, and counts the workers it had to rename.
///
/// A worker id is not validated where it is minted, and a host id is 1 to 63 bytes of
/// `[a-z0-9._-]`. A host name here also starts with a lowercase letter or a digit. An id
/// that fits is used as it is; any other is replaced by `w-` and 32 hex digits of its
/// SHA-256, the same for every call. An id is never lowercased and its characters are never
/// swapped, which would give two workers one host.
#[derive(Default)]
pub struct HostNames {
    renamed: AtomicU64,
}

impl HostNames {
    /// A namer that has renamed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The host named for `worker`.
    pub fn host_name(&self, worker: &WorkerId) -> HostId {
        let text = worker.as_str();
        if let Some(host) = as_is(text) {
            return host;
        }
        self.renamed.fetch_add(1, Ordering::Relaxed);
        let mut hash = Sha256::new();
        hash.update(HOST_DOMAIN);
        hash.update(text.as_bytes());
        let kept: Vec<u8> = hash
            .finalize()
            .iter()
            .copied()
            .take(HOST_HASH_BYTES)
            .collect();
        let name = format!("w-{}", hex::encode(kept));
        HostId::new(&name).expect("w- and 32 hex digits are a host id")
    }

    /// How many calls so far gave a worker a name other than its own.
    #[must_use]
    pub fn renamed(&self) -> u64 {
        self.renamed.load(Ordering::Relaxed)
    }
}

/// The host `text` names itself, if it is one that starts with a letter or a digit.
fn as_is(text: &str) -> Option<HostId> {
    let first = *text.as_bytes().first()?;
    if first.is_ascii_lowercase() || first.is_ascii_digit() {
        HostId::new(text).ok()
    } else {
        None
    }
}
