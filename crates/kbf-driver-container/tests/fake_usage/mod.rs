//! What the driver reports of the lease cgroup's counters (`cpu.stat`, `memory.peak`,
//! `memory.numa_stat`), against the stand-in `podman` and a plain directory standing in
//! for the cgroup mount: the action's act steps write the files as the kernel would,
//! and the result's `ResourceUsage` and the daemon's `lease phases:` line are read back.
//!
//! A module of `fake_podman.rs`, not a test binary of its own, like `fake_memory`
//! (the coverage job counts the runtime's generic lines per build of a binary).

use std::time::Duration;

use kbf_daemon::phases::LeasePhases;
use kbf_proto::worker::{NumaNodeBytes, ResourceUsage};
use kbf_types::LeaseId;

use crate::support::Spec;
use crate::support::fake::{Fake, image};

/// The act steps that write `lines` to the lease cgroup's `file`.
fn write(file: &str, lines: &[&str]) -> Vec<String> {
    let mut steps = vec!["write-lines".to_owned(), format!("$CG/{file}")];
    steps.extend(lines.iter().map(|l| (*l).to_owned()));
    steps
}

/// Joins act steps, `;` between them.
fn steps(parts: &[Vec<String>]) -> Vec<String> {
    let mut all = Vec::new();
    for part in parts {
        if !all.is_empty() {
            all.push(";".to_owned());
        }
        all.extend(part.iter().cloned());
    }
    all
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| (*s).to_owned()).collect()
}

fn node(node: u32, bytes: u64) -> NumaNodeBytes {
    NumaNodeBytes { node, bytes }
}

/// Catches the cgroup's counters not read at the end of a lease (the result carries no
/// usage), read from the wrong file, lost on the way to the `lease phases:` line, or
/// the four numbers every runtime reports (user and system CPU, peak memory, wall)
/// left at zero for a runtime that has the cgroup's: they come from the same files.
#[tokio::test]
async fn a_lease_reports_its_cgroup_counters_in_the_result_and_the_phases_line() {
    let fake = Fake::new("usage");
    let spec = Spec::new(&image(), "unused");
    let action = steps(&[
        write(
            "cpu.stat",
            &[
                "usage_usec 3000000",
                "user_usec 2400000",
                "system_usec 600000",
                "nr_periods 10",
                "nr_throttled 2",
                "throttled_usec 4500",
            ],
        ),
        write("memory.peak", &["1073741824"]),
        // Two nodes: anon + file is 800 + 100 on node 0 and 200 + 0 on node 1.
        write(
            "memory.numa_stat",
            &[
                "anon N0=800 N1=200",
                "file N0=100 N1=0",
                "kernel_stack N0=999 N1=999",
            ],
        ),
        words(&["exit", "0"]),
    ]);
    let result = fake.run(1, &spec, &action).await.expect("ran");
    fake.assert_clean(1);

    let usage = ResourceUsage::in_result(&result).expect("the result carries usage");
    assert_eq!(
        (
            usage.cpu_user_micros,
            usage.cpu_system_micros,
            usage.peak_memory_bytes
        ),
        (2_400_000, 600_000, 1 << 30)
    );
    assert!(usage.wall_micros > 0, "{usage:?}");
    let cgroup = usage.cgroup.as_ref().expect("the cgroup's counters");
    assert_eq!(
        (
            cgroup.cpu_usage_micros,
            cgroup.cpu_periods,
            cgroup.cpu_throttled_periods,
            cgroup.cpu_throttled_micros,
            cgroup.memory_peak_bytes,
        ),
        (
            Some(3_000_000),
            Some(10),
            Some(2),
            Some(4500),
            Some(1 << 30)
        )
    );
    assert_eq!(cgroup.numa, vec![node(0, 900), node(1, 200)]);

    let phases = LeasePhases::new(
        LeaseId::new(1, 1),
        "container",
        Duration::ZERO,
        Some(&result),
    );
    let line = phases.to_string();
    assert!(
        line.contains(
            " cpu_usage_us=3000000 cpu_user_us=2400000 cpu_system_us=600000 cpu_periods=10 \
             cpu_throttled=2 throttled_us=4500 mem_peak=1073741824 parallelism="
        ),
        "{line}"
    );
    assert!(
        line.ends_with(" numa_bytes=0:900,1:200 numa_spread=0.18"),
        "{line}"
    );
    // 3 s of CPU over the run's wall time, to the hundredth.
    let wall = u128::from(usage.wall_micros);
    let expect = (3_000_000u128 * 100 + wall / 2) / wall;
    assert_eq!(
        phases.parallelism_centi,
        Some(u64::try_from(expect).expect("fits"))
    );
    assert_eq!(phases.numa_spread_centi, Some(18));
}

/// Catches a cgroup without the files (cgroup v1, a kernel without `memory.peak` or
/// NUMA) failing the lease or reporting zeros: the lease must still complete, and the
/// line must say `-` for every counter, not 0 (a throttled count of 0 means something).
#[tokio::test]
async fn a_cgroup_without_the_files_still_completes_with_every_counter_absent() {
    let fake = Fake::new("usage-absent");
    let spec = Spec::new(&image(), "unused");
    let result = fake.run(1, &spec, &["exit", "3"]).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    fake.assert_clean(1);
    let usage = ResourceUsage::in_result(&result).expect("usage with the wall time");
    assert_eq!((usage.cpu_user_micros, usage.peak_memory_bytes), (0, 0));
    assert!(usage.wall_micros > 0);
    let phases = LeasePhases::new(
        LeaseId::new(1, 1),
        "container",
        Duration::ZERO,
        Some(&result),
    );
    assert!(
        phases.to_string().ends_with(
            " cpu_usage_us=- cpu_user_us=- cpu_system_us=- cpu_periods=- cpu_throttled=- \
             throttled_us=- mem_peak=- parallelism=- numa_bytes=- numa_spread=-"
        ),
        "{phases}"
    );
}

/// Catches the NUMA split read only at the end of the lease: the action here holds
/// memory on both nodes for longer than the sampler's period, then frees it before
/// exiting (as a process exit does), so only a sample taken while it ran sees it.
#[tokio::test]
async fn the_numa_split_is_the_one_seen_while_the_action_ran() {
    let fake = Fake::new("usage-numa");
    let spec = Spec::new(&image(), "unused");
    let action = steps(&[
        write(
            "memory.numa_stat",
            &["anon N0=7000 N1=3000", "file N0=0 N1=0"],
        ),
        words(&["sleep", "1.6"]),
        write("memory.numa_stat", &["anon N0=0 N1=0", "file N0=10 N1=0"]),
        words(&["exit", "0"]),
    ]);
    let result = fake.run(1, &spec, &action).await.expect("ran");
    let usage = ResourceUsage::in_result(&result).expect("usage");
    let cgroup = usage.cgroup.expect("the cgroup's counters");
    assert_eq!(cgroup.numa, vec![node(0, 7000), node(1, 3000)]);
    assert_eq!(cgroup.numa_spread_centi(), Some(30));
}
