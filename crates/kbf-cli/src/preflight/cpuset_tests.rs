//! Rule `cpuset`, ported from `kbf-deploy linux check-units` (its `cpuset_tests.rs`): the
//! cgroup kbf runs in keeps every CPU and NUMA node of the node, because the container
//! driver places leases on every one of them (`--lease-placement=numa-node` pins each to
//! one NUMA node). check-units read `AllowedCPUs=` and `AllowedMemoryNodes=` in the unit
//! and `kbf.slice`; kbf reads what they cause, the cgroup's `cpuset.cpus.effective` and
//! `cpuset.mems.effective`, which a drop-in elsewhere or `systemctl set-property` shows
//! in too. Each test goes from the flags to the report, on a planted host.

use super::super::tests::{DAEMON, Fake, needs_from};
use super::super::{Class, Report};
use super::{Ids, check};

const CGROUP: &str = "0::/system.slice/kbf-daemon.service\n";
const UNIT: &str = "/sys/fs/cgroup/system.slice/kbf-daemon.service";
const SLICE: &str = "/sys/fs/cgroup/system.slice";
const CPUS: &str = "/sys/devices/system/cpu/online";
/// The nodes with memory, which `cpuset.mems.effective` follows (not the online ones).
const NODES: &str = "/sys/devices/system/node/has_memory";
const ONLINE_NODES: &str = "/sys/devices/system/node/online";

/// A node with CPUs 0-7 and NUMA nodes 0-1, and kbf in `kbf-daemon.service` with `cpus`
/// and `mems` as its cgroup's effective sets.
fn node(cpus: &str, mems: &str) -> Fake {
    Fake::empty()
        .file("/proc/self/cgroup", 0o444, CGROUP.as_bytes())
        .file(CPUS, 0o444, b"0-7\n")
        .file(NODES, 0o444, b"0-1\n")
        .file(ONLINE_NODES, 0o444, b"0-1\n")
        .file(
            &format!("{UNIT}/cpuset.cpus.effective"),
            0o444,
            cpus.as_bytes(),
        )
        .file(
            &format!("{UNIT}/cpuset.mems.effective"),
            0o444,
            mems.as_bytes(),
        )
}

fn run_with(flags: &[&str], host: &Fake) -> Report {
    let mut all = vec!["daemon"];
    all.extend(DAEMON.iter().filter(|f| !f.starts_with("--driver")));
    all.extend(flags);
    let mut report = Report::default();
    check(&needs_from(&all), host, &mut report);
    report
}

fn run(host: &Fake) -> Report {
    run_with(&["--driver=container"], host)
}

fn warned(report: &Report, words: &[&str]) {
    assert!(report.problems.is_empty(), "{report:#?}");
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    for w in words {
        assert!(report.warnings[0].contains(w), "{w}: {report:#?}");
    }
}

/// Catches a kernel list read wrongly (ranges, single numbers, a trailing newline, a
/// reversed range or a malformed word taken for a list), a set that does not cover
/// another passed, and the sets printed other than as the kernel writes them.
#[test]
fn lists_are_read_as_the_kernel_writes_them() {
    assert_eq!(Ids::strict("0-63\n").expect("list").to_string(), "0-63");
    assert_eq!(Ids::strict("0,2-3").expect("list").to_string(), "0,2-3");
    assert_eq!(Ids::strict("4-7,0-3").expect("list").to_string(), "0-7");
    assert_eq!(Ids::strict("0-"), None);
    assert_eq!(Ids::strict("3-1"), None);
    assert_eq!(Ids::strict("x"), None);
    assert_eq!(Ids::strict(""), None);
    let set = |s| Ids::strict(s).expect("list");
    assert!(set("0-15").covers(&set("0-7")));
    assert!(set("0-7").covers(&set("0-7")));
    assert!(!set("0-3").covers(&set("0-7")));
    assert!(!set("0-3,5-7").covers(&set("0-7")));
    assert!(set("0-3,5-7").covers(&set("5,7")));
}

/// Catches a cgroup narrower than the node passed (a CPU or a NUMA node of the node left
/// out), one that covers the node failed, and a daemon on the container driver that is not
/// checked at all.
#[test]
fn a_cgroup_narrower_than_the_node_fails() {
    assert_eq!(run(&node("0-7\n", "0-1\n")), Report::default());
    assert_eq!(run(&node("0-7\n", "0\n")).problems.len(), 1);
    let report = run(&node("0-3\n", "0\n"));
    assert_eq!(report.problems.len(), 2, "{report:#?}");
    assert!(report.problems.iter().all(|p| p.class == Class::Refused));
    let (cpus, mems) = (&report.problems[0], &report.problems[1]);
    assert!(
        cpus.what.contains("AllowedCPUs")
            && cpus.what.contains("0-3")
            && cpus.what.contains("narrower than the node's CPUs 0-7")
            && cpus.what.contains("/system.slice/kbf-daemon.service"),
        "{cpus:?}"
    );
    assert!(
        mems.what.contains("AllowedMemoryNodes")
            && mems
                .what
                .contains("narrower than the node's NUMA nodes 0-1"),
        "{mems:?}"
    );
    assert!(cpus.why.contains("--lease-placement=numa-node"), "{cpus:?}");
    assert!(
        cpus.fix.contains("AllowedCPUs=") && cpus.fix.contains("kbf.slice"),
        "{cpus:?}"
    );
}

/// Catches the slice's limit not read (check-units read `kbf.slice` too): the service's
/// cgroup has no cpuset files of its own, and the slice above it is narrower; the nearest
/// ancestor that has them is the one that speaks for the cgroup.
#[test]
fn the_nearest_ancestor_with_the_files_speaks_for_the_cgroup() {
    let host = Fake::empty()
        .file("/proc/self/cgroup", 0o444, CGROUP.as_bytes())
        .file(CPUS, 0o444, b"0-7\n")
        .file(NODES, 0o444, b"0-1\n")
        .file(&format!("{SLICE}/cpuset.cpus.effective"), 0o444, b"1-7\n")
        .file(&format!("{SLICE}/cpuset.mems.effective"), 0o444, b"0-1\n");
    let report = run(&host);
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    assert!(report.problems[0].what.contains("1-7"), "{report:#?}");
    let root = Fake::empty()
        .file("/proc/self/cgroup", 0o444, b"0::/\n")
        .file(CPUS, 0o444, b"0-7\n")
        .file(NODES, 0o444, b"0-1\n")
        .file("/sys/fs/cgroup/cpuset.cpus.effective", 0o444, b"0-7\n")
        .file("/sys/fs/cgroup/cpuset.mems.effective", 0o444, b"0-1\n");
    assert_eq!(run(&root), Report::default());
}

/// Catches a list that could not be compared passed without a word: no cpuset files on
/// any ancestor, the node's own list missing, garbled or not a file, and the cgroup's list
/// garbled or unreadable. Each is one warning, and the other list is still compared.
#[test]
fn a_list_that_cannot_be_compared_is_a_warning() {
    // Only the NUMA nodes of the node cannot be read; the CPUs are still compared.
    let host = node("0-3\n", "0\n").without(NODES);
    let report = run(&host);
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    assert!(report.warnings[0].contains(NODES), "{report:#?}");
    assert!(report.warnings[0].contains("cannot be read"), "{report:#?}");
    warned(
        &run(&node("0-7\n", "0-1\n").file(CPUS, 0o444, b"garbled")),
        &[CPUS, "not a list"],
    );
    warned(
        &run(&node("0-7\n", "0-1\n").dir(CPUS, 0, 0, 0o755)),
        &[CPUS],
    );
    warned(
        &run(&node("0-7\n", "0-1\n").file(&format!("{UNIT}/cpuset.cpus.effective"), 0o444, b"?")),
        &["cpuset.cpus.effective", "not a list"],
    );
    let bare = Fake::empty()
        .file("/proc/self/cgroup", 0o444, CGROUP.as_bytes())
        .file(CPUS, 0o444, b"0-7\n")
        .file(NODES, 0o444, b"0-1\n");
    let report = run(&bare);
    assert!(report.problems.is_empty(), "{report:#?}");
    assert_eq!(report.warnings.len(), 2, "{report:#?}");
    assert!(
        report.warnings[0].contains("cpuset.cpus.effective"),
        "{report:#?}"
    );
    let denied = node("0-7\n", "0-1\n").unreadable(
        &format!("{UNIT}/cpuset.cpus.effective"),
        std::io::ErrorKind::PermissionDenied,
    );
    let report = run(&denied);
    assert!(report.problems.is_empty(), "{report:#?}");
    assert!(!report.warnings.is_empty(), "{report:#?}");
}

/// Catches a host without cgroup v2 (no `0::` line, or no `/proc/self/cgroup`) passed in
/// silence.
#[test]
fn a_host_without_a_cgroup_v2_path_is_a_warning() {
    let v1 = node("0-3\n", "0\n").file("/proc/self/cgroup", 0o444, b"12:cpuset:/kbf\n");
    warned(&run(&v1), &["/proc/self/cgroup", "cgroup v2"]);
    let none = node("0-3\n", "0\n").without("/proc/self/cgroup");
    warned(&run(&none), &["/proc/self/cgroup"]);
}

/// Catches the rule run for a daemon that is not on the container driver (no leases are
/// placed on CPUs there, and a CI container's cpuset is none of its business): nothing is
/// read, and nothing said.
#[test]
fn only_the_container_driver_places_leases() {
    let narrow = node("0-3\n", "0\n");
    assert_eq!(run_with(&["--driver=fake"], &narrow), Report::default());
    // The frontend and the storage node run no builds.
    let mut report = Report::default();
    check(
        &needs_from(&["frontend", "--storage=http://127.0.0.1:8983"]),
        &narrow,
        &mut report,
    );
    assert_eq!(report, Report::default());
}

/// Catches the NUMA nodes compared with the online ones: `cpuset.mems.effective` holds
/// only nodes with memory, so a host with a memory-less node (online 0-1, memory on 0) would
/// refuse every daemon start. A real narrowing (the cgroup misses a node that has memory)
/// still fails.
#[test]
fn a_numa_node_without_memory_is_not_a_narrowing() {
    let memoryless = node("0-7\n", "0\n").file(NODES, 0o444, b"0\n");
    assert_eq!(run(&memoryless), Report::default());
    let narrowed = node("0-7\n", "1\n").file(NODES, 0o444, b"0-1\n");
    let report = run(&narrowed);
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    assert!(
        report.problems[0]
            .what
            .contains("narrower than the node's NUMA nodes 0-1")
    );
}
