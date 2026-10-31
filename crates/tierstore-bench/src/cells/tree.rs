//! The synthetic tree of cell G: 18,105 files of 344,233,431 bytes in all, from a seed.
//!
//! The sizes are the real tree's count and total, drawn from a heavy-tailed distribution (the
//! real sizes are not in this tool): every size is at least one byte, and the sizes sum to the
//! total exactly. The content of a file is seeded bytes, so a tree is never stored, only
//! regenerated, and its manifest holds digests only (path, size, SHA-256), which is all a reader
//! needs to check what it read.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::cells::content::{self, Rng};

/// Files in the tree of the paper.
pub const FILES: usize = 18_105;

/// Bytes in the tree of the paper.
pub const TOTAL_BYTES: u64 = 344_233_431;

/// Files in a directory of the tree.
const PER_DIR: usize = 100;

/// One file of the tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeFile {
    /// `dNNN/fNNNNN`: key-safe, relative.
    pub path: String,
    /// Size in bytes, at least one.
    pub size: u64,
}

/// A tree: its files and the seed its content comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// Seed of the content (and of the sizes).
    pub seed: u64,
    /// The files, in order.
    pub files: Vec<TreeFile>,
}

/// A line of the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// The file.
    pub path: String,
    /// Its size.
    pub size: u64,
    /// SHA-256 of its content, hex.
    pub sha256: String,
}

impl Tree {
    /// A tree of `files` files and `total` bytes from `seed` (`total` at least `files`, `files`
    /// at least one).
    #[must_use]
    pub fn generate(seed: u64, files: usize, total: u64) -> Self {
        let mut rng = Rng::new(seed);
        // Log-uniform over a range of 2^14: many small files, a tail of large ones.
        let weights: Vec<f64> = (0..files).map(|_| (rng.unit() * 14.0).exp2()).collect();
        let sum: f64 = weights.iter().sum();
        let spare = total - files as u64;
        let mut sizes: Vec<u64> = weights
            .iter()
            .map(|w| 1 + (w / sum * spare as f64) as u64)
            .collect();
        // The truncations left a remainder smaller than the number of files: one byte each to
        // the first files until the total is exact.
        let left = total - sizes.iter().sum::<u64>();
        for size in sizes
            .iter_mut()
            .take(usize::try_from(left).expect("a count"))
        {
            *size += 1;
        }
        let files = sizes
            .into_iter()
            .enumerate()
            .map(|(i, size)| TreeFile {
                path: format!("d{:03}/f{:05}", i / PER_DIR, i),
                size,
            })
            .collect();
        Self { seed, files }
    }

    /// The content of file `index`.
    #[must_use]
    pub fn content(&self, index: usize) -> Vec<u8> {
        content::fill(
            self.seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            self.files[index].size,
        )
    }

    /// The manifest: every file with the digest of its content. Reads the whole tree.
    #[must_use]
    pub fn manifest(&self) -> Vec<ManifestEntry> {
        self.files
            .iter()
            .enumerate()
            .map(|(i, file)| ManifestEntry {
                path: file.path.clone(),
                size: file.size,
                sha256: hex::encode(Sha256::digest(self.content(i))),
            })
            .collect()
    }

    /// Bytes in all.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a tree that is not the paper's: the count and the total are exact, no file is
    /// empty, the sizes are not all one value, and the same seed gives the same tree.
    #[test]
    fn the_paper_tree_has_its_count_and_exact_total() {
        let tree = Tree::generate(1, FILES, TOTAL_BYTES);
        assert_eq!(tree.files.len(), FILES);
        assert_eq!(tree.total(), TOTAL_BYTES);
        assert!(tree.files.iter().all(|f| f.size >= 1));
        let (min, max) = tree.files.iter().fold((u64::MAX, 0), |(lo, hi), f| {
            (lo.min(f.size), hi.max(f.size))
        });
        assert!(max > 100 * min, "{min}..{max}");
        assert_eq!(tree, Tree::generate(1, FILES, TOTAL_BYTES));
        assert_ne!(tree, Tree::generate(2, FILES, TOTAL_BYTES));
    }

    /// Catches paths that a key refuses or that collide: 100 files to a directory, five digits
    /// of index, all distinct.
    #[test]
    fn paths_are_key_safe_and_distinct() {
        let tree = Tree::generate(1, 250, 250_000);
        assert_eq!(tree.files[0].path, "d000/f00000");
        assert_eq!(tree.files[99].path, "d000/f00099");
        assert_eq!(tree.files[100].path, "d001/f00100");
        assert_eq!(tree.files[249].path, "d002/f00249");
        let all: std::collections::BTreeSet<&str> =
            tree.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(all.len(), 250);
        for file in &tree.files {
            assert!(objstore_s3::ObjectKey::new(format!("t/{}", file.path)).is_ok());
        }
    }

    /// Catches a total that is not met when every file is one byte, and one that is not met when
    /// there is one file.
    #[test]
    fn the_total_is_exact_at_the_edges() {
        assert_eq!(Tree::generate(1, 10, 10).total(), 10);
        assert_eq!(Tree::generate(1, 1, 12_345).files[0].size, 12_345);
    }

    /// Catches content that depends on anything but the seed, index and size, two files with one
    /// content, and a manifest that does not describe what `content` gives.
    #[test]
    fn content_and_manifest_agree() {
        let tree = Tree::generate(7, 20, 4000);
        assert_eq!(tree.content(3), tree.content(3));
        assert_ne!(tree.content(3), tree.content(4));
        assert_eq!(tree.content(3).len() as u64, tree.files[3].size);
        let manifest = tree.manifest();
        assert_eq!(manifest.len(), 20);
        let entry = &manifest[3];
        assert_eq!(entry.path, tree.files[3].path);
        assert_eq!(entry.size, tree.files[3].size);
        assert_eq!(entry.sha256, hex::encode(Sha256::digest(tree.content(3))));
        assert_eq!(entry.sha256.len(), 64);
        let json = serde_json::to_string(entry).unwrap();
        assert_eq!(
            serde_json::from_str::<ManifestEntry>(&json).unwrap(),
            *entry
        );
    }
}
