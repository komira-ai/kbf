//! The protocol version each daemon speaks, as `GET /v1/nodes` lists it, and the oldest of
//! them (`docs/design/frontend-proxy.md` section 9, rule R1). Every protocol constant is 1
//! today and the worker service refuses any other `Hello`, so a test registers its nodes at
//! the farm with versions of its own, as a fleet of different builds would: the value a
//! test puts in can then differ from the constants and from the other nodes'.

mod support;

use std::sync::Arc;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::worker::{Hello, ServerMessage};
use kbf_sched::DaemonInstance;
use kbf_server::farm::{Reported, StreamId};
use kbf_server::worker::reported;
use kbf_server::{Farm, fleet::NodeView};
use kbf_types::{Resources, WorkerId};
use serde_json::{Value, json};
use support::hello;
use tokio::sync::mpsc;

fn new_farm() -> Farm {
    Farm::new(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
}

/// What a node's `Hello` reports when it names `protocol`.
fn speaking(protocol: Option<u32>) -> Reported {
    let caps = NodeCaps::from_report([("arch", "x86_64")])
        .expect("caps")
        .with_drivers(["container"]);
    Reported {
        protocol,
        ..Reported::serving(Resources::new(8_000, 16 << 30), caps)
    }
}

/// The receiving end of a node's stream: dropping it ends the stream.
type Responses = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// Registers `node` on a new stream whose first `Hello` named `protocol`.
fn join(farm: &Farm, node: &str, protocol: Option<u32>) -> (StreamId, Responses) {
    let (outbound, responses) = mpsc::unbounded_channel();
    let stream = farm.register(
        &WorkerId::new(node),
        DaemonInstance::new(node),
        // No predecessor named: the first daemon of the node.
        DaemonInstance::default(),
        speaking(protocol),
        outbound,
        ServerMessage::default(),
    );
    (stream, responses)
}

/// The body of `GET /v1/nodes`, as the farm gives it to the API.
fn nodes_body(farm: &Farm) -> Value {
    serde_json::to_value(farm.nodes()).expect("JSON")
}

/// What each node's entry says of its protocol, in node-id order.
fn listed(body: &Value) -> Value {
    let nodes = body["nodes"].as_array().expect("a list");
    Value::Array(nodes.iter().map(|n| n["protocol"].clone()).collect())
}

/// Catches: a `Hello`'s version dropped on the way into the report (a node would always
/// read as unknown), changed (a constant stands in for it), and a field the sender never
/// set (wire 0) read as a node that speaks version 0.
#[test]
fn a_hellos_protocol_version_is_what_the_node_is_reported_speaking() {
    for (wire, named) in [
        (1, Some(1)),
        (7, Some(7)),
        (u32::MAX, Some(u32::MAX)),
        (0, None),
    ] {
        let hello = Hello {
            protocol_version: wire,
            ..hello("n", 8, 16)
        };
        let got = reported(&hello).expect("a valid report");
        assert_eq!(got.protocol, named, "wire {wire}");
    }
}

/// Catches: the envelope's oldest taken as the greatest version (`max`), as the first or
/// the last node's, or not at all; each node's own entry carrying another node's version;
/// and a summary nested under a key of its own, where the gate that reads
/// `oldest_protocol` and `protocol_unknown` off the body would not find it. The least
/// version is the middle node's.
#[tokio::test]
async fn the_envelope_names_the_least_protocol_among_the_nodes() {
    let farm = new_farm();
    let _a = join(&farm, "a", Some(3));
    let _b = join(&farm, "b", Some(2));
    let _c = join(&farm, "c", Some(5));
    let body = nodes_body(&farm);
    assert_eq!(listed(&body), json!([3, 2, 5]));
    assert_eq!(body["oldest_protocol"], json!(2), "{body}");
    assert_eq!(body["protocol_unknown"], json!(0), "{body}");
}

/// Catches: a node that names no version folded into the oldest (as 0, or as the least
/// there is), left uncounted, or counted as known; and a list with no version known
/// answering an oldest of 0 where it has none (a gate that reads the oldest would take 0
/// for a version a node speaks, and `null` for "no node to ask").
#[tokio::test]
async fn a_node_naming_no_protocol_is_counted_and_never_the_oldest() {
    let farm = new_farm();
    let empty = nodes_body(&farm);
    assert_eq!(empty["oldest_protocol"], Value::Null, "{empty}");
    assert_eq!(empty["protocol_unknown"], json!(0), "{empty}");
    let _a = join(&farm, "a", Some(3));
    let _b = join(&farm, "b", None);
    let _c = join(&farm, "c", None);
    let body = nodes_body(&farm);
    assert_eq!(listed(&body), json!([3, null, null]));
    assert_eq!(body["oldest_protocol"], json!(3), "{body}");
    assert_eq!(body["protocol_unknown"], json!(2), "{body}");

    let only_unknown = new_farm();
    let _x = join(&only_unknown, "x", None);
    let body = nodes_body(&only_unknown);
    assert_eq!(body["oldest_protocol"], Value::Null, "{body}");
    assert_eq!(body["protocol_unknown"], json!(1), "{body}");
}

/// Catches: a summary over the connected nodes only. A node whose stream ended is still
/// listed with the version it last spoke, and it counts: it may come back on that
/// version, and a release that stopped accepting it would then lock it out.
#[tokio::test]
async fn a_node_whose_stream_ended_still_counts_with_the_version_it_last_spoke() {
    let farm = new_farm();
    let (_, gone) = join(&farm, "a", Some(2));
    let _b = join(&farm, "b", Some(4));
    drop(gone);
    let body = nodes_body(&farm);
    assert_eq!(body["nodes"][0]["connected"], json!(false), "{body}");
    assert_eq!(listed(&body), json!([2, 4]));
    assert_eq!(body["oldest_protocol"], json!(2), "{body}");
}

/// Catches: a node that returns on another version still listed with its first (the
/// version is the newest stream's, not the first ever seen), one that returns naming none
/// keeping the earlier version (a node of unknown version would pass as a known one), and
/// a `Hello` resent on the stream changing the version (only the first one is negotiated:
/// `Welcome` answered it).
#[tokio::test]
async fn the_version_is_the_newest_streams_first_hello() {
    let farm = new_farm();
    let node = WorkerId::new("a");
    let version = |farm: &Farm| -> Option<u32> {
        let view: NodeView = farm.node_view(&node).expect("listed");
        view.protocol
    };
    let (_, _first) = join(&farm, "a", Some(1));
    assert_eq!(version(&farm), Some(1));
    let (second, _second) = join(&farm, "a", Some(3));
    assert_eq!(version(&farm), Some(3), "the newer stream's");
    assert_eq!(nodes_body(&farm)["oldest_protocol"], json!(3));

    farm.resize(&node, second, speaking(Some(9)));
    assert_eq!(version(&farm), Some(3), "a resent Hello is not negotiated");

    let (_, _third) = join(&farm, "a", None);
    assert_eq!(version(&farm), None, "not the earlier stream's");
    assert_eq!(nodes_body(&farm)["protocol_unknown"], json!(1));
}
