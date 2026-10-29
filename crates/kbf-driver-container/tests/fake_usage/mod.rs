//! What the driver reports of the lease cgroup's counters (`cpu.stat`, `memory.peak`,
//! `memory.numa_stat`), against the stand-in `crun` and a plain directory standing in
//! for the cgroup mount: the action's act steps write the files as the kernel would,
//! and the result's `ResourceUsage` and the daemon's `lease phases:` line are read back.
//!
//! A module of `fake_crun.rs`, not a test binary of its own, like `fake_memory`
//! (the coverage job counts the runtime's generic lines per build of a binary).

use std::time::Duration;

use kbf_daemon::phases::LeasePhases;
use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::{ActionsCpu, ActionsCpuMax};
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
/// memory on both nodes, frees it before exiting (as a process exit does), and does not
/// do so until the test has seen the sampler read what it held, so only a sample taken
/// while it ran sees it. Nothing here waits on a clock: the action is held by a file the
/// test makes once the runtime's count of samples that held memory is above zero.
#[tokio::test]
async fn the_numa_split_is_the_one_seen_while_the_action_ran() {
    let fake = Fake::with("usage-numa", |config| {
        config.numa_every = Duration::from_millis(5);
    });
    let spec = Spec::new(&image(), "unused");
    let action = steps(&[
        write(
            "memory.numa_stat",
            &["anon N0=7000 N1=3000", "file N0=0 N1=0"],
        ),
        // The bound is for a test that fails: the file is made as soon as it is read.
        words(&["wait-for", "$CG/sampled", "600"]),
        write("memory.numa_stat", &["anon N0=0 N1=0", "file N0=10 N1=0"]),
        words(&["exit", "0"]),
    ]);
    let sampled = async {
        while fake.runtime.numa_samples() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        std::fs::write(fake.lease_cgroup(1).join("sampled"), "").expect("release the action");
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(120), async {
        tokio::join!(fake.run(1, &spec, &action), sampled)
    })
    .await
    .expect("the sampler never read the memory the action held");
    let usage = ResourceUsage::in_result(&result.expect("ran")).expect("usage");
    let cgroup = usage.cgroup.expect("the cgroup's counters");
    assert_eq!(cgroup.numa, vec![node(0, 7000), node(1, 3000)]);
    assert_eq!(cgroup.numa_spread_centi(), Some(30));
}

/// The counters an action writes into its cgroup, then the file that says they are all
/// there (the files are written one after another).
fn counters() -> Vec<Vec<String>> {
    vec![
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
        write("memory.peak", &["2000000000"]),
        write(
            "memory.numa_stat",
            &["anon N0=800 N1=200", "file N0=0 N1=0"],
        ),
    ]
}

/// Catches an out-of-memory kill that returns early with nothing counted (the peak of
/// the killed action is what the memory ladder needs): on the code before this change
/// `failed_usage` has nothing, so the first assertion below fails. The usage is the
/// cgroup's as the kill left it, the daemon's line prints it and ends with `oom=1`, and
/// it is handed over once.
#[tokio::test]
async fn an_oom_killed_lease_reports_its_peak_memory_and_cpu() {
    let fake = Fake::new("usage-oom");
    let spec = Spec::new(&image(), "unused");
    let mut parts = counters();
    parts.push(write(
        "memory.events",
        &["low 0", "high 4", "max 1", "oom 1", "oom_kill 1"],
    ));
    parts.push(words(&["exit", "137"]));
    let outcome = fake.run(1, &spec, &steps(&parts)).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { .. })),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let usage = fake
        .runtime
        .failed_usage(LeaseId::new(1, 1))
        .expect("the OOM-killed lease's usage");
    assert_eq!(usage.peak_memory_bytes, 2_000_000_000);
    let cgroup = usage.cgroup.as_ref().expect("the cgroup's counters");
    assert_eq!(
        (cgroup.memory_peak_bytes, cgroup.cpu_usage_micros),
        (Some(2_000_000_000), Some(3_000_000))
    );
    assert_eq!(cgroup.numa, vec![node(0, 800), node(1, 200)]);
    assert_eq!(
        fake.runtime.failed_usage(LeaseId::new(1, 1)),
        None,
        "handed over once"
    );

    let line = LeasePhases::new(LeaseId::new(1, 1), "container", Duration::ZERO, None)
        .failed_with(Some(&usage), true)
        .to_string();
    assert!(
        line.contains(" cpu_usage_us=3000000 ") && line.contains(" mem_peak=2000000000 "),
        "{line}"
    );
    assert!(line.ends_with(" oom=1"), "{line}");
}

/// Catches a timed-out lease that returns with nothing counted (the CPU of an action
/// that ran out of time): on the code before this change `failed_usage` has nothing, so
/// the first assertion fails. No clock is waited for: the timeout is an hour on a paused
/// clock, which the test moves past it once the action has written its counters, and
/// the loop that waits for them never lets the runtime idle (idling would move the
/// clock by itself, past the timeout before the action wrote anything).
#[tokio::test(start_paused = true)]
async fn a_timed_out_lease_reports_its_cpu_and_peak_memory() {
    let fake = Fake::new("usage-timeout");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_secs(3600));
    let mut parts = counters();
    parts.push(write("ready", &[]));
    parts.push(words(&["sleep", "600"]));
    let action = steps(&parts);
    let counted = async {
        let ready = fake.lease_cgroup(1).join("ready");
        while !ready.exists() {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(3601)).await;
    };
    let (outcome, ()) = tokio::join!(fake.run(1, &spec, &action), counted);
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let usage = fake
        .runtime
        .failed_usage(LeaseId::new(1, 1))
        .expect("the timed-out lease's usage");
    let cgroup = usage.cgroup.as_ref().expect("the cgroup's counters");
    assert_eq!(
        (cgroup.cpu_usage_micros, cgroup.memory_peak_bytes),
        (Some(3_000_000), Some(2_000_000_000))
    );
    let line = LeasePhases::new(LeaseId::new(1, 1), "container", Duration::ZERO, None)
        .failed_with(Some(&usage), false)
        .to_string();
    assert!(
        line.contains(" cpu_usage_us=3000000 ") && line.contains(" mem_peak=2000000000 "),
        "{line}"
    );
    assert!(!line.contains("oom="), "{line}");
}

/// Catches: the lease path not recording what the lease started under (the defect that
/// replaces the start's reading with the default): the CPU booked for the lease and the
/// node's CPU pressure at its start must ride in the result's usage, on the cgroup basis;
/// and a node without a pressure file must report the pressure absent, not zero.
#[tokio::test]
async fn a_lease_reports_its_booked_cpu_and_the_node_cpu_pressure_at_its_start() {
    use kbf_proto::worker::UsageBasis;
    use kbf_types::Resources;

    let fake = Fake::with("usage-start", |c| {
        c.cpu_pressure_file = c.scratch.join("cpu-pressure");
    });
    std::fs::write(
        fake.scratch.join("cpu-pressure"),
        "some avg10=12.34 avg60=3.20 avg300=1.05 total=123456\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
    )
    .expect("pressure file");
    let spec = Spec::new(&image(), "unused");
    let action = words(&["exit", "0"]);
    let result = fake
        .run_with(1, &spec, &action, Resources::new(1500, 1 << 30))
        .await
        .expect("ran");
    let usage = ResourceUsage::in_result(&result).expect("the result carries usage");
    assert_eq!(usage.basis(), UsageBasis::Cgroup);
    assert_eq!(usage.booked_millicpus, Some(1500));
    assert_eq!(usage.node_cpu_pressure_centi, Some(1234));

    // The same node with no pressure file: the booking stands, the pressure is absent.
    std::fs::remove_file(fake.scratch.join("cpu-pressure")).expect("remove");
    let result = fake
        .run_with(2, &spec, &action, Resources::new(2000, 1 << 30))
        .await
        .expect("ran");
    let usage = ResourceUsage::in_result(&result).expect("usage");
    assert_eq!(usage.booked_millicpus, Some(2000));
    assert_eq!(usage.node_cpu_pressure_centi, None);
}

/// Catches the runtime reading the node load from a path other than the configured
/// `actions/` cgroup (the parent's leading `/` joined as an absolute path), the clock
/// given to the runtime being ignored, and a lease nobody ran having a peak.
#[tokio::test]
async fn the_runtime_reads_the_actions_cgroup_with_its_clock() {
    let fake = Fake::new("node-load");
    let actions = fake.cgroup.join("actions");
    for (file, text) in [
        ("cpu.stat", "usage_usec 42\nthrottled_usec 7\n"),
        ("memory.current", "1000\n"),
        ("memory.stat", "inactive_file 400\nactive_file 150\n"),
        ("cpu.max", "max 100000\n"),
    ] {
        std::fs::write(actions.join(file), text).expect("file");
    }
    let runtime = kbf_driver_container::ContainerRuntime::new(
        fake.config.clone(),
        std::sync::Arc::clone(&fake.cas),
    )
    .expect("runtime")
    .with_clock(std::sync::Arc::new(|| 9_000));
    let load = runtime.node_load().await.expect("reading");
    assert_eq!(
        (
            load.seq,
            load.mono_usec,
            load.cpu_usage_usec,
            load.working_set_bytes
        ),
        (1, 9_000, 42, 600)
    );
    assert_eq!(load.active_file_bytes, 150);
    assert_eq!(load.throttled_usec, Some(7));
    assert_eq!(load.pressure_some_total_usec, None);
    assert!(!load.actions_cpu_limited && load.covered.is_empty());
    assert_eq!(
        runtime.take_peak_window_millicores(LeaseId { term: 1, seq: 1 }),
        None
    );
}

/// What the node's load reading says of the actions cgroup's limit, and the text of its
/// `cpu.max`, after the runtime was told what a Welcome said.
async fn told(fake: &Fake, placement_load: bool) -> (bool, String) {
    Runtime::welcomed_placement_load(&*fake.runtime, placement_load).await;
    let load = Runtime::node_load(&*fake.runtime)
        .await
        .expect("reading")
        .expect("a container runtime takes readings");
    let written = std::fs::read_to_string(fake.cgroup.join("actions/cpu.max")).expect("cpu.max");
    (load.load.actions_cpu_limited, written)
}

/// A fake node whose actions cgroup holds the limit the start wrote (4 CPUs), with the
/// runtime following `mode` over 4 CPUs; `None` is a runtime given no mode.
fn limited_node(name: &str, mode: Option<ActionsCpuMax>) -> Fake {
    let fake = Fake::with(name, |config| {
        config.actions_cpu = mode.map(|mode| ActionsCpu {
            mode,
            lease_cpus: 4,
        });
    });
    let actions = fake.cgroup.join("actions");
    for (file, text) in [
        ("cpu.stat", "usage_usec 42\n"),
        ("memory.current", "1000\n"),
        ("memory.stat", "inactive_file 400\nactive_file 150\n"),
        ("cpu.max", "400000 100000"),
    ] {
        std::fs::write(actions.join(file), text).expect("file");
    }
    fake
}

const RESERVE: &str = "400000 100000";
const LIFTED: &str = "max 100000";

/// Catches `--actions-cpu-max=auto` not following the Welcome, and a heartbeat that does not
/// report what was written: until the first Welcome the limit of the start stays; a Welcome
/// that says true lifts it; one that says false (a reconnect to a server in another mode)
/// writes the reserve's limit again; and the reading (`actions_cpu_limited`) is the opposite
/// of what is lifted each time.
#[tokio::test]
async fn auto_follows_each_welcome_and_the_reading_reports_it() {
    let fake = limited_node("cpu-max-auto", Some(ActionsCpuMax::Auto));
    let before = Runtime::node_load(&*fake.runtime).await.expect("reading");
    assert!(before.expect("reading").load.actions_cpu_limited);
    for (placement_load, limited, written) in [
        (true, false, LIFTED),
        (false, true, RESERVE),
        (true, false, LIFTED),
    ] {
        assert_eq!(
            told(&fake, placement_load).await,
            (limited, written.to_owned()),
            "Welcome {placement_load}"
        );
    }
}

/// Catches `on` and `off` yielding to the Welcome: each writes the same whatever it says.
#[tokio::test]
async fn on_and_off_override_each_welcome() {
    for (mode, limited, written) in [
        (ActionsCpuMax::On, false, LIFTED),
        (ActionsCpuMax::Off, true, RESERVE),
    ] {
        let fake = limited_node(&format!("cpu-max-{mode:?}"), Some(mode));
        for placement_load in [true, false, true] {
            assert_eq!(
                told(&fake, placement_load).await,
                (limited, written.to_owned()),
                "{mode:?} with Welcome {placement_load}"
            );
        }
    }
}

/// Catches a runtime given no mode writing to `cpu.max` (a runtime built without the
/// node's setting leaves the limit of the start), and a failed write ending the stream or
/// changing the limit: a `cpu.max` that cannot be written is logged and stays as it was.
#[tokio::test]
async fn without_a_mode_or_when_the_write_fails_the_limit_stays() {
    let none = limited_node("cpu-max-none", None);
    assert_eq!(told(&none, true).await, (true, RESERVE.to_owned()));

    let fake = limited_node("cpu-max-unwritable", Some(ActionsCpuMax::On));
    let cpu_max = fake.cgroup.join("actions/cpu.max");
    std::fs::remove_file(&cpu_max).expect("rm");
    std::fs::create_dir(&cpu_max).expect("a directory cannot be written as a file");
    Runtime::welcomed_placement_load(&*fake.runtime, true).await;
    assert!(cpu_max.is_dir(), "nothing was written over it");
}
