//! Opening a tier directory: the `TIER` check, recovery of staged blocks from the
//! committed state, and the orphan sweep (design section 6.4).

use std::collections::{BTreeMap, BTreeSet};

use kbf_meta::{Epoch, ObjectId};
use kbf_store::{Fs, FsFile};

use crate::block::{TIER, TIER_TMP, TierId, block_name, parse_block_name};
use crate::error::TierError;
use crate::tier::{Block, NextSeq, Phase, State, Tier, TierOptions};

/// One object the committed state holds as staged on a tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagedObject {
    /// The object, and so the block file `<epoch>-<seq>.blk`.
    pub object: ObjectId,
    /// Its committed length: the largest `end` a committed `PutStaged` gave it.
    pub len: u64,
    /// Whether its `SealStaged` (or its segment upload's `PutStaged`) committed.
    pub sealed: bool,
}

/// The committed state's staged table, as the tier reads it at open. The metadata
/// (rollout stage 3's staged table) implements it; the tier trusts nothing else about
/// which blocks hold acknowledged bytes, so a block it does not name is an orphan.
pub trait StagedIndex {
    /// Every object staged (not yet flushed) on `tier`.
    fn staged_on(&self, tier: TierId) -> Vec<StagedObject>;
}

/// Who the tier is at this start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierStart {
    /// This server's tier id, as the committed state holds it.
    pub tier: TierId,
    /// This start's writer epoch. Every block of an earlier start is closed to appends.
    pub epoch: Epoch,
    /// The sizes and delays.
    pub options: TierOptions,
}

/// What [`Tier::open`] found and did, for the caller to commit and report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Staged objects whose block file is missing or shorter than the committed
    /// length: commit `ObjectUnreachable { Missing }` for each, and alert.
    pub lost: Vec<ObjectId>,
    /// Staged objects whose file held bytes past the committed length, cut off (they
    /// were never acknowledged).
    pub truncated: Vec<ObjectId>,
    /// Staged objects not yet sealed: [`WriteTier::poll_seals`](crate::WriteTier::poll_seals)
    /// names them until their `SealStaged` commits.
    pub unsealed: Vec<ObjectId>,
    /// Staged, sealed objects: for the flusher.
    pub sealed: Vec<ObjectId>,
    /// Block files deleted as orphans: no staged object names them.
    pub removed: Vec<String>,
}

impl<F> Tier<F>
where
    F: Fs + Send + Sync + std::fmt::Debug + 'static,
    F::File: Send + Sync + std::fmt::Debug + 'static,
{
    /// Opens the tier directory `fs` for this start.
    ///
    /// 1. `TIER` must name `start.tier`, unless nothing is staged on `start.tier` and
    ///    nothing on the tier `TIER` names; then `TIER` is (re)written, synced and its
    ///    name synced. Otherwise the start is refused, so a wrong mount or flag never
    ///    destroys the index (`--write-tier-abandon` is the caller's, before this).
    /// 2. Each staged object's block is kept if it holds at least the committed length,
    ///    cut to that length and synced if it holds more; else it is reported lost.
    /// 3. Every other block file is an orphan and deleted, and the directory synced.
    ///    `.seg` files and names the tier does not write are left alone.
    ///
    /// `next_seq` numbers the objects of new blocks within `start.epoch`.
    ///
    /// # Errors
    ///
    /// [`TierError::WrongTier`], [`TierError::OtherTier`], [`TierError::FutureBlock`]
    /// (nothing is changed on disk then), or [`TierError::Io`].
    pub fn open(
        fs: F,
        start: TierStart,
        index: &dyn StagedIndex,
        next_seq: NextSeq,
    ) -> Result<(Self, Recovery), TierError> {
        let names: BTreeSet<String> = fs
            .list()
            .map_err(|e| TierError::io("list", ".", e))?
            .into_iter()
            .collect();
        let staged: BTreeMap<ObjectId, StagedObject> = index
            .staged_on(start.tier)
            .into_iter()
            .map(|s| (s.object, s))
            .collect();
        if let Some(name) = names
            .iter()
            .find(|n| parse_block_name(n).is_some_and(|o| o.epoch() >= start.epoch))
        {
            return Err(TierError::FutureBlock { name: name.clone() });
        }
        check_tier_file(&fs, &names, start.tier, &staged, index)?;

        let mut recovery = Recovery::default();
        let mut state = State::new(0);
        for s in staged.values() {
            let name = block_name(s.object);
            let len = if names.contains(&name) {
                fs.file_len(&name)
                    .map_err(|e| TierError::io("stat", &name, e))?
            } else {
                0
            };
            if !names.contains(&name) || len < s.len {
                recovery.lost.push(s.object);
                continue;
            }
            if len > s.len {
                let mut file = fs
                    .open_append(&name)
                    .map_err(|e| TierError::io("open", &name, e))?;
                file.truncate(s.len)
                    .and_then(|()| file.sync())
                    .map_err(|e| TierError::io("truncate", &name, e))?;
                recovery.truncated.push(s.object);
            }
            let phase = if s.sealed {
                recovery.sealed.push(s.object);
                Phase::Sealed
            } else {
                recovery.unsealed.push(s.object);
                Phase::SealReady
            };
            state
                .blocks
                .insert(s.object, Block::recovered(s.len, phase));
            state.used += s.len;
        }

        for name in &names {
            if parse_block_name(name).is_some_and(|o| !staged.contains_key(&o)) {
                fs.remove(name)
                    .map_err(|e| TierError::io("remove", name, e))?;
                recovery.removed.push(name.clone());
            }
        }
        if !recovery.removed.is_empty() {
            fs.sync_dir()
                .map_err(|e| TierError::io("sync_dir", ".", e))?;
        }
        let tier = Self::from_parts(fs, start.options, start.epoch, state, next_seq);
        Ok((tier, recovery))
    }
}

/// Step 1 of [`Tier::open`].
fn check_tier_file<F: Fs>(
    fs: &F,
    names: &BTreeSet<String>,
    tier: TierId,
    staged: &BTreeMap<ObjectId, StagedObject>,
    index: &dyn StagedIndex,
) -> Result<(), TierError> {
    let found = if names.contains(TIER) {
        let bytes = fs.read(TIER).map_err(|e| TierError::io("read", TIER, e))?;
        TierId::parse_file(&bytes)
    } else {
        None
    };
    if found == Some(tier) {
        return Ok(());
    }
    if !staged.is_empty() {
        return Err(TierError::WrongTier {
            expected: tier,
            found,
        });
    }
    if let Some(other) = found.filter(|other| !index.staged_on(*other).is_empty()) {
        return Err(TierError::OtherTier { found: other });
    }
    let io = |op, e| TierError::io(op, TIER, e);
    if names.contains(TIER_TMP) {
        fs.remove(TIER_TMP).map_err(|e| io("remove", e))?;
    }
    let mut file = fs.create(TIER_TMP).map_err(|e| io("create", e))?;
    file.append(&tier.file_bytes())
        .and_then(|()| file.sync())
        .map_err(|e| io("write", e))?;
    fs.rename(TIER_TMP, TIER).map_err(|e| io("rename", e))?;
    fs.sync_dir().map_err(|e| io("sync_dir", e))
}
