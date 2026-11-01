//! What the invocation record keeps of one lease's report (`docs/design/ui-data.md`
//! 4.2, phase U1): the nine REAPI times of the run, its `ResourceUsage` with the
//! cgroup block whole, the booking its lease was granted (every run) and, for a run
//! killed for memory at its own limit, the cap. [`of`] reads the report before it is judged (the judging drops a
//! killed run's result); [`killed`] adds the booking once the scheduler has taken it.

mod recording;
#[cfg(test)]
mod recording_tests;

pub(crate) use recording::Recording;

use std::time::SystemTime;

use kbf_invocation::{
    FootprintUsage, NumaBytes, ReapiTimes, Run, RunUsage, UsageBasis as RunBasis, WallTime,
};
use kbf_proto::reapi::ExecutedActionMetadata;
use kbf_proto::worker::{self, ResourceUsage, UsageBasis};
use kbf_types::{MemoryKill, MemoryRun};
use prost_types::Timestamp;

use crate::machine::Booked;
use crate::stamp::Stamp;

/// What `result`, for a run whose server times are `stamp` and taken in at
/// `received`, measured; `booked` is what its lease was granted (`None` for a lease the
/// server never started) and `usage` the `ResourceUsage` its result carried, as
/// [`crate::action_usage::Measured`] decoded it. The times are those its
/// `ActionResult` would carry once stamped ([`Stamp::times`]): the daemon's own,
/// `queued` from the server, and the worker start and completion the server saw when
/// the daemon left them out (a killed or lost run reports none of its own).
pub(crate) fn of(
    result: &worker::Result,
    usage: Option<&ResourceUsage>,
    stamp: Stamp,
    received: SystemTime,
    booked: Option<Booked>,
) -> Run {
    let none = ExecutedActionMetadata::default();
    let m = result
        .action_result
        .as_ref()
        .and_then(|r| r.execution_metadata.as_ref())
        .unwrap_or(&none);
    let stamped = stamp.times(m, received);
    Run {
        times: ReapiTimes {
            queued: wall(Some(stamped.queued)),
            worker_start: wall(Some(stamped.worker_start)),
            input_fetch_start: wall(m.input_fetch_start_timestamp),
            input_fetch_completed: wall(m.input_fetch_completed_timestamp),
            execution_start: wall(m.execution_start_timestamp),
            execution_completed: wall(m.execution_completed_timestamp),
            output_upload_start: wall(m.output_upload_start_timestamp),
            output_upload_completed: wall(m.output_upload_completed_timestamp),
            worker_completed: wall(Some(stamped.worker_completed)),
        },
        usage: usage.map(run_usage),
        booked_memory_bytes: booked.map(|b| b.memory_bytes),
        booked_millicpus: booked.map(|b| b.cpu_millis),
        cap_bytes: None,
        phase_times: if kbf_proto::task_claim::is_marked(m) {
            kbf_invocation::PhaseTimes::TaskClaim
        } else {
            kbf_invocation::PhaseTimes::Measured
        },
    }
}

/// `run`, with the booking of `killed`, the scheduler's record of it as a run killed
/// for memory, and its cap for a kill at its own limit.
pub(crate) fn killed(run: Run, killed: &MemoryRun) -> Run {
    Run {
        booked_memory_bytes: Some(killed.booked),
        cap_bytes: match killed.kill {
            MemoryKill::OwnLimit { cap } => Some(cap),
            MemoryKill::NodePressure => None,
        },
        ..run
    }
}

/// The run's usage, from the block its basis names and no other: a footprint basis
/// reads the footprint block and leaves every cgroup field `None`; a cgroup basis, or
/// none (a daemon that predates the field), reads the cgroup block when there is one,
/// and the basis is never inferred from which block is there. A block that is absent
/// stays absent; it is never filled from the other or made zero.
pub(crate) fn run_usage(u: &ResourceUsage) -> RunUsage {
    let carried = RunUsage {
        node_cpu_pressure_centi: u.node_cpu_pressure_centi,
        booked_millicpus: u.booked_millicpus,
        ..RunUsage::default()
    };
    if u.basis() == UsageBasis::MacFootprint {
        let f = u.footprint.unwrap_or_default();
        return RunUsage {
            wall_micros: f.wall_micros.unwrap_or(u.wall_micros),
            basis: RunBasis::MacFootprint,
            footprint: u.footprint.map(|f| FootprintUsage {
                peak_footprint_bytes: f.peak_footprint_bytes,
                cpu_user_micros: f.cpu_user_micros,
                cpu_system_micros: f.cpu_system_micros,
                wall_micros: f.wall_micros,
            }),
            ..carried
        };
    }
    let basis = if u.basis() == UsageBasis::Cgroup {
        RunBasis::Cgroup
    } else {
        RunBasis::Unspecified
    };
    // No block, no counters: nothing is made up for a runtime that has no cgroup (the
    // daemon's own `wait4` figures), and the basis is whatever the runtime said.
    let Some(c) = u.cgroup.as_ref() else {
        return RunUsage {
            wall_micros: u.wall_micros,
            basis,
            ..carried
        };
    };
    RunUsage {
        wall_micros: u.wall_micros,
        basis,
        cpu_usage_micros: c.cpu_usage_micros,
        cpu_user_micros: c.cpu_user_micros,
        cpu_system_micros: c.cpu_system_micros,
        cpu_periods: c.cpu_periods,
        cpu_throttled_periods: c.cpu_throttled_periods,
        cpu_throttled_micros: c.cpu_throttled_micros,
        memory_peak_bytes: c.memory_peak_bytes,
        memory_events_max: c.memory_events_max,
        memory_events_oom_kill: c.memory_events_oom_kill,
        memory_swap_peak_bytes: c.memory_swap_peak_bytes,
        memory_anon_swap_peak_bytes: c.memory_anon_swap_peak_bytes,
        cpu_pressure_some_micros: c.cpu_pressure_some_micros,
        memory_pressure_some_micros: c.memory_pressure_some_micros,
        memory_pressure_full_micros: c.memory_pressure_full_micros,
        cpus_effective: c.cpus_effective,
        peak_window_millicores: c.peak_window_millicores,
        numa: c
            .numa
            .iter()
            .map(|n| NumaBytes {
                node: n.node,
                bytes: n.bytes,
            })
            .collect(),
        ..carried
    }
}

/// A protobuf timestamp on the wall clock, to the microsecond; `None` for none, or
/// one before the epoch or past `u64` microseconds.
fn wall(t: Option<Timestamp>) -> Option<WallTime> {
    let t = t?;
    let micros = i128::from(t.seconds) * 1_000_000 + i128::from(t.nanos) / 1_000;
    u64::try_from(micros).ok().map(WallTime::from_unix_micros)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use kbf_proto::reapi::ActionResult;
    use kbf_proto::worker::{CgroupUsage, NumaNodeBytes};
    use kbf_types::WorkerId;

    use super::*;

    fn secs(n: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(n)
    }

    fn at(n: u64) -> Option<WallTime> {
        Some(WallTime::from_unix_micros(n * 1_000_000))
    }

    /// Catches: a REAPI time dropped or copied into the wrong field (each of the
    /// daemon's eight has its own second), `queued` not the server's, a cgroup
    /// counter dropped or swapped, or the NUMA list lost.
    #[test]
    fn every_time_and_every_cgroup_counter_is_kept() {
        let ts = |n| Some(Timestamp::from(secs(n)));
        let usage = ResourceUsage {
            wall_micros: 2_000_000,
            cgroup: Some(CgroupUsage {
                cpu_usage_micros: Some(11),
                cpu_user_micros: Some(12),
                cpu_system_micros: Some(13),
                cpu_periods: Some(14),
                cpu_throttled_periods: Some(15),
                cpu_throttled_micros: Some(16),
                memory_peak_bytes: Some(17),
                numa: vec![NumaNodeBytes { node: 1, bytes: 18 }],
                ..CgroupUsage::default()
            }),
            ..ResourceUsage::default()
        };
        let result = worker::Result {
            action_result: Some(ActionResult {
                execution_metadata: Some(ExecutedActionMetadata {
                    queued_timestamp: ts(99),
                    worker_start_timestamp: ts(2),
                    input_fetch_start_timestamp: ts(3),
                    input_fetch_completed_timestamp: ts(4),
                    execution_start_timestamp: ts(5),
                    execution_completed_timestamp: ts(6),
                    output_upload_start_timestamp: ts(7),
                    output_upload_completed_timestamp: ts(8),
                    worker_completed_timestamp: ts(9),
                    auxiliary_metadata: vec![usage.to_any()],
                    ..ExecutedActionMetadata::default()
                }),
                ..ActionResult::default()
            }),
            ..worker::Result::default()
        };
        let stamp = Stamp {
            queued: secs(1),
            started: secs(50),
        };
        let decoded = result
            .action_result
            .as_ref()
            .and_then(ResourceUsage::in_result);
        let run = of(&result, decoded.as_ref(), stamp, secs(60), None);
        let times = run.times.named().map(|(_, t)| t);
        assert_eq!(
            times,
            [
                at(1),
                at(2),
                at(3),
                at(4),
                at(5),
                at(6),
                at(7),
                at(8),
                at(9)
            ]
        );
        let u = run.usage.expect("usage");
        assert_eq!(u.wall_micros, 2_000_000);
        assert_eq!(
            [
                u.cpu_usage_micros,
                u.cpu_user_micros,
                u.cpu_system_micros,
                u.cpu_periods,
                u.cpu_throttled_periods,
                u.cpu_throttled_micros,
                u.memory_peak_bytes
            ],
            [11, 12, 13, 14, 15, 16, 17].map(Some)
        );
        assert_eq!(u.numa, [NumaBytes { node: 1, bytes: 18 }]);
    }

    /// The metadata an executor run's result carries: the three phases the task claimed, as the
    /// frontend lays them (`fetch` 2 s, `run` 5 s, `pack` 1 s, the pack read at second 28).
    fn claimed_by_task() -> worker::Result {
        let ts = |n| Some(Timestamp::from(secs(n)));
        worker::Result {
            action_result: Some(ActionResult {
                execution_metadata: Some(ExecutedActionMetadata {
                    input_fetch_start_timestamp: ts(20),
                    input_fetch_completed_timestamp: ts(22),
                    execution_start_timestamp: ts(22),
                    execution_completed_timestamp: ts(27),
                    output_upload_start_timestamp: ts(27),
                    output_upload_completed_timestamp: ts(28),
                    ..ExecutedActionMetadata::default()
                }),
                ..ActionResult::default()
            }),
            ..worker::Result::default()
        }
    }

    /// Catches an executor run's record missing a time: the task's three phases reach the record
    /// unchanged beside the server's three (queued, Start sent, result taken in), so the record
    /// has all nine, as a daemon's run does, and the two ends bracket the task's phases.
    #[test]
    fn the_invocation_record_of_an_executor_run_has_all_nine_times() {
        let stamp = Stamp {
            queued: secs(10),
            started: secs(15),
        };
        let run = of(&claimed_by_task(), None, stamp, secs(30), None);
        let times = run.times.named().map(|(_, t)| t);
        assert_eq!(
            times,
            [
                at(10),
                at(15),
                at(20),
                at(22),
                at(22),
                at(27),
                at(27),
                at(28),
                at(30)
            ]
        );
    }

    /// Catches a time made up for a run whose task claimed none: with no metadata the four
    /// phase pairs of the record are absent, never zero, and the server's three remain.
    #[test]
    fn an_executor_run_without_task_timing_has_no_phase_times() {
        let stamp = Stamp {
            queued: secs(10),
            started: secs(15),
        };
        let bare = worker::Result {
            action_result: Some(ActionResult::default()),
            ..worker::Result::default()
        };
        let run = of(&bare, None, stamp, secs(30), None);
        assert_eq!(
            run.times,
            ReapiTimes {
                queued: at(10),
                worker_start: at(15),
                worker_completed: at(30),
                ..ReapiTimes::default()
            }
        );
    }

    /// Catches the provenance read off the wrong thing: metadata the frontend marked as a
    /// task's claim is a `TaskClaim` run, a daemon's metadata (its usage entry, no mark) and a
    /// report with none are `Measured`.
    #[test]
    fn an_executor_run_is_task_claim_and_a_daemon_run_is_measured() {
        let stamp = Stamp {
            queued: secs(10),
            started: secs(15),
        };
        let mut claimed = claimed_by_task();
        let metadata = claimed
            .action_result
            .as_mut()
            .and_then(|r| r.execution_metadata.as_mut())
            .expect("metadata");
        kbf_proto::task_claim::mark(metadata);
        let run = of(&claimed, None, stamp, secs(30), None);
        assert_eq!(run.phase_times, kbf_invocation::PhaseTimes::TaskClaim);

        let daemon = worker::Result {
            action_result: Some(ActionResult {
                execution_metadata: Some(ExecutedActionMetadata {
                    execution_start_timestamp: Some(Timestamp::from(secs(22))),
                    auxiliary_metadata: vec![ResourceUsage::default().to_any()],
                    ..ExecutedActionMetadata::default()
                }),
                ..ActionResult::default()
            }),
            ..worker::Result::default()
        };
        let run = of(&daemon, None, stamp, secs(30), None);
        assert_eq!(run.phase_times, kbf_invocation::PhaseTimes::Measured);
        let bare = of(&worker::Result::default(), None, stamp, secs(30), None);
        assert_eq!(bare.phase_times, kbf_invocation::PhaseTimes::Measured);
    }

    /// Catches: a report without a result (a killed or lost run) given no times, or
    /// times other than the server's (queued, Start sent, taken in); and usage made
    /// up for a result that carries none.
    #[test]
    fn a_report_without_a_result_has_the_server_s_times_only() {
        let run = of(
            &worker::Result::default(),
            None,
            Stamp {
                queued: secs(1),
                started: secs(2),
            },
            secs(3),
            None,
        );
        assert_eq!(
            run.times,
            ReapiTimes {
                queued: at(1),
                worker_start: at(2),
                worker_completed: at(3),
                ..ReapiTimes::default()
            }
        );
        assert_eq!(run.usage, None);
    }

    /// Catches: the grant dropped from a run record (an accepted run without what its
    /// lease held), the two axes swapped, a lease the server never started given a made-up
    /// booking, and a memory kill's booking changing the CPU one.
    #[test]
    fn a_run_keeps_what_its_lease_was_granted() {
        let stamp = Stamp {
            queued: secs(1),
            started: secs(2),
        };
        let booked = Booked {
            cpu_millis: 2_500,
            memory_bytes: 3 << 30,
        };
        let granted = of(
            &worker::Result::default(),
            None,
            stamp,
            secs(3),
            Some(booked),
        );
        assert_eq!(
            (granted.booked_millicpus, granted.booked_memory_bytes),
            (Some(2_500), Some(3 << 30))
        );
        let unknown = of(&worker::Result::default(), None, stamp, secs(3), None);
        assert_eq!(
            (unknown.booked_millicpus, unknown.booked_memory_bytes),
            (None, None)
        );
        let kill = MemoryRun {
            worker: WorkerId::new("n"),
            booked: 4 << 30,
            kill: MemoryKill::NodePressure,
        };
        let killed = super::killed(granted, &kill);
        assert_eq!(
            (killed.booked_millicpus, killed.booked_memory_bytes),
            (Some(2_500), Some(4 << 30))
        );
    }

    fn cgroup_block() -> CgroupUsage {
        CgroupUsage {
            cpu_usage_micros: Some(11),
            cpu_user_micros: Some(12),
            cpu_system_micros: Some(13),
            memory_peak_bytes: Some(17),
            ..CgroupUsage::default()
        }
    }

    fn footprint_block() -> worker::FootprintUsage {
        worker::FootprintUsage {
            peak_footprint_bytes: Some(21),
            cpu_user_micros: Some(22),
            cpu_system_micros: Some(23),
            wall_micros: Some(24),
        }
    }

    /// Catches: a footprint run read from the cgroup block (a version that always
    /// reads cgroup), the footprint peak filed as `memory_peak_bytes` (a footprint and a
    /// cgroup peak pooled), and a cgroup block that rides beside a footprint basis
    /// leaking its counters.
    #[test]
    fn a_footprint_basis_reads_the_footprint_block_and_never_the_cgroup_one() {
        let usage = ResourceUsage {
            wall_micros: 99,
            basis: UsageBasis::MacFootprint.into(),
            footprint: Some(footprint_block()),
            cgroup: Some(cgroup_block()),
            ..ResourceUsage::default()
        };
        let u = run_usage(&usage);
        assert_eq!(u.basis, RunBasis::MacFootprint);
        assert_eq!(
            u.footprint,
            Some(FootprintUsage {
                peak_footprint_bytes: Some(21),
                cpu_user_micros: Some(22),
                cpu_system_micros: Some(23),
                wall_micros: Some(24),
            })
        );
        assert_eq!(u.wall_micros, 24, "the footprint block's own wall time");
        assert_eq!(
            [
                u.cpu_usage_micros,
                u.cpu_user_micros,
                u.cpu_system_micros,
                u.cpu_periods,
                u.memory_peak_bytes
            ],
            [None; 5],
            "no cgroup field under a footprint basis"
        );
    }

    /// Catches: a cgroup run (or an older daemon's, with no basis) reading the
    /// footprint block, which would file a footprint where a cgroup peak belongs; a
    /// basis lost; and an absent footprint made into a zero or a block of zeros.
    #[test]
    fn a_cgroup_basis_or_none_reads_the_cgroup_block_and_an_absent_block_stays_absent() {
        let mixed = |basis: UsageBasis| ResourceUsage {
            basis: basis.into(),
            footprint: Some(footprint_block()),
            cgroup: Some(cgroup_block()),
            wall_micros: 5,
            ..ResourceUsage::default()
        };
        for (basis, want) in [
            (UsageBasis::Cgroup, RunBasis::Cgroup),
            (UsageBasis::Unspecified, RunBasis::Unspecified),
        ] {
            let u = run_usage(&mixed(basis));
            assert_eq!(u.basis, want);
            assert_eq!(u.footprint, None, "{basis:?}");
            assert_eq!(u.memory_peak_bytes, Some(17), "{basis:?}");
            assert_eq!(u.wall_micros, 5);
        }
        // A footprint basis with no footprint block: absent, not zero.
        let none = run_usage(&ResourceUsage {
            basis: UsageBasis::MacFootprint.into(),
            wall_micros: 6,
            ..ResourceUsage::default()
        });
        assert_eq!(none.basis, RunBasis::MacFootprint);
        assert_eq!(none.footprint, None);
        assert_eq!(none.wall_micros, 6, "the top-level wall time stands in");
        assert_eq!(none.memory_peak_bytes, None);
        // A footprint field the driver could not read stays `None`, beside one it did.
        let part = run_usage(&ResourceUsage {
            basis: UsageBasis::MacFootprint.into(),
            footprint: Some(worker::FootprintUsage {
                peak_footprint_bytes: Some(0),
                ..worker::FootprintUsage::default()
            }),
            ..ResourceUsage::default()
        });
        let f = part.footprint.expect("block");
        assert_eq!(
            (
                f.peak_footprint_bytes,
                f.cpu_user_micros,
                f.cpu_system_micros
            ),
            (Some(0), None, None)
        );
    }

    /// Catches: the node's CPU pressure or the booked CPU dropped on the way to the
    /// record, on either basis, or read as zero when the daemon did not report them.
    #[test]
    fn the_cpu_pressure_and_booking_ride_on_either_basis_and_stay_absent_when_unreported() {
        for basis in [UsageBasis::Cgroup, UsageBasis::MacFootprint] {
            let u = run_usage(&ResourceUsage {
                basis: basis.into(),
                node_cpu_pressure_centi: Some(1234),
                booked_millicpus: Some(1500),
                ..ResourceUsage::default()
            });
            assert_eq!(
                (u.node_cpu_pressure_centi, u.booked_millicpus),
                (Some(1234), Some(1500)),
                "{basis:?}"
            );
        }
        let u = run_usage(&ResourceUsage::default());
        assert_eq!(
            (u.node_cpu_pressure_centi, u.booked_millicpus),
            (None, None)
        );
    }

    /// Catches: the lease counters (events, swap peak, anon + swap peak, pressure
    /// totals, CPUs) dropped on the way to the record, and a runtime with no cgroup
    /// block (the daemon's own `wait4` figures) given a basis, a peak or counters it
    /// did not measure: the basis is what the runtime said, never inferred.
    #[test]
    fn the_lease_counters_are_kept_and_no_block_means_no_sample_and_no_inferred_basis() {
        let u = run_usage(&ResourceUsage {
            basis: UsageBasis::Cgroup.into(),
            cgroup: Some(CgroupUsage {
                memory_events_max: Some(1),
                memory_events_oom_kill: Some(2),
                memory_swap_peak_bytes: Some(3),
                memory_anon_swap_peak_bytes: Some(4),
                cpu_pressure_some_micros: Some(5),
                memory_pressure_some_micros: Some(6),
                memory_pressure_full_micros: Some(7),
                cpus_effective: Some(8),
                ..CgroupUsage::default()
            }),
            ..ResourceUsage::default()
        });
        assert_eq!(
            [
                u.memory_events_max,
                u.memory_events_oom_kill,
                u.memory_swap_peak_bytes,
                u.memory_anon_swap_peak_bytes,
                u.cpu_pressure_some_micros,
                u.memory_pressure_some_micros,
                u.memory_pressure_full_micros,
            ],
            [1, 2, 3, 4, 5, 6, 7].map(Some)
        );
        assert_eq!(u.cpus_effective, Some(8));
        // No block, an unspecified basis, and the figures `wait4` gave.
        let wait4 = run_usage(&ResourceUsage {
            cpu_user_micros: 1,
            peak_memory_bytes: 99,
            wall_micros: 5,
            ..ResourceUsage::default()
        });
        assert_eq!(wait4.basis, RunBasis::Unspecified);
        assert_eq!(wait4.memory_peak_bytes, None, "ru_maxrss is no cgroup peak");
        assert_eq!(wait4.cpu_usage_micros, None);
        assert_eq!(wait4.wall_micros, 5);
        // A cgroup block does not make the basis cgroup.
        let unsaid = run_usage(&ResourceUsage {
            cgroup: Some(cgroup_block()),
            ..ResourceUsage::default()
        });
        assert_eq!(unsaid.basis, RunBasis::Unspecified);
    }

    /// Catches: a cap set for a busy node's kill, or not set for an own-limit kill,
    /// and the booking dropped.
    #[test]
    fn a_kill_keeps_its_booking_and_only_an_own_limit_kill_its_cap() {
        let run = |kill| {
            killed(
                Run::default(),
                &MemoryRun {
                    worker: WorkerId::new("n"),
                    booked: 8,
                    kill,
                },
            )
        };
        let own = run(MemoryKill::OwnLimit { cap: 64 });
        assert_eq!(
            (own.booked_memory_bytes, own.cap_bytes),
            (Some(8), Some(64))
        );
        let busy = run(MemoryKill::NodePressure);
        assert_eq!((busy.booked_memory_bytes, busy.cap_bytes), (Some(8), None));
    }

    /// Catches: a time before the epoch read as a huge or wrapped one.
    #[test]
    fn a_time_before_the_epoch_is_none() {
        let before = Timestamp {
            seconds: -1,
            nanos: 0,
        };
        assert_eq!(wall(Some(before)), None);
        assert_eq!(wall(None), None);
        let micro = Timestamp {
            seconds: 2,
            nanos: 3_999,
        };
        assert_eq!(
            wall(Some(micro)),
            Some(WallTime::from_unix_micros(2_000_003))
        );
    }
}
