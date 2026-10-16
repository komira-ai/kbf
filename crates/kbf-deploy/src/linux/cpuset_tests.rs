//! Rule `cpuset`'s reading of CPU and NUMA node lists, and its comparison with the node.

use kbf_host::exec::Entry;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;

fn unit(service: &str) -> UnitFile {
    UnitFile::parse(&format!("[Service]\n{service}")).expect("a unit")
}

fn node_with(cpus: &str, nodes: Option<&str>) -> FakeExec {
    let mut exec = FakeExec::new(0);
    let file = |text: &str| Entry::File {
        perms: Perms::new(0o444),
        owner: Owner { uid: 0, gid: 0 },
        bytes: text.as_bytes().to_vec(),
    };
    exec.entries
        .insert(HostPath::new(KEYS[0].2).expect("absolute"), file(cpus));
    if let Some(n) = nodes {
        exec.entries
            .insert(HostPath::new(KEYS[1].2).expect("absolute"), file(n));
    }
    exec
}

/// Catches a list read other than systemd reads it: commas and whitespace both
/// separating, ranges, a later assignment replacing instead of adding, an empty one not
/// resetting, a malformed word (or a reversed range, or a number past systemd's bound)
/// failing the whole list instead of being ignored, and adjacent ranges not merged.
#[test]
fn lists_are_read_as_systemd_reads_them() {
    let read = |s: &str| allowed(&unit(s), "Service", "AllowedCPUs").map(|i| i.to_string());
    assert_eq!(read("AllowedCPUs=0-3,8 9\n").as_deref(), Some("0-3,8-9"));
    assert_eq!(
        read("AllowedCPUs=0-1\nAllowedCPUs=2-3\n").as_deref(),
        Some("0-3")
    );
    assert_eq!(read("AllowedCPUs=0-1\nAllowedCPUs=\n").as_deref(), None);
    assert_eq!(
        read("AllowedCPUs=0-1\nAllowedCPUs=\nAllowedCPUs=4\n").as_deref(),
        Some("4")
    );
    assert_eq!(read("AllowedCPUs=x 3-1 9000 2 5-\n").as_deref(), Some("2"));
    assert_eq!(read("AllowedCPUs=x\n").as_deref(), None);
    assert_eq!(read("AllowedCPUs=4-7,0-3\n").as_deref(), Some("0-7"));
    assert_eq!(read("").as_deref(), None);
    assert_eq!(Ids::strict("0-63\n"), Some(Ids(vec![(0, 63)])));
    assert_eq!(Ids::strict("0,2-3"), Some(Ids(vec![(0, 0), (2, 3)])));
    assert_eq!(Ids::strict("0-"), None);
    assert_eq!(Ids::strict(""), None);
}

/// Catches a narrower list passed (a CPU or a NUMA node of the node left out), a list
/// that covers the node failed, the slice's `[Slice]` not read, and a list that could
/// not be compared passed without a word.
#[test]
fn a_list_narrower_than_the_node_fails_and_one_not_compared_warns() {
    let exec = node_with("0-7\n", Some("0-1\n"));
    let run = |exec: &FakeExec, u: &UnitFile, section| {
        let (mut failures, mut notes) = (Vec::new(), Vec::new());
        check(
            exec,
            "kbf-daemon.service",
            u,
            section,
            &mut failures,
            &mut notes,
        );
        (failures, notes)
    };
    let none: (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    assert_eq!(run(&exec, &unit(""), "Service"), none);
    assert_eq!(
        run(
            &exec,
            &unit("AllowedCPUs=0-7\nAllowedMemoryNodes=0 1\n"),
            "Service"
        ),
        none
    );
    assert_eq!(run(&exec, &unit("AllowedCPUs=0-15\n"), "Service"), none);
    let (failures, notes) = run(
        &exec,
        &unit("AllowedCPUs=0-3\nAllowedMemoryNodes=0\n"),
        "Service",
    );
    assert_eq!(
        failures,
        [
            "kbf-daemon.service sets AllowedCPUs=0-3, narrower than the node's CPUs 0-7: the daemon places leases on every one of them (--lease-placement=numa-node pins a lease to one NUMA node), and the unit's cgroup keeps them off the rest",
            "kbf-daemon.service sets AllowedMemoryNodes=0, narrower than the node's NUMA nodes 0-1: the daemon places leases on every one of them (--lease-placement=numa-node pins a lease to one NUMA node), and the unit's cgroup keeps them off the rest",
        ]
    );
    assert!(notes.is_empty());
    let slice = UnitFile::parse("[Slice]\nAllowedCPUs=1-7\n").expect("a unit");
    assert_eq!(run(&exec, &slice, "Slice").0.len(), 1);
    assert_eq!(run(&exec, &slice, "Service"), none);

    // Offline: the node's NUMA nodes cannot be read, so that list only warns.
    let offline = node_with("0-7\n", None);
    let (failures, notes) = run(&offline, &unit("AllowedMemoryNodes=0\n"), "Service");
    assert!(failures.is_empty());
    assert_eq!(
        notes,
        [
            "WARN cpuset: kbf-daemon.service sets AllowedMemoryNodes=0, and the node's NUMA nodes cannot be read here (no /sys/devices/system/node/online): run check-units on the node to compare them"
        ]
    );
    for (text, why) in [
        (
            Some("garbled"),
            "/sys/devices/system/cpu/online is not a list of numbers",
        ),
        (None, "/sys/devices/system/cpu/online is not a file"),
    ] {
        let mut exec = FakeExec::new(0);
        let entry = match text {
            Some(t) => Entry::File {
                perms: Perms::new(0o444),
                owner: Owner { uid: 0, gid: 0 },
                bytes: t.as_bytes().to_vec(),
            },
            None => Entry::Other,
        };
        exec.entries
            .insert(HostPath::new(KEYS[0].2).expect("absolute"), entry);
        let (_, notes) = run(&exec, &unit("AllowedCPUs=0\n"), "Service");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(&format!("({why})")), "{}", notes[0]);
    }
    assert_eq!(
        node(&exec, "/unreadable").err().as_deref(),
        Some("/unreadable: permission denied")
    );
    assert!(node(&exec, "relative").is_err());
}
