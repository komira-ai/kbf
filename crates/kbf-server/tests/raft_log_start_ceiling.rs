//! The start ceiling of `RaftMetaLog` ([`RaftMetaLog::raise_up_to`] and
//! [`RaftMetaLog::raise_at_start`]): `raise_at_start` raises the log's machine version to
//! a target but never past the ceiling, and never lowers it. No start of the server calls
//! either. Each test names the defect it catches.

use std::num::NonZeroU64;

use kbf_log::{FarmLogId, MachineVersion};
use kbf_meta::Retention;
use kbf_server::RaftMetaLog;
use kbf_server::raft_log::{RUNS, START_CEILING};
use kbf_store::FaultFs;

const V1: MachineVersion = MachineVersion::V1;
const V2: MachineVersion = MachineVersion::V2;
const V3: MachineVersion = MachineVersion::V3;
const V4: MachineVersion = MachineVersion::V4;
const V5: MachineVersion = MachineVersion::V5;

async fn open(fs: &FaultFs) -> RaftMetaLog<FaultFs> {
    let id = FarmLogId::new(NonZeroU64::new(9).expect("non-zero"));
    RaftMetaLog::open(fs.clone(), Retention::default(), "kbf/", move || Ok(id))
        .await
        .expect("open the log")
}

/// Catches: a log whose ceiling is not the release's (mutant: the field starts at
/// version 1, so no start raises anything), and a `with_start_ceiling` that changes
/// nothing.
#[tokio::test]
async fn a_log_carries_the_releases_start_ceiling_until_a_test_lowers_it() {
    let log = open(&FaultFs::new()).await;
    assert_eq!(log.start_ceiling(), START_CEILING);
    let lowered = log.with_start_ceiling(V3);
    assert_eq!(lowered.start_ceiling(), V3);
}

/// Catches: a raise that goes past the ceiling (mutant: `raise_up_to` raises to the
/// target), one that takes the higher of the two (mutant: `max` for `min`), one that
/// lowers a log already past the ceiling or commits again at it (the applied index must
/// not move), and a ceiling that holds back a target below it (mutant: the ceiling
/// committed in place of the target). The clock is paused, so no tick lands between the
/// reads of the applied index.
#[tokio::test(start_paused = true)]
async fn a_raise_stops_at_the_ceiling_and_never_lowers() {
    let log = open(&FaultFs::new()).await;
    log.raise_up_to(V5, V3).expect("a raise");
    assert_eq!(log.machine_version(), V3);
    let applied = log.applied_index();
    for (target, ceiling) in [(V5, V3), (V3, V5), (V2, V5), (V5, V1)] {
        log.raise_up_to(target, ceiling).expect("a no-op");
    }
    assert_eq!(log.machine_version(), V3);
    assert_eq!(log.applied_index(), applied, "a raise committed again");
    log.raise_up_to(V4, V5).expect("a raise");
    assert_eq!(log.machine_version(), V4, "held back by the ceiling");
    log.raise_up_to(V5, V5).expect("a raise");
    assert_eq!(log.machine_version(), V5);
}

/// Catches: a start that ignores the log's own ceiling (mutant: `raise_at_start` raises
/// to the target), a ceiling at the version the binary runs that holds a raise back
/// (mutant: the ceiling forces a lower version), and a ceiling that is not kept across a
/// restart as the log's version is: the restart reads version 4 from the log, a start
/// with the same ceiling commits nothing past it, and one with a higher ceiling goes on.
#[tokio::test]
async fn a_start_raises_to_its_ceiling_and_a_later_start_with_a_higher_one_goes_on() {
    let fs = FaultFs::new();
    let log = open(&fs).await.with_start_ceiling(V4);
    log.raise_at_start(V5).expect("a raise");
    assert_eq!(log.machine_version(), V4);
    drop(log);
    let log = open(&fs.restart()).await.with_start_ceiling(V4);
    assert_eq!(log.machine_version(), V4, "the version is the log's");
    log.raise_at_start(V5).expect("a no-op");
    assert_eq!(log.machine_version(), V4);
    drop(log);
    let log = open(&fs.restart()).await.with_start_ceiling(RUNS);
    log.raise_at_start(V5).expect("a raise");
    assert_eq!(log.machine_version(), V5);
}
