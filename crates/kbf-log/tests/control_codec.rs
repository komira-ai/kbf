//! The `kbf.log.v1` machine-version command, and the farm commands of earlier builds.
//!
//! Catches:
//! - a conversion that loses a machine version, or a decoder that accepts a zero one;
//! - a farm command (`Farm`, `Report`, `Takeover`, `Checkpoint`), whatever it holds, that
//!   is read as a value or refused instead of skipped as an unknown command;
//! - a command written or read under a format this build does not know.

mod control_support;

use std::num::NonZeroU32;

use control_support::{dec, enc};
use kbf_log::{DecodeError, EncodeError, Format, LogCommand, MachineVersion, encode};
use kbf_proto::kbf::log::v1 as proto;
use prost::Message;

const UNKNOWN: Result<LogCommand, DecodeError> = Ok(LogCommand::Unknown);

fn wrap(command: proto::log_command::Command) -> Vec<u8> {
    proto::LogCommand {
        command: Some(command),
    }
    .encode_to_vec()
}

/// Asserts that a machine version round trips, for versions this build knows and one it
/// does not.
#[test]
fn a_machine_version_round_trips() {
    for n in [1, 2, 3, 4, 5, 6, u32::MAX] {
        let version = MachineVersion::new(NonZeroU32::new(n).expect("non-zero"));
        let command = LogCommand::MachineVersion(version);
        assert_eq!(dec(&enc(&command)), Ok(command), "version {n}");
    }
}

/// Catches a decoder that fills in an absent machine version (proto3's zero).
#[test]
fn a_zero_machine_version_is_refused() {
    let bytes = wrap(proto::log_command::Command::MachineVersion(
        proto::MachineVersion { version: 0 },
    ));
    assert_eq!(
        dec(&bytes),
        Err(DecodeError::Missing("MachineVersion.version"))
    );
}

/// Asserts that each arm of a farm command decodes as an unknown command, filled in or
/// empty, and not as a value or a refusal: the entry is skipped as a whole.
#[test]
fn farm_commands_decode_as_unknown() {
    use proto::log_command::Command as C;
    let commands = [
        C::Farm(proto::FarmBatch {
            farm_time_ms: 1,
            wall_time_ns: 2,
            inputs: vec![proto::FarmInput { input: None }],
        }),
        C::Farm(proto::FarmBatch::default()),
        C::Report(proto::Report::default()),
        C::Takeover(proto::Takeover {
            term: 0,
            farm_time_ms: 5,
        }),
        C::Checkpoint(proto::Checkpoint {
            index: 1,
            digest: vec![1, 2, 3],
        }),
    ];
    for command in commands {
        assert_eq!(dec(&wrap(command)), UNKNOWN);
    }
}

/// Catches a command written or read under a format this build does not know (format 6;
/// formats 1 to 5 all carry the machine version, since formats 2 to 5 only add metadata
/// commands).
#[test]
fn commands_refuse_an_unknown_format() {
    let command = LogCommand::MachineVersion(MachineVersion::V1);
    let format = Format::new(6);
    assert_eq!(encode(format, &command), Err(EncodeError::Format(format)));
    assert_eq!(
        kbf_log::decode(format, &enc(&command)),
        Err(DecodeError::Format(format))
    );
}

/// Asserts that each machine version names its entry format, and that a version this
/// binary does not know has no format.
#[test]
fn machine_versions_name_their_entry_formats() {
    assert_eq!(MachineVersion::V1.get(), 1);
    assert_eq!(MachineVersion::V2.get(), 2);
    assert_eq!(MachineVersion::V1.format(), Some(Format::V1));
    assert_eq!(MachineVersion::V2.format(), Some(Format::V2));
    assert_eq!(MachineVersion::V3.get(), 3);
    assert_eq!(MachineVersion::V3.format(), Some(Format::V3));
    assert_eq!(MachineVersion::V4.get(), 4);
    assert_eq!(MachineVersion::V4.format(), Some(Format::V4));
    assert_eq!(MachineVersion::V5.get(), 5);
    assert_eq!(MachineVersion::V5.format(), Some(Format::V5));
    let v6 = MachineVersion::new(NonZeroU32::new(6).expect("non-zero"));
    assert_eq!(v6.format(), None);
    assert!(
        MachineVersion::V1 < MachineVersion::V2
            && MachineVersion::V2 < MachineVersion::V3
            && MachineVersion::V3 < MachineVersion::V4
            && MachineVersion::V4 < MachineVersion::V5
            && MachineVersion::V5 < v6
    );
}
