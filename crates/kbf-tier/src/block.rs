//! The names in a tier directory and the `TIER` file.

use kbf_meta::{Epoch, ObjectId, TierId};

/// The file that names the tier a directory belongs to.
pub const TIER: &str = "TIER";

/// Where the `TIER` file is written before it is renamed into place.
pub(crate) const TIER_TMP: &str = "TIER.tmp";

/// The content of a `TIER` file naming `tier`.
pub(crate) fn tier_file_bytes(tier: TierId) -> Vec<u8> {
    format!("{}\n", tier.get()).into_bytes()
}

/// The tier a `TIER` file names, or `None` if its bytes are not one (a damaged or
/// foreign file).
pub(crate) fn parse_tier_file(bytes: &[u8]) -> Option<TierId> {
    let text = std::str::from_utf8(bytes).ok()?;
    let id = TierId::new(text.strip_suffix('\n')?.parse().ok()?);
    (tier_file_bytes(id) == bytes).then_some(id)
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

/// The flushed copy of `object`: its block renamed once its `ObjectFlushed`
/// committed, `<epoch>-<seq>.seg`.
#[must_use]
pub fn seg_name(object: ObjectId) -> String {
    format!("{:016x}-{:016x}.seg", object.epoch().get(), object.seq())
}

/// The object a flushed copy's name names, or `None` if `name` is not exactly the
/// name [`seg_name`] gives some object.
#[must_use]
pub fn parse_seg_name(name: &str) -> Option<ObjectId> {
    let stem = name.strip_suffix(".seg")?;
    parse_block_name(&format!("{stem}.blk")).filter(|o| seg_name(*o) == name)
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
        assert_eq!(seg_name(id), "00000000000000ab-0000000000000007.seg");
        assert_eq!(parse_seg_name(&seg_name(id)), Some(id));
        assert_eq!(parse_seg_name(&block_name(id)), None);
        assert_eq!(
            parse_seg_name("00000000000000AB-0000000000000007.seg"),
            None
        );
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
        assert_eq!(tier_file_bytes(id), b"42\n");
        assert_eq!(parse_tier_file(&tier_file_bytes(id)), Some(id));
        for bad in [&b"42"[..], b"042\n", b"x\n", b"\xff\n", b"42\n\n", b""] {
            assert_eq!(parse_tier_file(bad), None, "{bad:?}");
        }
    }
}
