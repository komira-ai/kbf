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
    // Name order puts the CPU-only node first, and nothing is booked on it: a placement
    // that ignored the GPU would pick it.
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

/// The Mac of a GPU test: 12 CPUs, 32 GiB, the native and whole-machine drivers, and the
/// GPU entries a Mac reports: one `gpu` slot, of a native GPU with 80 cores and Metal
/// family 9.
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
            ("gpu", "1"),
            ("gpu.kind", "native"),
            ("gpu.cores", "80"),
            ("gpu.metal_family", "9"),
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

/// Catches a GPU action lease on a Mac that cannot be placed (the Mac's GPU slot is
/// not booked, so it is refused or waits for ever), two GPU leases that run on one Mac
/// together (the GPU slot is not held, or is freed late), CPU-only work held back by
/// the booked GPU (the Mac's CPU stays shared), and a GPU slot kept after its lease
/// ends so that the queued GPU lease never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_action_on_a_mac_books_its_one_gpu_slot() {
    let cell = support::Cell::start().await;
    let mut mac = FakeDaemon::connect(cell.worker_addr, mac())
        .await
        .expect("registered");
    let props = [("OSFamily", "darwin"), ("gpu", "1")];
    let first = Job::new("first model test", &props);
    let second = Job::new("second model test", &props);
    let beside = Job::new("a small Mac action", &[("OSFamily", "darwin")]);
    for job in [&first, &second, &beside] {
        cell.upload(&job.blobs()).await;
    }
    let mut first_ops = cell.execute(&first.action).await;
    let first_start = mac.start().await;
    assert_eq!(first_start.kind, "action");
    assert_eq!(
        first_start.action_digest.as_ref(),
        Some(&first.action.proto)
    );

    let mut second_ops = cell.execute(&second.action).await;
    mac.no_work().await;

    // The Mac's CPU is still shared: an action that asks for no GPU runs beside it.
    let mut beside_ops = cell.execute(&beside.action).await;
    let beside_start = mac.start().await;
    assert_eq!(
        beside_start.action_digest.as_ref(),
        Some(&beside.action.proto)
    );
    let small = output(&cell, "ran beside", 0).await;
    assert!(
        mac.report(ran(beside_start.lease_id, &small))
            .await
            .accepted
    );
    done(&mut beside_ops).await;

    let answered = output(&cell, "the first model answered", 0).await;
    assert!(
        mac.report(ran(first_start.lease_id, &answered))
            .await
            .accepted
    );
    done(&mut first_ops).await;
    let second_start = mac.start().await;
    assert_eq!(
        second_start.action_digest.as_ref(),
        Some(&second.action.proto)
    );
    let answered = output(&cell, "the second model answered", 0).await;
    assert!(
        mac.report(ran(second_start.lease_id, &answered))
            .await
            .accepted
    );
    done(&mut second_ops).await;
}

/// Catches more than a Mac's one GPU asked for on macOS taken: it would wait for ever
/// for a node that cannot exist. It is refused at once, saying a Mac has one. A GPU
/// asked for with no OS named is not a Mac's, and is booked as before (see
/// `a_gpu_serves_one_lease_at_a_time`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_gpus_on_macos_are_refused() {
    let cell = support::Cell::start().await;
    let job = Job::new("two GPUs", &[("OSFamily", "macos"), ("gpu", "2")]);
    cell.upload(&job.blobs()).await;
    let refused = cell
        .execute_with(&job.action, false)
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(refused.message().contains("one GPU"), "{refused:?}");
}

/// A Hello for `node` with 4 CPUs, 8 GiB and an `action_slots` entry of `slots`.
fn hello_with_slots(node: &str, slots: &str) -> Hello {
    let mut hello = hello(node, 4, 8);
    hello.capabilities.push(Capability {
        key: "action_slots".to_owned(),
        value: slots.to_owned(),
    });
    hello
}

/// Catches: a node report's `action_slots` entry dropped (a Mac with fewer pool users
/// than cores given more leases than users), a report without one capped, zero taken for
/// no cap, and a repeated or garbled entry taken as some count.
#[test]
fn capacity_reads_the_action_slots_entry() {
    assert_eq!(
        capacity(&hello_with_slots("m", "2")),
        Ok(Resources::new(4_000, 8 * GIB).with_lease_cap(2))
    );
    assert_eq!(
        capacity(&hello_with_slots("m", "0")),
        Ok(Resources::new(4_000, 8 * GIB).with_lease_cap(0))
    );
    assert_eq!(capacity(&hello("c", 4, 8)).unwrap().leases, None);
    let mut twice = hello_with_slots("m", "1");
    twice.capabilities.push(Capability {
        key: "action_slots".to_owned(),
        value: "1".to_owned(),
    });
    for bad in [twice, hello_with_slots("m", "two")] {
        let refused = capacity(&bad).expect_err("refused");
        assert!(refused.contains("action_slots"), "{refused}");
    }
}
