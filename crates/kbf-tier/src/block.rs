//! The names in a tier directory and the `TIER` file.

use std::fmt;

use kbf_meta::{Epoch, ObjectId};

/// The file that names the tier a directory belongs to.
pub const TIER: &str = "TIER";

/// Where the `TIER` file is written before it is renamed into place.
pub(crate) const TIER_TMP: &str = "TIER.tmp";

/// A write tier's id: which tier an object is staged on.
///
/// The metadata hands it out once per directory (`Command::AddTier`, rollout stage 3)
/// and the tier writes it to `<dir>/TIER`, so a start can tell its own directory from
/// a wrong mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TierId(u64);

impl TierId {
    /// Tier number `n`.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The content of a `TIER` file naming this tier.
    pub(crate) fn file_bytes(self) -> Vec<u8> {
        format!("{}\n", self.0).into_bytes()
    }

    /// The tier a `TIER` file names, or `None` if its bytes are not one (a damaged or
    /// foreign file).
    pub(crate) fn parse_file(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let id = Self(text.strip_suffix('\n')?.parse().ok()?);
        (id.file_bytes() == bytes).then_some(id)
    }
}

impl fmt::Display for TierId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The block file of `object`: `<epoch>-<seq>.blk`, each as 16 lowercase hex digits,
/// as in the object's key in the store.
#[must_use]
pub fn block_name(object: ObjectId) -> String {
    format!("{:016x}-{:016x}.blk", object.epoch().get(), object.seq())
}

/// The object a block file name names, or `None` if `name` is not exactly the name
/// [`block_name`] gives some object.
#[must_use]
pub fn parse_block_name(name: &str) -> Option<ObjectId> {
    let (epoch, seq) = name.strip_suffix(".blk")?.split_once('-')?;
    let epoch = u64::from_str_radix(epoch, 16).ok()?;
    let seq = u64::from_str_radix(seq, 16).ok()?;
    let object = ObjectId::new(Epoch::new(epoch), seq);
    (block_name(object) == name).then_some(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a block name that does not round-trip (a restart would treat every
    /// block as foreign and leave it, or as an orphan of the wrong object), and a
    /// parser that takes a name the tier never writes (`+`, upper case, short fields,
    /// other suffixes) and so deletes a file that is not its own.
    #[test]
    fn block_names_round_trip_and_nothing_else_parses() {
        let id = ObjectId::new(Epoch::new(0xab), 7);
        assert_eq!(block_name(id), "00000000000000ab-0000000000000007.blk");
        assert_eq!(parse_block_name(&block_name(id)), Some(id));
        for bad in [
            "00000000000000AB-0000000000000007.blk",
            "+0000000000000ab-0000000000000007.blk",
            "ab-7.blk",
            "00000000000000ab-0000000000000007.seg",
            "00000000000000ab0000000000000007.blk",
            "00000000000000ag-0000000000000007.blk",
            "00000000000000ab-000000000000000g.blk",
        ] {
            assert_eq!(parse_block_name(bad), None, "{bad}");
        }
    }

    /// Catches: a `TIER` file that does not round-trip, and one whose damaged or
    /// foreign content is read as some tier id (a wrong mount would then pass).
    #[test]
    fn tier_files_round_trip_and_damage_is_none() {
        let id = TierId::new(42);
        assert_eq!(id.to_string(), "42");
        assert_eq!(id.get(), 42);
        assert_eq!(TierId::parse_file(&id.file_bytes()), Some(id));
        for bad in [&b"42"[..], b"042\n", b"x\n", b"\xff\n", b"42\n\n", b""] {
            assert_eq!(TierId::parse_file(bad), None, "{bad:?}");
        }
    }
}
