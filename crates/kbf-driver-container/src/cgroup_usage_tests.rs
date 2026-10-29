use kbf_proto::worker::NumaNodeBytes;

use super::*;

/// A `cpu.stat` of a lease cgroup on a v2 kernel with the cpu controller enabled and no
/// `cpu.max` (what kbf sets): CPU time, and the bandwidth counters at zero.
const CPU_STAT: &str = "usage_usec 4812345
user_usec 3912345
system_usec 900000
nice_usec 0
core_sched.force_idle_usec 0
nr_periods 0
nr_throttled 0
throttled_usec 0
nr_bursts 0
burst_usec 0
";

/// A `memory.numa_stat` on a two-node host (cgroup v2: bytes, one line per counter).
const NUMA_STAT: &str = "anon N0=1073741824 N1=268435456
file N0=536870912 N1=134217728
kernel_stack N0=65536 N1=32768
pagetables N0=2097152 N1=1048576
sec_pagetables N0=0 N1=0
percpu N0=0 N1=0
sock N0=0 N1=0
shmem N0=4096 N1=0
file_mapped N0=1048576 N1=0
file_dirty N0=0 N1=0
file_writeback N0=0 N1=0
swapcached N0=0 N1=0
anon_thp N0=0 N1=0
file_thp N0=0 N1=0
shmem_thp N0=0 N1=0
inactive_anon N0=1073741824 N1=268435456
active_anon N0=0 N1=0
inactive_file N0=268435456 N1=67108864
active_file N0=268435456 N1=67108864
unevictable N0=0 N1=0
slab_reclaimable N0=1048576 N1=0
slab_unreclaimable N0=2097152 N1=0
workingset_refault_anon N0=0 N1=0
";

/// A fresh directory beside the test binary (inside the target directory).
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    crate::unit_dir(&format!("cgroup-usage-{name}"))
}

fn write(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).expect("write");
}

fn node(node: u32, bytes: u64) -> NumaNodeBytes {
    NumaNodeBytes { node, bytes }
}

/// Catches a `cpu.stat` key read into the wrong field (user and system swapped, the
/// throttled count taken for the throttled time).
#[test]
fn cpu_stat_of_a_v2_lease_cgroup_is_read_key_by_key() {
    assert_eq!(
        cpu_stat(CPU_STAT),
        CpuStat {
            usage: Some(4_812_345),
            user: Some(3_912_345),
            system: Some(900_000),
            periods: Some(0),
            throttled: Some(0),
            throttled_usec: Some(0),
        }
    );
    let throttled = "usage_usec 10\nnr_periods 40\nnr_throttled 7\nthrottled_usec 123456\n";
    let stat = cpu_stat(throttled);
    assert_eq!(
        (stat.periods, stat.throttled, stat.throttled_usec),
        (Some(40), Some(7), Some(123_456))
    );
}

/// Catches only `anon` or only `file` counted, another counter (`kernel_stack`,
/// `shmem`, the `inactive_*` subsets) added in, or a node's bytes lost.
#[test]
fn numa_stat_is_anon_plus_file_per_node() {
    let nodes = numa_stat(NUMA_STAT);
    assert_eq!(
        nodes.into_iter().collect::<Vec<_>>(),
        [
            (0, 1_073_741_824 + 536_870_912),
            (1, 268_435_456 + 134_217_728)
        ]
    );
}

/// Catches a file in cgroup v1's shape (`user 5` lines, no `usage_usec`), a line that
/// is not a number and a numa_stat without NUMA taken for values or an error.
#[test]
fn files_of_another_shape_leave_their_fields_absent() {
    assert_eq!(cpu_stat("user 123\nsystem 45\n"), CpuStat::default());
    assert_eq!(
        cpu_stat("usage_usec lots\nuser_usec\n\n"),
        CpuStat::default()
    );
    assert!(numa_stat("").is_empty());
    assert!(numa_stat("anon 4096\nfile\nkernel_stack N0=1\n").is_empty());
    let odd = numa_stat("anon N0=many Nx=5 N1=2\n");
    assert_eq!(odd.into_iter().collect::<Vec<_>>(), [(1, 2)]);
}

/// Catches a missing file (a cgroup v1 host, a kernel before `memory.peak`, no NUMA)
/// made an error, or a zero in place of an absent field: the usage must say
/// "not measured", which a zero would not.
#[test]
fn a_cgroup_without_the_files_has_every_field_absent() {
    let dir = scratch("empty");
    let usage = NumaSample::default().finish(&dir);
    assert_eq!(usage, CgroupUsage::default());
    assert!(usage.cpu_usage_micros.is_none() && usage.memory_peak_bytes.is_none());
    assert!(usage.numa.is_empty());

    // The directory itself gone (a lease that never made its cgroup): the same.
    let gone = dir.join("missing");
    assert_eq!(NumaSample::default().finish(&gone), CgroupUsage::default());
}

/// Catches a file that does not parse (`memory.peak` as "max" or empty) taken for 0.
#[test]
fn a_file_that_does_not_parse_leaves_its_field_absent() {
    let dir = scratch("junk");
    write(&dir, "memory.peak", "max\n");
    write(&dir, "cpu.stat", "garbage\n");
    write(&dir, "memory.numa_stat", "");
    assert_eq!(NumaSample::default().finish(&dir), CgroupUsage::default());
}

/// Catches a field of the usage left unfilled, or filled from the wrong file, with all
/// the files of a two-node v2 lease there.
#[test]
fn a_lease_cgroup_with_every_file_fills_every_field() {
    let dir = scratch("full");
    write(&dir, "cpu.stat", CPU_STAT);
    write(&dir, "memory.peak", "2147483648\n");
    write(&dir, "memory.numa_stat", NUMA_STAT);
    let usage = NumaSample::default().finish(&dir);
    assert_eq!(
        usage,
        CgroupUsage {
            cpu_usage_micros: Some(4_812_345),
            cpu_user_micros: Some(3_912_345),
            cpu_system_micros: Some(900_000),
            cpu_periods: Some(0),
            cpu_throttled_periods: Some(0),
            cpu_throttled_micros: Some(0),
            memory_peak_bytes: Some(2_147_483_648),
            numa: vec![node(0, 1_610_612_736), node(1, 402_653_184)],
            ..CgroupUsage::default()
        }
    );
}

/// Catches the NUMA split taken from the last sample only: after the processes exit the
/// anon memory is uncharged, so the end shows little, and the split that mattered (read
/// while the action ran) must be kept. Also that a sample that cannot be read (the
/// file gone) clears nothing.
#[test]
fn the_numa_sample_with_the_most_memory_wins_over_a_later_smaller_one() {
    let dir = scratch("best");
    let mut sample = NumaSample::default();
    write(
        &dir,
        "memory.numa_stat",
        "anon N0=100 N1=900\nfile N0=0 N1=0\n",
    );
    sample.observe(&dir);
    write(
        &dir,
        "memory.numa_stat",
        "anon N0=0 N1=0\nfile N0=40 N1=10\n",
    );
    sample.observe(&dir);
    std::fs::remove_file(dir.join("memory.numa_stat")).expect("remove");
    sample.observe(&dir);
    // The end: a still smaller sample.
    write(&dir, "memory.numa_stat", "anon N0=1 N1=1\n");
    let usage = sample.finish(&dir);
    assert_eq!(usage.numa, vec![node(0, 100), node(1, 900)]);

    // And a later, larger one replaces it.
    let mut sample = NumaSample::default();
    write(&dir, "memory.numa_stat", "anon N0=100 N1=900\n");
    sample.observe(&dir);
    write(&dir, "memory.numa_stat", "anon N0=2000 N1=900\n");
    assert_eq!(sample.finish(&dir).numa, vec![node(0, 2000), node(1, 900)]);
}

/// Catches the count of samples (what a test waits on to know the sampler ran) counting
/// a reading that held no memory, or one that could not be read: only a reading with
/// memory in it is one.
#[test]
fn only_a_reading_with_memory_is_counted() {
    let dir = scratch("counted");
    let counted = Arc::new(AtomicU64::new(0));
    let mut sample = NumaSample::counting(Arc::clone(&counted));
    sample.observe(&dir);
    write(&dir, "memory.numa_stat", "anon N0=0 N1=0\nfile N0=0 N1=0\n");
    sample.observe(&dir);
    assert_eq!(counted.load(Ordering::Acquire), 0);
    write(&dir, "memory.numa_stat", "anon N0=1 N1=0\n");
    sample.observe(&dir);
    sample.observe(&dir);
    assert_eq!(counted.load(Ordering::Acquire), 2);
}

/// Catches a sampler that never samples (the memory of the run is gone by the end): the
/// file's value is read while the watch is polled, and gone from the file by the end.
/// The clock is paused: it moves only when everything waits for a timer, so the ticks
/// all fall before the end of the select, whatever the machine's speed.
#[tokio::test(start_paused = true)]
async fn the_watch_samples_while_the_action_runs() {
    let dir = scratch("watch");
    write(&dir, "memory.numa_stat", "anon N0=500 N1=500\n");
    let mut sample = NumaSample::default();
    let every = Duration::from_millis(5);
    tokio::select! {
        () = sample.watch(&dir, every) => unreachable!("the watch never ends"),
        () = tokio::time::sleep(every * 20) => {}
    }
    write(&dir, "memory.numa_stat", "");
    assert_eq!(sample.finish(&dir).numa, vec![node(0, 500), node(1, 500)]);
}

/// A `/proc/pressure/cpu` as a kernel with pressure stall information writes it.
const CPU_PRESSURE: &str = "some avg10=12.34 avg60=3.20 avg300=1.05 total=123456\n\
                            full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";

/// Catches: the CPU pressure read from the wrong line or window (`full`, `avg60`), a
/// fraction scaled wrong (12.34 is 1234 hundredths), a one-digit fraction read as
/// tenths of that, and text that is not a pressure line read as a number.
#[test]
fn the_cpu_pressure_is_the_some_avg10_in_hundredths_of_a_percent() {
    assert_eq!(some_avg10_centi(CPU_PRESSURE), Some(1234));
    assert_eq!(some_avg10_centi("some avg10=0.00 avg60=9.00\n"), Some(0));
    assert_eq!(some_avg10_centi("some avg10=7.5 avg60=0.00\n"), Some(750));
    assert_eq!(
        some_avg10_centi("some avg10=100.00 avg60=0.00\n"),
        Some(10_000)
    );
    assert_eq!(some_avg10_centi("some avg10=1.239 avg60=0.00\n"), Some(123));
    assert_eq!(some_avg10_centi("full avg10=5.00 avg60=0.00\n"), None);
    assert_eq!(some_avg10_centi("some avg60=5.00\n"), None);
    assert_eq!(some_avg10_centi("some avg10=x avg60=5.00\n"), None);
    assert_eq!(some_avg10_centi("some avg10=-1.00\n"), None);
    assert_eq!(some_avg10_centi("some avg10=.50\n"), None);
    assert_eq!(some_avg10_centi("some avg10=1.5x avg60=0.00\n"), None);
    assert_eq!(some_avg10_centi("some avg10=99999999999.00\n"), None);
    assert_eq!(some_avg10_centi(""), None);
}

/// Catches: a missing pressure file (a kernel without pressure stall information, or
/// PSI disabled) read as zero pressure, which would tell the estimator the node was idle
/// when nothing is known; an unparsable file treated as readable; and a zero booking
/// reported as a booking of 0.
#[test]
fn the_cpu_pressure_is_absent_when_the_file_is_missing_and_a_zero_booking_is_none() {
    let dir = scratch("cpu-psi");
    let missing = dir.join("pressure-cpu");
    assert_eq!(
        Start::read(&missing, 1500),
        Start {
            cpu_pressure_centi: None,
            booked_millicpus: Some(1500)
        }
    );
    std::fs::write(&missing, "garbled\n").unwrap();
    assert_eq!(Start::read(&missing, 0), Start::default());
    std::fs::write(&missing, CPU_PRESSURE).unwrap();
    assert_eq!(
        Start::read(&missing, 2000),
        Start {
            cpu_pressure_centi: Some(1234),
            booked_millicpus: Some(2000)
        }
    );
    // A measured idle node is zero, not absent.
    std::fs::write(&missing, "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
    assert_eq!(Start::read(&missing, 1).cpu_pressure_centi, Some(0));
}

/// Catches: the container driver's usage not saying it is a cgroup measurement (the
/// estimator would key it with an unknown basis), no footprint block beside the cgroup
/// one, and the start's evidence dropped from the report.
#[test]
fn the_container_usage_is_on_the_cgroup_basis_with_no_footprint() {
    let usage = resource_usage(
        CgroupUsage {
            cpu_user_micros: Some(3),
            memory_peak_bytes: Some(9),
            ..CgroupUsage::default()
        },
        Duration::from_micros(77),
        Start {
            cpu_pressure_centi: Some(5),
            booked_millicpus: Some(1000),
        },
    );
    assert_eq!(usage.basis(), UsageBasis::Cgroup);
    assert_eq!(usage.footprint, None);
    assert_eq!((usage.peak_memory_bytes, usage.wall_micros), (9, 77));
    assert_eq!(usage.node_cpu_pressure_centi, Some(5));
    assert_eq!(usage.booked_millicpus, Some(1000));
    let bare = resource_usage(CgroupUsage::default(), Duration::ZERO, Start::default());
    assert_eq!(
        (bare.node_cpu_pressure_centi, bare.booked_millicpus),
        (None, None)
    );
}

/// Catches: `max` and `oom_kill` swapped or read from another key (`high`, `oom`), a
/// pressure total read from the wrong line or taken for the average, a CPU list counted
/// by its ranges instead of its members, and a malformed list counted at all.
#[test]
fn events_pressure_totals_and_cpu_lists_are_read_by_key() {
    let events = "low 0\nhigh 5\nmax 7\noom 2\noom_kill 1\noom_group_kill 0\n";
    assert_eq!(memory_events(events), (Some(7), Some(1)));
    assert_eq!(memory_events("high 5\n"), (None, None));
    assert_eq!(memory_events("max x\noom_kill\n"), (None, None));
    let pressure = "some avg10=0.10 avg60=0.00 avg300=0.00 total=4120\n\
                    full avg10=0.00 avg60=0.00 avg300=0.00 total=77\n";
    assert_eq!(pressure_total(pressure, "some"), Some(4120));
    assert_eq!(pressure_total(pressure, "full"), Some(77));
    assert_eq!(pressure_total("some avg10=0.10\n", "some"), None);
    assert_eq!(pressure_total("", "full"), None);
    assert_eq!(cpu_list_len("0-3,8,10-11\n"), Some(7));
    assert_eq!(cpu_list_len("5"), Some(1));
    assert_eq!(cpu_list_len(""), None);
    assert_eq!(cpu_list_len("3-1"), None);
    assert_eq!(cpu_list_len("a-b"), None);
    assert_eq!(anon_bytes("file 9\nanon 4096\nslab 1\n"), Some(4096));
    assert_eq!(anon_bytes("anon_thp 1\n"), None);
}

/// Catches: the lease's counters (events, swap peak, pressure totals, CPUs) lost on the
/// way to the report, and a counter the kernel lacks (no `memory.swap.peak` before 6.5,
/// no pressure files without PSI, no `memory.peak` before 5.19) reported as 0.
#[test]
fn the_lease_counters_are_reported_and_a_missing_file_is_absent_not_zero() {
    let dir = scratch("counters");
    write(&dir, "memory.peak", "1048576\n");
    write(&dir, "memory.events", "max 3\noom_kill 0\n");
    write(&dir, "memory.swap.peak", "2048\n");
    write(
        &dir,
        "cpu.pressure",
        "some avg10=0.00 avg60=0.00 avg300=0.00 total=500\n",
    );
    write(
        &dir,
        "memory.pressure",
        "some avg10=0.00 avg60=0.00 avg300=0.00 total=60\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=9\n",
    );
    write(&dir, "cpuset.cpus.effective", "0-7\n");
    let usage = NumaSample::default().finish(&dir);
    assert_eq!(usage.memory_peak_bytes, Some(1_048_576));
    assert_eq!(
        (usage.memory_events_max, usage.memory_events_oom_kill),
        (Some(3), Some(0))
    );
    assert_eq!(usage.memory_swap_peak_bytes, Some(2048));
    assert_eq!(usage.cpu_pressure_some_micros, Some(500));
    assert_eq!(
        (
            usage.memory_pressure_some_micros,
            usage.memory_pressure_full_micros
        ),
        (Some(60), Some(9))
    );
    assert_eq!(usage.cpus_effective, Some(8));
    assert_eq!(
        usage.memory_anon_swap_peak_bytes, None,
        "no sample could be read"
    );

    // An old kernel without PSI, `memory.peak` or `memory.swap.peak`.
    let old = scratch("counters-old");
    write(&old, "memory.events", "max 0\noom_kill 0\n");
    let usage = NumaSample::default().finish(&old);
    assert_eq!(usage.memory_peak_bytes, None);
    assert_eq!(usage.memory_swap_peak_bytes, None);
    assert_eq!(usage.cpu_pressure_some_micros, None);
    assert_eq!(usage.memory_pressure_full_micros, None);
    assert_eq!(usage.cpus_effective, None);
    assert_eq!(usage.memory_events_max, Some(0), "a zero count is a count");
}

/// Catches: the anon + swap peak taken from the last sample instead of the largest (the
/// anon memory is gone once the processes exit, so the end reads little), a sample with
/// only one of its two files counted as anon + swap, and swap left out of the sum.
#[test]
fn the_anon_swap_peak_is_the_largest_sample_with_both_files() {
    let dir = scratch("anon-swap");
    let mut sample = NumaSample::default();
    write(&dir, "memory.stat", "anon 1000\nfile 5\n");
    sample.observe(&dir);
    assert_eq!(
        sample.anon_swap_peak, None,
        "no memory.swap.current: not a sample"
    );
    write(&dir, "memory.swap.current", "300\n");
    sample.observe(&dir);
    assert_eq!(sample.anon_swap_peak, Some(1300));
    write(&dir, "memory.stat", "anon 10\n");
    write(&dir, "memory.swap.current", "0\n");
    sample.observe(&dir);
    assert_eq!(
        sample.anon_swap_peak,
        Some(1300),
        "a smaller sample keeps the peak"
    );
    assert_eq!(sample.finish(&dir).memory_anon_swap_peak_bytes, Some(1300));
}
