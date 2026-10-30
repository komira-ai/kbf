//! The order in which the endpoints are tried for a key: rendezvous (highest-random-weight)
//! hashing.
//!
//! The weight of an endpoint for a key is the first eight bytes, read as a big-endian number, of
//! the SHA-256 of the endpoint's name (the URL as the file gives it), a zero byte and the key.
//! The endpoints are tried from the greatest weight to the least; two weights that are equal
//! (which two distinct names do not give in practice) are told apart by the place in the file.
//!
//! The weight is a pure function of the two names, so the order is the same in every process
//! and every release, and it does not depend on the order of the endpoints in the file. A
//! change of the set moves only the keys it must: when an endpoint leaves, a key whose first
//! choice was another endpoint keeps it, and the keys of the one that left spread over the rest
//! by their second choices; when one joins, it takes the keys it now weighs most for and no
//! other key moves. A hash that is not fixed (`std`'s `DefaultHasher` is allowed to change) would
//! move every key at a release.

use sha2::{Digest, Sha256};

/// The weight of `endpoint` for `key`.
pub(super) fn weight(endpoint: &str, key: &str) -> u64 {
    let mut hash = Sha256::new();
    hash.update(endpoint.as_bytes());
    hash.update([0]);
    hash.update(key.as_bytes());
    let digest = hash.finalize();
    digest
        .iter()
        .take(8)
        .fold(0, |weight, byte| (weight << 8) | u64::from(*byte))
}

/// The places of `names` in the order they are tried for `key`: the first is the endpoint of
/// the greatest weight.
pub(super) fn rank(names: &[String], key: &str) -> Vec<usize> {
    let mut order: Vec<(u64, usize)> = names
        .iter()
        .enumerate()
        .map(|(place, name)| (weight(name, key), place))
        .collect();
    order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    order.into_iter().map(|(_, place)| place).collect()
}
