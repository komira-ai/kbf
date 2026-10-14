//! The disk layer of the local write tier (design `docs/design/write-path.md`
//! section 6, rollout stage 4): block files in `--write-tier-dir`, written in reserved
//! ranges, synced before anything in them is acknowledged, sealed, counted against
//! `--write-tier-bytes`, and recovered at a restart from the committed state.
//!
//! - A **block** is one file, `<epoch>-<seq>.blk`, named by the object it becomes. A
//!   blob's offset in the block is its offset in that object. Grouped writes share
//!   the one open block, each in a range reserved in order ([`WriteTier::reserve_group`]);
//!   a segment upload's segment is a block of its own ([`WriteTier::reserve_upload`]).
//! - **Durability.** [`WriteTier::sync`] runs `fdatasync` on the block's handle after
//!   the range's positioned writes, and `sync_dir` once per new block, before it
//!   returns the [`Synced`] a `PutStaged` may name. A write or sync error poisons the
//!   block: no later sync of it succeeds (a failed sync may have consumed another
//!   range's write-back error), and the grouped block is sealed.
//! - **Answers.** A range is answered when its `PutStaged` commits
//!   ([`WriteTier::committed`]: the committed length becomes the largest committed end,
//!   so out-of-order commits never move it back) or fails (the reservation dropped: a
//!   grouped range's bytes stay as dead bytes; an upload's block is deleted).
//! - **Seal.** The open grouped block is sealed at the segment limit or after
//!   `--write-tier-seal-after`: it takes no new range, and once every range is
//!   answered [`WriteTier::poll_seals`] names it for `SealStaged`. A block whose ranges
//!   all failed is deleted instead.
//! - **Budget.** Every live block's reserved extent counts, from reservation until
//!   [`WriteTier::unstage`] (after its flush) or deletion. A range that does not fit is
//!   refused with [`TierError::Full`]: the caller takes the direct path.
//! - **Reads** never pass a block's committed length: bytes no commit named were never
//!   acknowledged and are never served.
//! - **Restart** ([`Tier::open`]): the `TIER` file check, each staged object's block
//!   cut to its committed length (or reported lost), and every other block deleted
//!   (the orphan sweep). The committed state comes in through [`StagedIndex`].
//!
//! Not here (rollout stage 4b): the flusher, local reads through the metadata,
//! eviction of flushed copies, `--write-tier-abandon`, metrics. All disk access goes
//! through [`kbf_store::Fs`], so the tests crash it with [`kbf_store::FaultFs`].

mod args;
mod block;
mod error;
mod open;
mod tier;

pub use crate::args::TierArgs;
pub use crate::block::{TIER, TierId, block_name, parse_block_name};
pub use crate::error::TierError;
pub use crate::open::{Recovery, StagedIndex, StagedObject, TierStart};
pub use crate::tier::{
    NextSeq, Reservation, Synced, Tier, TierOptions, Unsynced, Usage, WriteTier,
};
