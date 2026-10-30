//! The encoding of the replicated log's command entries (docs/design/ha.md, section
//! 4.6): conversions between the domain values and the `kbf.log.v1` messages of
//! `kbf-proto`, so the domain crates (`kbf-meta`, `kbf-sched`) stay free of codec
//! dependencies. The metadata commands are `kbf_meta`'s; the control commands and the
//! values they carry are in this crate ([`FarmBatch`], [`Report`], [`Takeover`],
//! [`Checkpoint`], [`MachineVersion`]).
//!
//! [`encode`] and [`decode`] take the log's committed [`Format`]: [`Format::V1`];
//! [`Format::V2`], which added the write tier's metadata commands
//! (docs/design/write-path.md, section 6.5); [`Format::V3`], which added `EvictObjects`;
//! [`Format::V4`], which added node writers (`SetWriter`, and object ids naming a storage
//! node's member and boot) and the commands of a rebuild of the index; or [`Format::V5`],
//! which added `ReseatStaged`. Any other format is refused, and that is all either takes
//! it for: what [`encode`] writes and what [`decode`] reads never depend on it, so every
//! command this binary knows is written and read under every format. A command is not a
//! reason to raise a log's format: a binary writes every command it knows to a log of any
//! format, and a reader that does not know one skips it (below). The formats stay so
//! that a log which holds a [`MachineVersion`] entry, written by a build that raised its
//! log, still replays.
//!
//! Every format changes additively, and a reader skips what it does not know instead of
//! refusing it. A later binary may add a field to a message, a command to a `oneof`, or a
//! new record kind; it may not renumber, remove or reinterpret anything, add a value to
//! an enum a reader acts on (it adds a field instead), or make a field required. So
//! [`decode`] skips:
//! - a field no message of this binary names: prost drops it, and the part this binary
//!   knows decodes as it would without the field;
//! - a `oneof` that is unset, because its arm is one this binary does not name (a
//!   command, a farm input, a record, an outcome, a QoS kind): the entry decodes as
//!   [`LogCommand::Unknown`];
//! - an enum value that is unspecified (zero) or unnamed, in a command this binary
//!   knows: the entry decodes as [`LogCommand::Unknown`] as well;
//! - a consumable amount whose consumable is unspecified or unnamed: that amount is left
//!   out of the node's capabilities and the rest of the command decodes as it would
//!   without it;
//! - a field absent that a later build added, which reads as its zero: a `Resources` without
//!   `disk_bytes` (field 6, added after format 1; zero is not written) is a vector that books
//!   no disk, and the bytes of a vector with none are the bytes it always had;
//! - the bytes [`encode`] would not have written (an explicit zero, a varint that is not
//!   minimal, a field repeated or out of order): the value they stand for is decoded.
//!
//! What a host does with a [`LogCommand::Unknown`] entry is its own decision; this crate
//! only reports it.
//!
//! [`decode`] still refuses an entry that is wrong in a way no later format makes right
//! (a [`DecodeError`] other than `UnknownCommand` and `UnknownValue`):
//! - bytes that are not a well-formed message;
//! - a required message field that is absent, a log id, term or machine version that is
//!   zero, or a hash that is not 32 bytes;
//! - a set (a closure, a touch) that is not in strictly increasing order;
//! - an object id that names no one writer, a store id or an urgency past 16 bits;
//! - a value its own type refuses (a custom QoS level, a capability request), as the
//!   type's own constructor says.
//!
//! An error is fail-stop for the replica that meets it.

mod control;
mod meta;

use std::fmt;
use std::num::NonZeroU64;

pub use control::{
    ActionEntry, Checkpoint, FarmBatch, FarmInput, MachineVersion, Report, Takeover,
};
use kbf_meta::Command;
use kbf_proto::kbf::log::v1 as proto;
use prost::Message;
use proto::log_command::Command as P;

/// Which entry encodings a log was raised to: the format of the highest committed
/// [`MachineVersion`] entry. Neither encoding nor decoding depends on it, and no binary
/// of this build raises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Format(u32);

impl Format {
    /// `kbf.log.v1` as `log.proto` has it at format 1.
    pub const V1: Self = Self(1);

    /// Format 1 plus the write tier's metadata commands: `AddTier`, `PutStaged`,
    /// `SealStaged`, `ObjectFlushed` and `FlushConflict`.
    pub const V2: Self = Self(2);

    /// Format 2 plus `EvictObjects`.
    pub const V3: Self = Self(3);

    /// Format 3 plus node writers: `SetWriter`, and an object id that names a
    /// registered storage node's member and boot instead of an epoch, in any command;
    /// and the commands of a rebuild of the index (`AdoptSegment`, `RebuildProgress`,
    /// `RebuildDone`).
    pub const V4: Self = Self(4);

    /// Format 4 plus `ReseatStaged`.
    pub const V5: Self = Self(5);

    /// Format number `n`, which may not be one this binary supports.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Whether this binary writes and reads the format.
    const fn known(self) -> bool {
        matches!(self, Self::V1 | Self::V2 | Self::V3 | Self::V4 | Self::V5)
    }
}

/// The id of one replicated log: drawn once, when the log is created, and committed in
/// its [`Bootstrap`] (docs/design/ha.md, section 4.5). Never zero. It names the log, not
/// a position in it (that is `kbf_raft::LogId`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FarmLogId(NonZeroU64);

impl FarmLogId {
    /// The id `n`.
    #[must_use]
    pub const fn new(n: NonZeroU64) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// The raw number, as the non-zero number it is.
    #[must_use]
    pub const fn non_zero(self) -> NonZeroU64 {
        self.0
    }
}

/// Sixteen lowercase hex digits, the form object key prefixes use.
impl fmt::Display for FarmLogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A log's first command (docs/design/ha.md, section 4.11): its id and the object key
/// prefix every start over it writes under. A replica takes the first committed one; a
/// later one changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bootstrap {
    /// The log's id.
    pub log_id: FarmLogId,
    /// The key prefix of every object the farm writes, the same across restarts.
    pub store_prefix: String,
}

/// The payload of a command entry (docs/design/ha.md, section 4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogCommand {
    /// A command of the metadata state machine.
    Meta(Command),
    /// The log's first command.
    Bootstrap(Bootstrap),
    /// Scheduler and farm inputs, batched.
    Farm(FarmBatch),
    /// A worker's result and the action-cache entry it writes.
    Report(Report),
    /// A new leader's first command.
    Takeover(Takeover),
    /// A determinism check.
    Checkpoint(Checkpoint),
    /// Raises the machine version. A log written by an earlier build holds these; this
    /// build writes none outside its tests.
    MachineVersion(MachineVersion),
    /// A command this binary does not know: a `oneof` arm that no message of this binary
    /// names, or an enum value it does not know inside a command it does. The entry's
    /// bytes are not kept, so it cannot be encoded.
    Unknown,
}

/// Why an entry could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// This binary does not write the format.
    #[error("log format {} is not supported", .0.get())]
    Format(Format),
    /// A digest names a hash function the format has no value for.
    #[error("a digest names a hash function log format 1 cannot encode")]
    DigestFunction,
    /// A request's work key is not an action's (its dedup text is not an action digest),
    /// which is all the format has a message for.
    #[error("a request's work key is not an action digest, which log format 1 requires")]
    WorkKey,
    /// A control record is of a kind the format has no message for.
    #[error("a control record is of a kind log format 1 cannot encode")]
    Record,
    /// A batch's wall-clock time is before the Unix epoch, or more than `u64::MAX`
    /// nanoseconds after it.
    #[error("a batch's wall-clock time is not a u64 count of nanoseconds since the Unix epoch")]
    WallTime,
    /// A [`LogCommand::Unknown`] has no encoding.
    #[error("a command this binary does not know has no encoding")]
    Unknown,
}

/// Why an entry could not be decoded. Every case stops the replica.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// This binary does not read the format.
    #[error("log format {} is not supported", .0.get())]
    Format(Format),
    /// The bytes are not a well-formed message.
    #[error("malformed entry: {0}")]
    Malformed(#[from] prost::DecodeError),
    /// A `oneof` is unset: a command or variant this binary does not know. [`decode`]
    /// reads such an entry as [`LogCommand::Unknown`] and never returns this.
    #[error("{0} names no command or variant this binary knows")]
    UnknownCommand(&'static str),
    /// An enum value that is unspecified or that this binary does not know. [`decode`]
    /// reads such an entry as [`LogCommand::Unknown`] and never returns this.
    #[error("{field} has value {value}, which is unspecified or unknown")]
    UnknownValue {
        /// The field, as `Message.field`.
        field: &'static str,
        /// The value found.
        value: i32,
    },
    /// A required message field is absent.
    #[error("{0} is absent")]
    Missing(&'static str),
    /// A digest's hash is not 32 bytes.
    #[error("{field} has a {len}-byte hash; 32 bytes expected")]
    HashLength {
        /// The field.
        field: &'static str,
        /// The length found.
        len: usize,
    },
    /// An object id names both an epoch and a node writer, or a node writer with a zero
    /// member or boot.
    #[error("{0} names no writer: an epoch alone, or a member and a boot alone")]
    Writer(&'static str),
    /// A store id does not fit in 16 bits.
    #[error("Location.store {0} does not fit in 16 bits")]
    Store(u32),
    /// A custom QoS level's urgency does not fit in 16 bits.
    #[error("CustomQos.urgency {0} does not fit in 16 bits")]
    Urgency(u32),
    /// A custom QoS level that `Qos::custom` refuses.
    #[error("invalid QoS level: {0}")]
    Qos(#[from] kbf_types::QosError),
    /// A capability request that `kbf_caps::Request::parse` refuses.
    #[error("invalid capability request: {0}")]
    Request(#[from] kbf_caps::RequestError),
    /// A set is out of order or repeats an element.
    #[error("{0} is not in strictly increasing order")]
    SetOrder(&'static str),
}

/// Encodes `command` under `format`, which must be one this binary knows and which
/// nothing else depends on: every command this binary knows encodes under every format it
/// knows, to the same bytes.
pub fn encode(format: Format, command: &LogCommand) -> Result<Vec<u8>, EncodeError> {
    if !format.known() {
        return Err(EncodeError::Format(format));
    }
    let command = match command {
        LogCommand::Meta(c) => P::Meta(meta::to_proto(c)?),
        LogCommand::Bootstrap(b) => P::Bootstrap(proto::Bootstrap {
            log_id: b.log_id.get(),
            store_prefix: b.store_prefix.clone(),
        }),
        LogCommand::Farm(b) => P::Farm(control::batch_to_proto(b)?),
        LogCommand::Report(r) => P::Report(control::report_to_proto(r)?),
        LogCommand::Takeover(t) => P::Takeover(control::takeover_to_proto(t)),
        LogCommand::Checkpoint(c) => P::Checkpoint(control::checkpoint_to_proto(c)),
        LogCommand::MachineVersion(v) => {
            P::MachineVersion(proto::MachineVersion { version: v.get() })
        }
        LogCommand::Unknown => return Err(EncodeError::Unknown),
    };
    let message = proto::LogCommand {
        command: Some(command),
    };
    Ok(message.encode_to_vec())
}

/// Decodes an entry's payload. `format` must be one this binary knows, and nothing else
/// depends on it: every command this binary knows decodes under every format. A command
/// or value it does not know decodes as [`LogCommand::Unknown`], and a field it does not
/// know is ignored; the crate documentation lists what is still refused.
pub fn decode(format: Format, bytes: &[u8]) -> Result<LogCommand, DecodeError> {
    if !format.known() {
        return Err(DecodeError::Format(format));
    }
    match command(bytes) {
        Err(DecodeError::UnknownCommand(_) | DecodeError::UnknownValue { .. }) => {
            Ok(LogCommand::Unknown)
        }
        decoded => decoded,
    }
}

/// The command in `bytes`; an unset `oneof` or an unknown enum value anywhere in it is
/// the error [`decode`] reads as [`LogCommand::Unknown`].
fn command(bytes: &[u8]) -> Result<LogCommand, DecodeError> {
    Ok(match proto::LogCommand::decode(bytes)?.command {
        Some(P::Meta(c)) => LogCommand::Meta(meta::from_proto(c)?),
        Some(P::Bootstrap(b)) => LogCommand::Bootstrap(Bootstrap {
            log_id: NonZeroU64::new(b.log_id)
                .map(FarmLogId)
                .ok_or(DecodeError::Missing("Bootstrap.log_id"))?,
            store_prefix: b.store_prefix,
        }),
        Some(P::Farm(b)) => LogCommand::Farm(control::batch_from_proto(b)?),
        Some(P::Report(r)) => LogCommand::Report(control::report_from_proto(r)?),
        Some(P::Takeover(t)) => LogCommand::Takeover(control::takeover_from_proto(t)?),
        Some(P::Checkpoint(c)) => LogCommand::Checkpoint(control::checkpoint_from_proto(c)?),
        Some(P::MachineVersion(v)) => {
            LogCommand::MachineVersion(control::machine_version_from_proto(v)?)
        }
        None => return Err(DecodeError::UnknownCommand("LogCommand")),
    })
}
