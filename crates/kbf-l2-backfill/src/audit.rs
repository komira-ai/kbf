//! The live-key sample for `tierstore store audit --keys` (tierstore's
//! `docs/design/fetch-cache-serve.md`, 10.8): a uniform random sample of the keys of the packs
//! the pack index holds live, taken in one pass over the index.
//!
//! A key's rank is the SHA-256 of the seed's 8 bytes, little endian, followed by the key; the
//! sample is the `n` keys of least rank. The ranks of distinct keys are independent and uniform
//! as far as SHA-256 is, so every set of `n` live keys is as likely as any other, the same seed
//! over the same keys gives the same sample, and a pass needs only the `n` least ranks seen.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use kbf_pack_index::cells::MAX_PAGE;
use kbf_pack_index::{PackIndex, PackIndexError};
use sha2::{Digest as _, Sha256};

/// The rank of `key` under `seed`.
#[must_use]
pub fn rank(seed: u64, key: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(seed.to_le_bytes());
    hash.update(key.as_bytes());
    hash.finalize().into()
}

/// The `n` keys of least rank among those offered, kept as they come.
pub struct Selector {
    seed: u64,
    n: NonZeroUsize,
    kept: BTreeSet<([u8; 32], String)>,
}

impl Selector {
    /// A selection of `n` keys under `seed`.
    #[must_use]
    pub fn new(seed: u64, n: NonZeroUsize) -> Self {
        Self {
            seed,
            n,
            kept: BTreeSet::new(),
        }
    }

    /// Offers `key`: kept if it is among the `n` of least rank so far.
    pub fn offer(&mut self, key: &str) {
        self.kept.insert((rank(self.seed, key), key.to_owned()));
        if self.kept.len() > self.n.get() {
            self.kept.pop_last();
        }
    }

    /// The keys kept, sorted by key.
    #[must_use]
    pub fn into_keys(self) -> Vec<String> {
        let mut keys: Vec<String> = self.kept.into_iter().map(|(_, key)| key).collect();
        keys.sort();
        keys
    }
}

/// What a sweep found.
#[derive(Debug, PartialEq, Eq)]
pub struct Sampled {
    /// The packs the index holds live.
    pub live_packs: u64,
    /// The sample, sorted by key.
    pub keys: Vec<String>,
}

/// Sweeps the live packs of `index`, page by page, and samples `n` of their keys under `seed`.
///
/// # Errors
/// [`PackIndexError`] from a page.
pub fn sweep(index: &dyn PackIndex, seed: u64, n: NonZeroUsize) -> Result<Sampled, PackIndexError> {
    let mut selector = Selector::new(seed, n);
    let mut live_packs = 0_u64;
    let mut after: Option<String> = None;
    loop {
        let page = index.live_packs(after.as_deref(), MAX_PAGE)?;
        let Some(last) = page.last() else { break };
        after = Some(last.key.clone());
        for pack in &page {
            selector.offer(&pack.key);
            live_packs += 1;
        }
    }
    Ok(Sampled {
        live_packs,
        keys: selector.into_keys(),
    })
}

/// The sample as the file `tierstore store audit --keys` reads: one key per line, each ended by
/// `\n`, sorted by key; no keys make an empty file.
#[must_use]
pub fn file_text(keys: &[String]) -> String {
    keys.iter().map(|key| format!("{key}\n")).collect()
}
