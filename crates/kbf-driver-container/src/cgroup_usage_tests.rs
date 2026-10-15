use std::path::PathBuf;

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
fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join("cgroup-usage")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
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

/// Catches a sampler that never samples (the memory of the run is gone by the end): the
/// file's value is read while the watch is polled, and gone from the file by the end.
#[tokio::test]
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
