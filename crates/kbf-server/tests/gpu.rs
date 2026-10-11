//! GPU reservation end to end: a `gpu` platform property, a node report's `gpu` entry,
//! and placement that holds a GPU for one lease at a time.

mod support;

use kbf_proto::worker::{Capability, Hello};
use kbf_server::worker::capacity;
use kbf_types::Resources;
use support::{FakeDaemon, Job, done, hello, output, ran};

const GIB: u64 = 1 << 30;

/// A Hello for `node` with 4 CPUs, 8 GiB and `gpus` GPUs.
fn hello_with_gpus(node: &str, gpus: &str) -> Hello {
    let mut hello = hello(node, 4, 8);
    hello.capabilities.push(Capability {
        key: "gpu".to_owned(),
        value: gpus.to_owned(),
    });
    hello
}

/// Catches: a node report's `gpu` entry dropped (no node would ever take GPU work), a
/// report without one refused (every daemon that does not detect GPUs would be shut
/// out), and a repeated or garbled `gpu` entry taken as some count.
#[test]
fn capacity_reads_the_gpu_entry() {
    assert_eq!(
        capacity(&hello_with_gpus("g", "2")),
        Ok(Resources::new(4_000, 8 * GIB).with_gpus(2))
    );
    assert_eq!(
        capacity(&hello("c", 4, 8)),
        Ok(Resources::new(4_000, 8 * GIB))
    );
    let mut twice = hello_with_gpus("g", "1");
    twice.capabilities.push(Capability {
        key: "gpu".to_owned(),
        value: "1".to_owned(),
    });
    for bad in [twice, hello_with_gpus("g", "one")] {
        let refused = capacity(&bad).expect_err("refused");
        assert!(refused.contains("gpu"), "{refused}");
    }
}

/// Catches: a `gpu=1` action placed on a node without a GPU; a second `gpu=1` action
/// placed on a one-GPU node whose GPU a running lease holds; a GPU kept booked after
/// its lease ends, so the queued action never runs; and CPU-only work held back by a
/// booked GPU.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_serves_one_lease_at_a_time() {
    let cell = support::Cell::start().await;
    // Name order puts the CPU-only node first: first fit would pick it.
    let mut cpu_only = cell.daemon("node-a", 4, 8).await;
    let mut gpu_node = FakeDaemon::connect(cell.worker_addr, hello_with_gpus("node-g", "1"))
        .await
        .expect("registered");

    let first = Job::new("train one", &[("gpu", "1")]);
    let second = Job::new("train two", &[("gpu", "1")]);
    let plain = Job::new("compile", &[]);
    for job in [&first, &second, &plain] {
        cell.upload(&job.blobs()).await;
    }

    let mut first_ops = cell.execute(&first.action).await;
    let first_start = gpu_node.start().await;
    assert_eq!(
        first_start.action_digest.as_ref(),
        Some(&first.action.proto)
    );

    let mut second_ops = cell.execute(&second.action).await;
    gpu_node.no_work().await;
    cpu_only.no_work().await;

    // CPU-only work still runs while the GPU is held.
    let mut plain_ops = cell.execute(&plain.action).await;
    let plain_start = cpu_only.start().await;
    assert_eq!(
        plain_start.action_digest.as_ref(),
        Some(&plain.action.proto)
    );
    let built = output(&cell, "compiled", 0).await;
    assert!(
        cpu_only
            .report(ran(plain_start.lease_id, &built))
            .await
            .accepted
    );
    done(&mut plain_ops).await;

    // The first lease ends: the GPU is free, and the queued action takes it.
    let trained = output(&cell, "trained once", 0).await;
    assert!(
        gpu_node
            .report(ran(first_start.lease_id, &trained))
            .await
            .accepted
    );
    done(&mut first_ops).await;
    let second_start = gpu_node.start().await;
    assert_eq!(
        second_start.action_digest.as_ref(),
        Some(&second.action.proto)
    );
    let trained = output(&cell, "trained twice", 0).await;
    assert!(
        gpu_node
            .report(ran(second_start.lease_id, &trained))
            .await
            .accepted
    );
    done(&mut second_ops).await;
    cpu_only.no_work().await;
}

/// The Mac of a GPU test: 12 CPUs, 32 GiB, the native and whole-machine drivers, and no
/// `gpu` entry (a Mac reports none).
fn mac() -> Hello {
    support::hello_on(
        "mac-1",
        12,
        32,
        &[
            ("arch", "arm64"),
            ("os", "macos"),
            ("drivers", "native"),
            ("drivers", "native-whole-machine"),
        ],
    )
}

/// Catches a GPU test on a Mac that cannot be placed (it books a GPU the Mac does not
/// report), that books less than the whole Mac, or that lets other work run beside it
/// (`docs/design/macos-vms.md` section 8.1: the GPU and the unified memory are the
/// test's alone). It reaches the daemon as a `whole_machine` lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_test_on_a_mac_books_the_whole_mac() {
    let cell = support::Cell::start().await;
    let mut mac = FakeDaemon::connect(cell.worker_addr, mac())
        .await
        .expect("registered");
    let gpu = Job::new(
        "drive the app with a local model",
        &[
            ("OSFamily", "darwin"),
            ("kbf-lease", "whole_machine"),
            ("gpu", "1"),
        ],
    );
    let beside = Job::new("a small Mac action", &[("OSFamily", "darwin")]);
    for job in [&gpu, &beside] {
        cell.upload(&job.blobs()).await;
    }
    let mut ops = cell.execute(&gpu.action).await;
    let start = mac.start().await;
    assert_eq!(start.kind, "whole_machine");
    assert_eq!(start.action_digest.as_ref(), Some(&gpu.action.proto));
    assert_eq!((start.millicpus, start.memory_bytes), (12_000, 32 * GIB));

    let mut beside_ops = cell.execute(&beside.action).await;
    mac.no_work().await;
    let result = output(&cell, "the model answered", 0).await;
    assert!(mac.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;
    let next = mac.start().await;
    assert_eq!(next.kind, "action");
    let small = output(&cell, "ran after", 0).await;
    assert!(mac.report(ran(next.lease_id, &small)).await.accepted);
    done(&mut beside_ops).await;
}

/// Catches a GPU asked for on macOS outside a whole-machine lease that is taken: it
/// would wait for a GPU no Mac reports (or, if one did, share the Mac with other
/// work). It is refused at once, saying what to add. A GPU asked for with no OS named
/// is not a Mac's, and is booked as before (see `a_gpu_serves_one_lease_at_a_time`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_on_macos_outside_a_whole_machine_lease_is_refused() {
    let cell = support::Cell::start().await;
    let job = Job::new("a GPU action lease", &[("OSFamily", "macos"), ("gpu", "1")]);
    cell.upload(&job.blobs()).await;
    let refused = cell
        .execute_with(&job.action, false)
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(
        refused.message().contains("kbf-lease=whole_machine"),
        "{refused:?}"
    );
}
