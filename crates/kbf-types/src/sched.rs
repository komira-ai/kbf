//! The generic scheduling vocabulary: QoS levels, lease identifiers and kinds, node
//! phases, farm time, the vocabulary of scheduled work (operations, workers, request
//! vectors, control-log records), the `StateMachine` trait and `Effect` enum, and the
//! version a binary reports and the cap two versions negotiate.
//!
//! Nothing here is specific to building: no REAPI, no content `Digest`. The
//! build-specific types (digest, image, release, enrollment, host role, object store and
//! operator APIs) stay in the crate root. Every item is also re-exported at the crate
//! root, so `kbf_types::LeaseId` and `kbf_types::sched::LeaseId` are the same type.
//!
//! One item still names a build type: `work` carries `ActionKey` (and with it a
//! `Digest`) in `StartLease` and `Outcome`. The test at the foot of this file lists
//! those lines and refuses any other.

mod lease;
mod phase;
mod qos;
mod state;
mod time;
mod version;
mod work;

pub use lease::{LeaseId, LeaseKind};
pub use phase::NodePhase;
pub use qos::{CustomQos, Qos, QosError};
pub use state::{Effect, StateMachine};
pub use time::FarmTime;
pub use version::{Supported, TooNew, VERSION_CAP, Version, VersionFault, understood};
pub use work::{
    ActionKey, Answer, ControlRecord, Failure, FarmFault, FarmRun, FencePolicy, LeaseGrant,
    LostWhy, MemoryKill, MemoryRun, OperationId, Outcome, Refusal, RefusalRecord, Resources,
    ResultRecord, StartLease, WaiterId, Waiting, WorkerId,
};

#[cfg(test)]
mod tests {
    /// The names this module must not use.
    const BUILD_NAMES: [&str; 3] = ["Digest", "ActionKey", "REAPI"];

    /// Code lines of `work.rs` that still name a build type. A later change removes
    /// `ActionKey` from the scheduling types and empties this list.
    const ALLOWED: [&str; 5] = [
        "use crate::{Digest, LeaseId, LeaseKind};",
        "pub struct ActionKey {",
        "pub action: Digest,",
        "action_result: Digest,",
        "pub key: ActionKey,",
    ];

    /// The non-comment lines before the unit tests of one source file.
    fn code_lines(source: &str) -> Vec<&str> {
        source
            .lines()
            .take_while(|line| *line != "#[cfg(test)]")
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .collect()
    }

    fn offenders(source: &str) -> Vec<&str> {
        code_lines(source)
            .into_iter()
            .filter(|line| BUILD_NAMES.iter().any(|name| line.contains(name)))
            .collect()
    }

    #[test]
    fn the_generic_files_name_no_build_type() {
        let clean = [
            ("lease.rs", include_str!("sched/lease.rs")),
            ("phase.rs", include_str!("sched/phase.rs")),
            ("qos.rs", include_str!("sched/qos.rs")),
            ("state.rs", include_str!("sched/state.rs")),
            ("time.rs", include_str!("sched/time.rs")),
            ("version.rs", include_str!("sched/version.rs")),
        ];
        for (file, source) in clean {
            assert_eq!(offenders(source), Vec::<&str>::new(), "{file}");
        }
    }

    #[test]
    fn work_names_only_the_listed_build_lines() {
        let found = offenders(include_str!("sched/work.rs"));
        assert_eq!(found, ALLOWED);
    }

    #[test]
    fn the_scan_sees_a_build_name_in_code_and_skips_comments() {
        let source = "// the REAPI name\nlet d: Digest;\n#[cfg(test)]\nlet e: Digest;\n";
        assert_eq!(offenders(source), vec!["let d: Digest;"]);
    }

    #[test]
    fn the_root_paths_are_the_sched_types() {
        let id: crate::sched::LeaseId = crate::LeaseId::new(1, 7);
        assert_eq!(id, crate::sched::LeaseId::new(1, 7));
    }
}
