//! The machine version of the replicated log (docs/design/ha.md, section 4.7) and its
//! conversion from the `kbf.log.v1` message.

use std::num::NonZeroU32;

use kbf_proto::kbf::log::v1 as proto;

use crate::DecodeError;
use crate::Format;

/// A machine version (docs/design/ha.md, section 4.7): which apply behavior runs.
/// Never zero. No binary of this build raises its log to one: formats change additively,
/// so a new command needs no raise. The versions stay for the logs that hold a raise an
/// earlier build wrote, which a replica still reads (and skips, when it names a version
/// past what it runs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineVersion(NonZeroU32);

impl MachineVersion {
    /// Version 1: the metadata log's first apply behavior, whose entries are
    /// [`Format::V1`].
    pub const V1: Self = Self(NonZeroU32::MIN);

    /// Version 2: version 1 plus the write tier's metadata commands, whose entries are
    /// [`Format::V2`] (docs/design/write-path.md, section 6.5). A log enters it by a
    /// committed `MachineVersion` entry naming it; every later entry is format 2.
    pub const V2: Self = Self(NonZeroU32::MIN.saturating_add(1));

    /// Version 3: version 2 plus `EvictObjects`, whose entries are [`Format::V3`]. A log
    /// enters it as it enters version 2.
    pub const V3: Self = Self(NonZeroU32::MIN.saturating_add(2));

    /// Version 4: version 3 plus node writers (`SetWriter`, and objects named by a
    /// storage node's member and boot), whose entries are [`Format::V4`]. A log enters it
    /// as it enters version 2.
    pub const V4: Self = Self(NonZeroU32::MIN.saturating_add(3));

    /// Version 5: version 4 plus `ReseatStaged` (a conflicting staged object's records
    /// moved to a new object in one step), whose entries are [`Format::V5`]. A log
    /// enters it as it enters version 2.
    pub const V5: Self = Self(NonZeroU32::MIN.saturating_add(4));

    /// The entry format a log at this version writes and reads; `None` for a version
    /// this binary does not know (a later binary's).
    #[must_use]
    pub const fn format(self) -> Option<Format> {
        match self.0.get() {
            1 => Some(Format::V1),
            2 => Some(Format::V2),
            3 => Some(Format::V3),
            4 => Some(Format::V4),
            5 => Some(Format::V5),
            _ => None,
        }
    }

    /// Version `n`.
    #[must_use]
    pub const fn new(n: NonZeroU32) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

pub(crate) fn machine_version_from_proto(
    v: proto::MachineVersion,
) -> Result<MachineVersion, DecodeError> {
    NonZeroU32::new(v.version)
        .map(MachineVersion)
        .ok_or(DecodeError::Missing("MachineVersion.version"))
}
