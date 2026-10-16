//! The wire bytes of the xDS responses, against files: the responses Envoy would receive
//! for a leader and for none. A golden is hex of the encoded `DiscoveryResponse`; it was
//! written by an encoder of its own, not by this crate's code, so a wrong field number
//! or type in the response fails here and not at the first Envoy that is given one.

use envoy_types::pb::envoy::config::endpoint::v3::ClusterLoadAssignment;
use envoy_types::pb::envoy::service::discovery::v3::DiscoveryResponse;
use kbf_gateway::ads::{CLUSTER, response};
use kbf_gateway::registry::Leader;
use prost::{Message as _, Name as _};

fn golden(hex_text: &str) -> Vec<u8> {
    hex::decode(hex_text.trim()).expect("hex")
}

fn leader() -> Leader {
    Leader {
        name: "kbf1.farm.test".to_owned(),
        address: "192.0.2.10:8980".parse().expect("address"),
        term: 7,
    }
}

/// Catches: any change to the bytes Envoy receives for a leader.
#[test]
fn the_response_for_a_leader_matches_its_golden() {
    let bytes = response(Some(&leader()), 3).encode_to_vec();
    assert_eq!(
        bytes,
        golden(include_str!("golden/response_leader.hex")),
        "got {}",
        hex::encode(&bytes)
    );
}

/// Catches: the same, for no leader: an empty assignment, not a missing resource.
#[test]
fn the_response_for_no_leader_matches_its_golden() {
    let bytes = response(None, 1).encode_to_vec();
    assert_eq!(
        bytes,
        golden(include_str!("golden/response_none.hex")),
        "got {}",
        hex::encode(&bytes)
    );
}

/// Catches: a golden that decodes to something other than what its name says: the leader
/// golden must hold the endpoint of the leader, in the resource Envoy's EDS subscription
/// reads.
#[test]
fn the_golden_says_what_it_should() {
    let decoded =
        DiscoveryResponse::decode(golden(include_str!("golden/response_leader.hex")).as_slice())
            .expect("decode");
    assert_eq!(decoded.version_info, "7:kbf1.farm.test@192.0.2.10:8980");
    assert_eq!(decoded.nonce, "3");
    assert_eq!(decoded.type_url, ClusterLoadAssignment::type_url());
    let [resource] = decoded.resources.as_slice() else {
        panic!("one resource");
    };
    assert_eq!(resource.type_url, ClusterLoadAssignment::type_url());
    let assignment = ClusterLoadAssignment::decode(resource.value.as_slice()).expect("decode");
    assert_eq!(assignment.cluster_name, CLUSTER);
    let text = format!("{assignment:?}");
    assert!(text.contains("\"192.0.2.10\""), "{text}");
    assert!(text.contains("PortValue(8980)"), "{text}");
    assert!(text.contains("\"kbf1.farm.test\""), "{text}");
}
