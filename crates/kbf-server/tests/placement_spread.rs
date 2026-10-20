//! Placement spreads leases end to end: daemons register their CPUs and memory in
//! `Hello`, actions are executed and held (no daemon reports a result), and the `Start`s
//! the daemons receive show where the server placed them. A lease goes to the worker
//! with the least booked, as a share of its capacity (the larger of its CPU share and
//! its memory share), not to the first worker in name order that has room.
//!
//! First fit gave every lease to the node named first until it was full: on a farm of
//! six nodes that was about half of all leases on one node, which ran at several times
//! its cores while the others ran lightly.

mod support;

use kbf_proto::google::longrunning::Operation;
use kbf_proto::worker::{Start, server_message};
use support::{Cell, FakeDaemon, Job, QUIET, hello_on};
use tonic::Streaming;

const MAC: &[(&str, &str)] = &[("arch", "arm64"), ("os", "macos")];

/// A booking: the platform-property entries that size an action.
type Size = &'static [(&'static str, &'static str)];

/// No sizing: one core and the ladder's first rung (1 GiB in a test cell).
const DEFAULT: Size = &[];

/// Executes one job per entry of `sizes`, each with the `platform` properties and its
/// size, and returns the open streams: the jobs stay held while the test keeps them.
async fn hold(
    cell: &Cell,
    name: &str,
    platform: &[(&str, &str)],
    sizes: &[Size],
) -> Vec<Streaming<Operation>> {
    let mut streams = Vec::new();
    for (i, size) in sizes.iter().enumerate() {
        let properties: Vec<(&str, &str)> = platform.iter().chain(*size).copied().collect();
        let job = Job::new(&format!("{name} {i}"), &properties);
        cell.upload(&job.blobs()).await;
        streams.push(cell.execute(&job.action).await);
    }
    streams
}

/// The `Start`s `daemon` gets until none comes for [`QUIET`]: how many leases the server
/// placed on it, counted exactly. Each lease's offer is taken with its `Start`, so a later
/// `no_work` sees only new work.
async fn starts(daemon: &mut FakeDaemon) -> Vec<Start> {
    let mut got = Vec::new();
    while let Some(taken) = daemon
        .expect_within(QUIET, |m| match m {
            server_message::Message::Start(s) => Some(Some(s.clone())),
            server_message::Message::LeaseOffer(_) => Some(None),
            _ => None,
        })
        .await
    {
        got.extend(taken);
    }
    got
}

/// `nodes` equal daemons of 4 cores each take `jobs` held one-core actions: each holds
/// `jobs / nodes` leases, give or take one, and the leases are all placed.
async fn equal_nodes_split(nodes: usize, jobs: usize) {
    let cell = Cell::start().await;
    let mut daemons = Vec::new();
    for name in ["node-a", "node-b", "node-c", "node-d"]
        .into_iter()
        .take(nodes)
    {
        daemons.push(cell.daemon(name, 4, 8).await);
    }
    let _held = hold(&cell, "equal", &[], &vec![DEFAULT; jobs]).await;

    let mut counts = Vec::new();
    for daemon in &mut daemons {
        counts.push(starts(daemon).await.len());
    }
    assert_eq!(counts.iter().sum::<usize>(), jobs, "{counts:?}");
    for count in &counts {
        // |count - jobs / nodes| < 1, multiplied out.
        assert!(
            count * nodes + nodes > jobs && count * nodes < jobs + nodes,
            "{jobs} leases on {nodes} equal nodes must give each {jobs}/{nodes} +-1: {counts:?}"
        );
    }
}

/// Catches: first fit in name order, which gave the node named first the four leases
/// its cores held and the other node one (4, 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_equal_nodes_split_five_leases() {
    equal_nodes_split(2, 5).await;
}

/// Catches: first fit in name order (4, 3, 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_equal_nodes_split_seven_leases() {
    equal_nodes_split(3, 7).await;
}

/// Catches: first fit in name order (4, 4, 2, 0), and a spread that stops at two nodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_equal_nodes_split_ten_leases() {
    equal_nodes_split(4, 10).await;
}

/// Catches: first fit (the 2-core node named first takes 2 of 3 leases and the 4-core
/// one 1); a spread by lease count, which ignores the cores (2 and 1 in three leases,
/// 3 and 3 in six, where the 2-core node would be over-booked); and a spread by absolute
/// free cores. A node with half the cores takes half the leases.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_with_half_the_cores_takes_half_the_leases() {
    for (jobs, want_small, want_large) in [(3, 1, 2), (6, 2, 4)] {
        let cell = Cell::start().await;
        let mut small_node = cell.daemon("node-a", 2, 8).await;
        let mut large_node = cell.daemon("node-b", 4, 8).await;
        let _held = hold(&cell, "weighted", &[], &vec![DEFAULT; jobs]).await;

        let on_small = starts(&mut small_node).await.len();
        let on_large = starts(&mut large_node).await.len();
        assert_eq!(
            (on_small, on_large),
            (want_small, want_large),
            "{jobs} leases on a 2-core node named first and a 4-core one"
        );
    }
}

/// Catches: a spread that looks at CPUs only. Two nodes of 8 cores and 16 GiB: the first
/// action books 4 cores and 12 GiB (the node named first), the second 4 cores and 1 GiB
/// (the other node, where nothing is booked), and the third, with both nodes at half
/// their cores, goes to the node with the memory free. First fit puts the second on the
/// first node as well; a CPU-only spread puts the third there (a tie in cores, then the
/// name), where 12 of 16 GiB are booked and a container runs out of memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_with_less_memory_free_gets_fewer_leases() {
    const BIG: Size = &[("kbf-book-cpus", "4"), ("kbf-book-mem-gib", "12")];
    const LIGHT: Size = &[("kbf-book-cpus", "4"), ("kbf-book-mem-gib", "1")];
    let cell = Cell::start().await;
    let mut a = cell.daemon("node-a", 8, 16).await;
    let mut b = cell.daemon("node-b", 8, 16).await;

    let _first = hold(&cell, "first", &[], &[BIG]).await;
    assert_eq!(starts(&mut a).await.len(), 1);
    assert_eq!(starts(&mut b).await.len(), 0);

    let _second = hold(&cell, "second", &[], &[LIGHT]).await;
    assert_eq!(
        starts(&mut b).await.len(),
        1,
        "the second lease goes to the node with nothing booked"
    );
    assert_eq!(starts(&mut a).await.len(), 0);

    let _third = hold(&cell, "third", &[], &[DEFAULT]).await;
    assert_eq!(
        starts(&mut b).await.len(),
        1,
        "the third lease goes to the node with memory free"
    );
    assert_eq!(
        starts(&mut a).await.len(),
        0,
        "no lease goes to the node with 12 of 16 GiB booked"
    );
}

/// Catches: a platform filter applied after the spread. The Mac is named first and holds
/// nothing, so it is the least booked of the three; Linux work still goes to the two
/// Linux nodes, which split it, and the Mac's own work goes to the Mac and no Linux node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_platform_is_matched_before_the_spread() {
    let cell = Cell::start().await;
    let mut mac = FakeDaemon::connect(cell.worker_addr, hello_on("node-a", 4, 8, MAC))
        .await
        .expect("registered");
    let mut linux_b = cell.daemon("node-b", 4, 8).await;
    let mut linux_c = cell.daemon("node-c", 4, 8).await;

    let _linux = hold(&cell, "linux", &[("OSFamily", "linux")], &[DEFAULT; 4]).await;
    assert_eq!(starts(&mut linux_b).await.len(), 2);
    assert_eq!(starts(&mut linux_c).await.len(), 2);
    mac.no_work().await;

    let darwin = [("OSFamily", "Darwin"), ("ISA", "arm-a64")];
    let _mac_job = hold(&cell, "mac", &darwin, &[DEFAULT]).await;
    assert_eq!(starts(&mut mac).await.len(), 1);
    linux_b.no_work().await;
    linux_c.no_work().await;
}
