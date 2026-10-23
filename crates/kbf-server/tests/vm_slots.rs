//! A node's `vm.slots` entry as the VM slots placement may book, and the share of the
//! host that a node reporting slots keeps back. Each test names the defect it catches.

#[path = "support/api_http.rs"]
mod api_http;
mod support;

use api_http::get;
use kbf_proto::worker::{Capability, Hello};
use kbf_server::HostFloor;
use kbf_server::worker::capacity;
use kbf_types::Resources;
use serde_json::json;
use support::{Cell, FakeDaemon, hello};

const GIB: u64 = 1 << 30;

/// A Hello for `node` with 10 CPUs and 32 GiB, reporting `slots` VM slots (none if
/// `None`).
fn hello_with_slots(node: &str, slots: Option<&str>) -> Hello {
    let mut hello = hello(node, 10, 32);
    if let Some(slots) = slots {
        hello.capabilities.push(Capability {
            key: "vm.slots".to_owned(),
            value: slots.to_owned(),
        });
    }
    hello
}

/// Catches: `vm.slots` not read (no Mac would ever hold a VM), five slots booked as five
/// (Apple allows two macOS guests on a Mac), a report without the entry given slots, a
/// zero given slots or the floor (a node that cannot run a VM gives up nothing), and a
/// repeated or garbled entry taken as some count.
#[test]
fn capacity_reads_vm_slots_and_clamps_them_to_two() {
    let floor = HostFloor::default();
    let host_share = Resources::new(8_000, 24 * GIB);
    assert_eq!(
        capacity(&hello_with_slots("m", Some("5")), floor),
        Ok(host_share.with_vms(2))
    );
    assert_eq!(
        capacity(&hello_with_slots("m", Some("2")), floor),
        Ok(host_share.with_vms(2))
    );
    assert_eq!(
        capacity(&hello_with_slots("m", Some("1")), floor),
        Ok(host_share.with_vms(1))
    );
    let whole = Resources::new(10_000, 32 * GIB);
    assert_eq!(capacity(&hello_with_slots("m", None), floor), Ok(whole));
    assert_eq!(
        capacity(&hello_with_slots("m", Some("0")), floor),
        Ok(whole)
    );
    let mut twice = hello_with_slots("m", Some("2"));
    twice.capabilities.push(Capability {
        key: "vm.slots".to_owned(),
        value: "1".to_owned(),
    });
    for bad in [twice, hello_with_slots("m", Some("two"))] {
        let refused = capacity(&bad, floor).expect_err("refused");
        assert!(refused.contains("vm.slots"), "{refused}");
    }
}

/// Catches: a floor that is not the configured one (the defaults used whatever was set),
/// one subtracted from CPUs only or memory only, one that can wrap a small host below
/// zero into a huge capacity, and slots lost when the floor takes everything.
#[test]
fn the_host_floor_is_subtracted_from_cpus_and_memory() {
    let hello = hello_with_slots("m", Some("2"));
    assert_eq!(
        capacity(&hello, HostFloor::new(4, 16)),
        Ok(Resources::new(6_000, 16 * GIB).with_vms(2))
    );
    assert_eq!(
        capacity(&hello, HostFloor::new(0, 0)),
        Ok(Resources::new(10_000, 32 * GIB).with_vms(2))
    );
    assert_eq!(
        capacity(&hello, HostFloor::new(64, 256)),
        Ok(Resources::new(0, 0).with_vms(2))
    );
    assert_eq!(HostFloor::default(), HostFloor::new(2, 8));
}

/// Catches: `--vm-host-floor-cpus` and `--vm-host-floor-mem-gib` not reaching the
/// listeners, a default other than 2 cores and 8 GiB, and a word accepted as a number.
#[test]
fn the_host_floor_comes_from_its_flags() {
    use clap::Parser;
    let listeners = |args: &[&str]| {
        kbf_server::Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
            .map(|args| args.listeners().expect("listeners"))
    };
    assert_eq!(
        listeners(&[]).expect("parses").vm_host_floor,
        HostFloor::new(2, 8)
    );
    let set = listeners(&["--vm-host-floor-cpus", "3", "--vm-host-floor-mem-gib=12"])
        .expect("parses")
        .vm_host_floor;
    assert_eq!(set, HostFloor::new(3, 12));
    assert!(listeners(&["--vm-host-floor-cpus", "two"]).is_err());
    assert!(listeners(&["--vm-host-floor-mem-gib", "-1"]).is_err());
}

/// Catches: a server that adds a node's reported capacity to what it already holds when
/// the same node name registers again (a daemon restarted over its predecessor's stream),
/// so the node would hold four VM slots and twice the floored CPUs and memory; and a node
/// whose `vm.slots` and floor do not reach the operator API's capacity. It proves one node
/// name only: the server has no host identity, so two names on one host are not detected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_registering_a_node_replaces_its_vm_slots_and_floor() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let _first = FakeDaemon::connect(cell.worker_addr, hello_with_slots("mac-1", Some("2")))
        .await
        .expect("registered");
    let _second = FakeDaemon::connect(cell.worker_addr, hello_with_slots("mac-1", Some("2")))
        .await
        .expect("registered");
    let (code, node) = get(api, "/v1/nodes/mac-1").await;
    assert_eq!(code, 200, "{node}");
    assert_eq!(
        node["capacity"],
        json!({"cpu_millis": 8_000, "memory_bytes": 24 * GIB, "gpus": 0, "vms": 2})
    );
}
