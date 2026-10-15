use std::path::PathBuf;

use super::*;

/// A fresh directory beside the test binary (inside the target directory).
fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-daemon-unit")
        .join("numa")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn node_dir(root: &Path, name: &str, cpulist: Option<&str>) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("node dir");
    if let Some(list) = cpulist {
        std::fs::write(dir.join("cpulist"), list).expect("cpulist");
    }
}

/// Catches nodes listed out of order (`node10` before `node2` by name), a CPU list
/// miscounted (a range's end counted exclusive, a single CPU taken for a range),
/// entries beside the nodes (`possible`, `online`, a `node` without a number) read as
/// nodes, and the line printed in another shape than the one the docs name.
#[test]
fn a_two_node_host_is_read_in_node_order_with_its_cpu_counts() {
    let root = scratch("two");
    node_dir(&root, "node1", Some("16-31,48-63\n"));
    node_dir(&root, "node0", Some("0-15,32-47\n"));
    std::fs::write(root.join("possible"), "0-1\n").expect("file");
    node_dir(&root, "nodeX", Some("0\n"));
    let layout = Layout::read(&root);
    assert_eq!(
        layout.nodes,
        [
            Node {
                id: 0,
                cpulist: "0-15,32-47".to_owned(),
                cpus: 32
            },
            Node {
                id: 1,
                cpulist: "16-31,48-63".to_owned(),
                cpus: 32
            },
        ]
    );
    assert_eq!(layout.cpus(), 64);
    assert_eq!(
        layout.to_string(),
        "nodes=2 cpus=64 node0=0-15,32-47 node1=16-31,48-63"
    );

    // Numeric order, not name order.
    let many = scratch("many");
    node_dir(&many, "node10", Some("5\n"));
    node_dir(&many, "node2", Some("0-3,8,10-11\n"));
    let ids: Vec<(u32, u32)> = Layout::read(&many)
        .nodes
        .iter()
        .map(|n| (n.id, n.cpus))
        .collect();
    assert_eq!(ids, [(2, 7), (10, 1)]);
}

/// Catches a host with no node directory (macOS, no NUMA support) failing instead of
/// reading as an empty layout, a memory-only node (an empty `cpulist`) dropped, and
/// a node whose list is unreadable or malformed counted as a node with CPUs.
#[test]
fn a_host_without_nodes_or_with_odd_ones_is_not_an_error() {
    let gone = scratch("none").join("missing");
    assert_eq!(Layout::read(&gone), Layout::default());
    assert_eq!(Layout::default().to_string(), "nodes=0 cpus=0");

    let root = scratch("odd");
    node_dir(&root, "node0", Some("0-3\n"));
    node_dir(&root, "node1", Some("\n"));
    node_dir(&root, "node2", None);
    node_dir(&root, "node3", Some("3-1\n"));
    node_dir(&root, "node4", Some("a-b\n"));
    let layout = Layout::read(&root);
    assert_eq!(layout.to_string(), "nodes=2 cpus=4 node0=0-3 node1=-");
}
