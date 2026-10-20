//! `GET /v1/nodes/{id}` and the `lease_count` and `booked` of `GET /v1/nodes`, over a
//! real listener and real daemons (`docs/design/kbf-ui-v1.md`, N5). Each test names the
//! defect it catches.

#[path = "support/api_http.rs"]
mod api_http;
mod support;

use api_http::{get, http};
use serde_json::json;
use support::{Cell, Job};

const GIB: u64 = 1 << 30;

/// Catches (N5): a detail that is not the list entry (a field the list has missing or
/// different), a capacity that is not what the daemon's `Hello` reported or is the
/// booking, a booking that does not follow a lease, `free`
/// that is not capacity less booked on every axis, a lease list that is not the node's
/// own, and `lease_count` and `booked` on the list that disagree with the detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_answers_its_entry_capacity_booking_and_leases() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let mut busy = cell.daemon("linux-1", 8, 16).await;
    let _idle = cell.daemon("linux-2", 4, 8).await;

    // Nothing leased yet: nothing booked, no lease, everything free.
    let (code, idle) = get(api, "/v1/nodes/linux-1").await;
    assert_eq!(code, 200, "{idle}");
    let capacity = json!({"cpu_millis": 8_000, "memory_bytes": 16 * GIB, "gpus": 0, "vms": 0});
    let nothing = json!({"cpu_millis": 0, "memory_bytes": 0, "gpus": 0, "vms": 0});
    assert_eq!(idle["capacity"], capacity);
    assert_eq!(idle["booked"], nothing);
    assert_eq!(idle["free"], capacity);
    assert_eq!(idle["leases"], json!([]));
    assert_eq!(idle["lease_count"], 0);

    let job = Job::new("compile", &[]);
    cell.upload(&job.blobs()).await;
    let _operation = cell.execute(&job.action).await;
    let lease = busy.start().await.lease_id.expect("a lease");
    let lease_name = format!("{}.{}", lease.term, lease.seq);

    let (_, leased) = get(api, "/v1/nodes/linux-1").await;
    assert_eq!(leased["leases"], json!([lease_name]));
    assert_eq!(leased["lease_count"], 1);
    let booked = &leased["booked"];
    let (cpu, memory) = (
        booked["cpu_millis"].as_u64().expect("cpu"),
        booked["memory_bytes"].as_u64().expect("memory"),
    );
    assert!(cpu > 0 && memory > 0, "a lease books something: {booked}");
    assert_eq!(
        leased["capacity"], capacity,
        "booking does not change capacity"
    );
    assert_eq!(
        leased["free"],
        json!({
            "cpu_millis": 8_000 - cpu,
            "memory_bytes": 16 * GIB - memory,
            "gpus": 0,
            "vms": 0,
        })
    );

    // The list entry carries `lease_count` and `booked`, and every field of the detail
    // that the list has.
    let (_, list) = get(api, "/v1/nodes").await;
    let entry = list["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["node_id"] == "linux-1")
        .expect("listed");
    assert_eq!(
        (&entry["lease_count"], &entry["booked"]),
        (&json!(1), booked)
    );
    let mut without = leased.clone();
    for key in ["capacity", "free", "leases"] {
        without.as_object_mut().expect("an object").remove(key);
    }
    assert_eq!(&without, entry, "the detail is the list entry and more");
    let other = list["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["node_id"] == "linux-2")
        .expect("listed");
    assert_eq!(
        (&other["lease_count"], &other["booked"]),
        (&json!(0), &nothing),
        "another node's lease is not this one's"
    );
    let (_, second) = get(api, "/v1/nodes/linux-2").await;
    assert_eq!(second["leases"], json!([]));
    assert_eq!(second["capacity"]["cpu_millis"], 4_000);
}

/// Catches: a read of a write's path answered as a node (or as the write, or as a
/// `405`): `GET /v1/nodes/{id}:cordon` and the other two verbs are `404` whether or not
/// the node exists, and so is a node that never registered and a path below the node;
/// the `404` names `no_node`; and an id that merely holds a colon is read as itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_id_with_a_write_suffix_or_a_path_below_it_is_404() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let _node = cell.daemon("linux-1", 8, 16).await;
    let _colon = cell.daemon("host:8080", 8, 16).await;
    for path in [
        "/v1/nodes/linux-1:cordon",
        "/v1/nodes/linux-1:drain",
        "/v1/nodes/linux-1:uncordon",
        "/v1/nodes/host%3A8080:cordon",
        "/v1/nodes/ghost:cordon",
        "/v1/nodes/ghost",
    ] {
        let (code, body) = get(api, path).await;
        assert_eq!(code, 404, "{path}: {body}");
        assert_eq!(body, json!({"error": "no_node"}), "{path}");
    }
    let (code, _, _) = http(api, "GET", "/v1/nodes/linux-1/leases", "").await;
    assert_eq!(code, 404, "a path below the node");
    // A node is still cordoned by its write, which the read must not have touched.
    let (_, entry) = get(api, "/v1/nodes/linux-1").await;
    assert_eq!(entry["placement"]["state"], "serving");
    // An id with a colon that is no verb, percent-encoded as `node_path` writes it.
    let (code, colon) = get(api, "/v1/nodes/host%3A8080").await;
    assert_eq!(code, 200, "{colon}");
    assert_eq!(colon["node_id"], "host:8080");
}
