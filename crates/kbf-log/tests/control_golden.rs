//! The format-1 bytes of the machine-version command, and of the farm commands that earlier
//! builds wrote and this one skips.
//!
//! Catches an encoding that changes under a committed log: a field tag swapped or
//! renumbered, or a command moved to another `oneof` number. The bytes below were written
//! out by hand from `log.proto`; a log an older binary wrote must decode to the same value
//! on a newer one. The files of `tests/golden/` hold the farm commands in full
//! (`golden_corpus.rs`).

mod control_support;

use std::num::NonZeroU32;

use control_support::{dec, enc};
use kbf_log::{LogCommand, MachineVersion};

/// Catches a machine-version command that is written as other bytes than an earlier build
/// wrote, or read as another value.
#[test]
fn a_machine_version_has_its_golden_bytes() {
    // MachineVersion (3a): version 2
    let command = LogCommand::MachineVersion(MachineVersion::new(NonZeroU32::new(2).expect("2")));
    let bytes = hex::decode("3a020802").expect("hex");
    assert_eq!(enc(&command), bytes, "encoding changed");
    assert_eq!(dec(&bytes), Ok(command), "golden bytes decode differently");
}

/// Catches a farm command, one per arm of the `oneof` an earlier build wrote (farm tick,
/// report, takeover, checkpoint), that is read as a value or refused instead of skipped.
#[test]
fn each_farm_command_arm_decodes_as_unknown() {
    let cases = [
        // farm (1a): farm time 1, wall time 2, one input: tick (3a), empty
        ("Farm", "1a08080110021a023a00"),
        // report (22): empty
        ("Report", "2200"),
        // takeover (2a): term 5, farm time 7
        ("Takeover", "2a0408051007"),
        // checkpoint (32): index 5
        ("Checkpoint", "32020805"),
    ];
    for (name, hex) in cases {
        let bytes = hex::decode(hex).expect("hex");
        assert_eq!(dec(&bytes), Ok(LogCommand::Unknown), "{name}");
    }
}
