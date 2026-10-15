//! Topology parsing, placement and the start-up fallbacks of `--lease-placement`.

use std::path::{Path, PathBuf};

use super::*;

fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join(format!("numa-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn set(text: &str) -> CpuSet {
    text.parse().expect("a cpu list")
}

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A sysfs node directory with one `node<N>/cpulist` per entry.
fn sysfs(name: &str, nodes: &[(u32, &str)]) -> PathBuf {
    let dir = scratch(name).join("node");
    std::fs::create_dir_all(&dir).expect("mkdir");
    for (id, list) in nodes {
        put(&dir.join(format!("node{id}/cpulist")), &format!("{list}\n"));
    }
    // Entries of the real directory that are not nodes.
    put(&dir.join("possible"), "0-87\n");
    put(&dir.join("has_cpu"), "0-1\n");
    dir
}

const NODE0: &str = "0-21,44-65";
const NODE1: &str = "22-43,66-87";

/// Catches a cpulist parsed without its ranges, its commas, its single numbers or its
/// empty form (a memory-only node), and a set printed in a form the kernel refuses.
#[test]
fn cpu_lists_parse_and_print_as_the_kernel_writes_them() {
    let node0 = set(NODE0);
    assert_eq!(node0.len(), 44);
    assert_eq!(node0.to_string(), NODE0);
    assert_eq!(set("3").to_string(), "3");
    assert_eq!(set("0,2-3,9").to_string(), "0,2-3,9");
    assert_eq!(set("1-2,3-4").to_string(), "1-4", "adjacent runs join");
    assert!(set("").is_empty());
    assert!(set("\n").is_empty());
    assert_eq!(set("0-3,8\n").len(), 5);
    for bad in ["x", "3-1", "1-", "-2", "1,,2", "0-3;5"] {
        assert!(bad.parse::<CpuSet>().is_err(), "{bad:?}");
    }
    assert_eq!(set(NODE0).intersection(&set("40-50")).to_string(), "44-50");
}

/// Catches the topology read from the wrong files: `node<N>/cpulist` per node, the
/// directory's other entries (`possible`, `has_cpu`) ignored, nodes in id order (not
/// directory order, and 10 after 2).
#[test]
fn the_topology_is_read_from_each_nodes_cpulist() {
    let dir = sysfs("read", &[(1, NODE1), (0, NODE0), (10, "")]);
    let nodes = read_nodes(&dir).expect("read");
    let shown: Vec<(u32, String)> = nodes.iter().map(|(i, c)| (*i, c.to_string())).collect();
    assert_eq!(
        shown,
        [
            (0, NODE0.to_owned()),
            (1, NODE1.to_owned()),
            (10, String::new())
        ]
    );
    assert!(read_nodes(&dir.join("absent")).is_err());
    put(&dir.join("node2/cpulist"), "x\n");
    assert!(read_nodes(&dir).is_err(), "a cpulist that does not parse");
    put(&dir.join("node2/cpulist"), "2\n");
    std::fs::create_dir(dir.join("node3")).expect("mkdir");
    assert!(read_nodes(&dir).is_err(), "a node with no cpulist");
}

fn placer() -> Arc<Placer> {
    Placer::new(vec![(0, set(NODE0)), (1, set(NODE1))]).expect("nodes")
}

/// No node, no placer (a placer over nothing would have no node to choose).
#[test]
fn no_nodes_no_placer() {
    assert!(Placer::new(Vec::new()).is_none());
}

/// Catches a placement that is not the least loaded node: leases alternate when they
/// book alike, the lowest id wins a tie, a heavy lease keeps the next ones on the other
/// node, and a released lease frees its node.
#[test]
fn leases_go_to_the_least_loaded_node_and_release_frees_it() {
    let placer = placer();
    let a = placer.place(1000);
    let b = placer.place(1000);
    let c = placer.place(1000);
    assert_eq!(
        (a.node(), b.node(), c.node()),
        (0, 1, 0),
        "ties go to node 0"
    );
    assert_eq!(a.cpus().to_string(), NODE0);
    assert_eq!(b.cpus().to_string(), NODE1);
    assert_eq!(placer.loads(), [2000, 1000]);
    drop(a);
    drop(c);
    assert_eq!(placer.loads(), [0, 1000]);
    assert_eq!(
        placer.place(1000).node(),
        0,
        "the freed node is the lighter"
    );
    // Weighted: 8 CPUs on node 0 leave node 1 for the next three small leases.
    let placer = self::placer();
    let big = placer.place(8000);
    let small: Vec<Pin> = (0..3).map(|_| placer.place(2000)).collect();
    assert_eq!(big.node(), 0);
    assert_eq!(
        small.iter().map(Pin::node).collect::<Vec<_>>(),
        [1, 1, 1],
        "6 CPUs on node 1 are still under node 0's 8"
    );
    let fourth = placer.place(2000);
    assert_eq!(fourth.node(), 1, "6 < 8: node 1 again");
    let fifth = placer.place(2000);
    assert_eq!(fifth.node(), 0, "8 is not < 8: node 0 on a tie");
}

/// Catches a lease that booked nothing counted as free (a node could then take every
/// unbooked lease): it counts as one CPU.
#[test]
fn a_lease_that_booked_nothing_counts_as_one_cpu() {
    let placer = placer();
    let _a = placer.place(0);
    assert_eq!(placer.loads(), [1000, 0]);
    let b = placer.place(0);
    assert_eq!(b.node(), 1);
    drop(b);
    assert_eq!(placer.loads(), [1000, 0]);
}

/// A host's cgroup tree as plain directories: the unit offers `offered`, `actions/`
/// under it.
fn cgroups(name: &str, offered: &str, effective: Option<&str>) -> (PathBuf, &'static str) {
    let root = scratch(&format!("{name}-cgroups")).join("cgroup");
    let unit = root.join("kbf.slice/kbf.service");
    put(&unit.join("cgroup.controllers"), offered);
    put(&unit.join("cgroup.subtree_control"), "cpu memory pids\n");
    put(
        &unit.join("actions/cgroup.subtree_control"),
        "cpu memory pids\n",
    );
    if let Some(list) = effective {
        put(&unit.join("actions/cpuset.cpus.effective"), list);
    }
    (root, "/kbf.slice/kbf.service/actions")
}

fn read(root: &Path, cgroup: &str, file: &str) -> Option<String> {
    std::fs::read_to_string(root.join(cgroup.trim_start_matches('/')).join(file)).ok()
}

/// Catches a dual-node host that is not placed, a `cpuset` enabled in only one of the two
/// cgroups that need it, and a topology kept as read when the daemon's cpuset reaches
/// less of it.
#[test]
fn a_dual_node_host_is_placed_over_the_cpus_the_daemon_may_use() {
    let dir = sysfs("dual", &[(0, NODE0), (1, NODE1)]);
    let (root, actions) = cgroups("dual", "cpu cpuset memory pids\n", Some("0-87\n"));
    let placer = start(&dir, &root, actions)
        .expect("started")
        .expect("placed");
    assert_eq!(placer.place(1000).cpus().to_string(), NODE0);
    let unit = "/kbf.slice/kbf.service";
    assert_eq!(
        read(&root, unit, "cgroup.subtree_control").as_deref(),
        Some("+cpuset")
    );
    assert_eq!(
        read(&root, actions, "cgroup.subtree_control").as_deref(),
        Some("+cpuset")
    );

    // A cgroup that shows no cpuset.cpus.effective limits nothing.
    let (root, actions) = cgroups("unlimited", "cpu cpuset memory pids\n", None);
    let placer = start(&dir, &root, actions)
        .expect("started")
        .expect("placed");
    assert_eq!(placer.place(1000).cpus().to_string(), NODE0);

    // A cpuset that holds part of node 0 and all of node 1: the lease gets the part.
    let (root, actions) = cgroups("partial", "cpu cpuset memory pids\n", Some("10-30,70-87\n"));
    let placer = start(&dir, &root, actions)
        .expect("started")
        .expect("placed");
    let (a, b) = (placer.place(1000), placer.place(1000));
    assert_eq!(
        (a.cpus().to_string(), b.cpus().to_string()),
        ("10-21".into(), "22-30,70-87".into())
    );
}

/// Catches a host that cannot be placed failing the start or touching its cgroups: a
/// single-node host, a host with no node directory, a node with no CPU, and a daemon
/// confined to one node each run unpinned. The cgroup tree of the first three has no
/// `cgroup.controllers`, so touching it would be an error.
#[test]
fn a_host_without_two_nodes_falls_back_to_no_placement() {
    let no_cgroups = scratch("none").join("cgroup");
    for (name, nodes) in [
        ("single", vec![(0, "0-87")]),
        ("memory-only", vec![(0, "0-87"), (1, "")]),
    ] {
        let dir = sysfs(name, &nodes);
        let placer = start(&dir, &no_cgroups, "/kbf/actions").expect("started");
        assert!(placer.is_none(), "{name}");
    }
    let absent = scratch("absent").join("no-such-dir");
    assert!(
        start(&absent, &no_cgroups, "/kbf/actions")
            .expect("started")
            .is_none()
    );

    let dir = sysfs("confined", &[(0, NODE0), (1, NODE1)]);
    let (root, actions) = cgroups("confined", "cpu cpuset memory pids\n", Some("0-21\n"));
    assert!(start(&dir, &root, actions).expect("started").is_none());
}

/// Catches a two-node host whose cpuset cannot be enabled starting unpinned: the start
/// fails and the message names `cpuset` and the fix.
#[test]
fn a_dual_node_host_that_cannot_enable_cpuset_fails_the_start() {
    let dir = sysfs("refused", &[(0, NODE0), (1, NODE1)]);
    let (root, actions) = cgroups("refused", "cpu memory pids\n", None);
    let error = start(&dir, &root, actions)
        .expect_err("refused")
        .to_string();
    assert!(
        error.contains("cpuset") && error.contains("Delegate="),
        "{error}"
    );
}

/// `--lease-placement` values.
#[test]
fn the_placement_flag_values_parse() {
    assert_eq!("none".parse::<LeasePlacement>(), Ok(LeasePlacement::None));
    assert_eq!(
        "numa-node".parse::<LeasePlacement>(),
        Ok(LeasePlacement::NumaNode)
    );
    assert!("numa".parse::<LeasePlacement>().is_err());
    assert_eq!(LeasePlacement::default(), LeasePlacement::None);
}
