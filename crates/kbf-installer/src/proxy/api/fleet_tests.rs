//! The three answers of the active child, as the gate reads them. Each test is an answer
//! as the server writes it (`docs/api.md`) and the snapshot it must become.

use std::collections::BTreeSet;

use hyper::StatusCode;
use serde_json::{Value, json};

use super::fleet::{Enrolled, Population, Unreadable};

fn bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}

fn names(of: &[&str]) -> BTreeSet<String> {
    of.iter().map(|s| (*s).to_owned()).collect()
}

fn node(id: &str, protocol: Value) -> Value {
    json!({ "node_id": id, "connected": true, "protocol": protocol, "software": null,
            "needs_attention": [], "placement": { "state": "serving" } })
}

fn member(id: u64, addr: &str, protocol: Value) -> Value {
    json!({ "id": id, "addr": addr, "state": "active", "weight": 1, "protocol": protocol })
}

fn registered(id: &str, roles: &[&str], state: &str, advertise: &str) -> Value {
    json!({ "id": id, "roles": roles, "state": state, "advertise": advertise, "zone": "",
            "token_id": "t", "token_label": "", "created_at": 1, "last_renewed_at": null,
            "not_after": 2, "revoked_at": null })
}

fn nodes_envelope() -> Value {
    json!({ "server": { "version": "0.1.0+0123456789ab", "commit": "0123456789ab" },
            "nodes": [node("mac-1", json!(2)), node("old-1", Value::Null)],
            "oldest_protocol": 2, "protocol_unknown": 1 })
}

fn members_envelope() -> Value {
    json!({ "generation": 7,
            "members": [member(1, "Store-1.Example:8982", json!(3)),
                        member(2, "192.0.2.12:8982", Value::Null)],
            "oldest_protocol": 3, "protocol_unknown": 1 })
}

#[test]
fn the_daemons_view_gives_the_ids_and_the_envelopes_two_numbers() {
    let got = Population::from_nodes(StatusCode::OK, &bytes(&nodes_envelope())).unwrap();
    assert_eq!(
        got,
        Population {
            names: names(&["mac-1", "old-1"]),
            oldest: Some(2),
            unknown: 1,
        }
    );
}

#[test]
fn the_members_view_gives_the_addresses_in_lower_case_and_the_envelopes_two_numbers() {
    let got = Population::from_members(StatusCode::OK, &bytes(&members_envelope())).unwrap();
    assert_eq!(
        got,
        Population {
            names: names(&["store-1.example:8982", "192.0.2.12:8982"]),
            oldest: Some(3),
            unknown: 1,
        }
    );
}

#[test]
fn a_null_oldest_protocol_is_no_protocol_named_and_is_not_unreadable() {
    let empty = json!({ "nodes": [], "oldest_protocol": null, "protocol_unknown": 0 });
    let got = Population::from_nodes(StatusCode::OK, &bytes(&empty)).unwrap();
    assert_eq!(got, Population::default());
    let empty = json!({ "generation": 0, "members": [], "oldest_protocol": null,
                        "protocol_unknown": 0 });
    let got = Population::from_members(StatusCode::OK, &bytes(&empty)).unwrap();
    assert_eq!(got, Population::default());
}

#[test]
fn a_view_without_the_protocol_fields_is_unreadable_not_empty() {
    // A child from before the fields: its envelope has the list and nothing about protocols.
    let neither_nodes = json!({ "server": {}, "nodes": [node("old-1", Value::Null)] });
    let neither_members = json!({ "generation": 1, "members": [member(1, "a:1", Value::Null)] });
    // A body with one of the two is as unusable as one with neither.
    let only_unknown = json!({ "nodes": [], "members": [], "protocol_unknown": 0 });
    let only_oldest = json!({ "nodes": [], "members": [], "oldest_protocol": 1 });
    // Every body that is read anyway, so one failure shows all of them.
    let mut read = Vec::new();
    for body in [neither_nodes, only_unknown.clone(), only_oldest.clone()] {
        if Population::from_nodes(StatusCode::OK, &bytes(&body)).is_ok() {
            read.push(format!("nodes {body}"));
        }
    }
    for body in [neither_members, only_unknown, only_oldest] {
        if Population::from_members(StatusCode::OK, &bytes(&body)).is_ok() {
            read.push(format!("members {body}"));
        }
    }
    assert!(
        read.is_empty(),
        "read without the protocol fields: {read:?}"
    );
}

#[test]
fn an_answer_that_is_not_a_200_or_not_json_is_unreadable_and_says_which_request() {
    let body = bytes(&nodes_envelope());
    for status in [
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::NOT_FOUND,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let Unreadable(why) = Population::from_nodes(status, &body).unwrap_err();
        assert_eq!(why, format!("GET /v1/nodes answered {status}"));
        let Unreadable(why) = Population::from_members(status, &body).unwrap_err();
        assert_eq!(why, format!("GET /v1/members answered {status}"));
    }
    let Unreadable(why) = Population::from_nodes(StatusCode::OK, b"<html>").unwrap_err();
    assert!(why.starts_with("GET /v1/nodes: "), "{why}");
    let Unreadable(why) = Population::from_members(StatusCode::OK, b"").unwrap_err();
    assert!(why.starts_with("GET /v1/members: "), "{why}");
}

#[test]
fn the_registry_gives_the_active_daemons_by_id_and_the_active_storage_by_address() {
    let body = json!({ "nodes": [
        registered("d1", &["daemon"], "active", ""),
        registered("s1", &["storage"], "active", "Store-1.Example:8982"),
        registered("b1", &["storage", "daemon"], "active", "192.0.2.9:8982"),
        registered("r1", &["daemon"], "revoked", ""),
        registered("r2", &["storage"], "revoked", "gone.example:8982"),
        // A state this build does not know is still enrolled: the gate errs towards refusing.
        registered("w1", &["daemon"], "suspended", ""),
        registered("x1", &["gateway"], "active", ""),
    ] });
    let got = Enrolled::from_registry(StatusCode::OK, &bytes(&body)).unwrap();
    assert_eq!(
        got,
        Enrolled {
            daemons: names(&["d1", "b1", "w1"]),
            storage: names(&["store-1.example:8982", "192.0.2.9:8982"]),
        }
    );
}

#[test]
fn a_child_without_enrollment_enrolls_no_one() {
    let got = Enrolled::from_registry(StatusCode::NOT_FOUND, b"{\"error\":\"not enabled\"}");
    assert_eq!(got, Ok(Enrolled::default()));
}

#[test]
fn a_registry_that_cannot_be_read_is_unreadable() {
    let ok = bytes(&json!({ "nodes": [] }));
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let Unreadable(why) = Enrolled::from_registry(status, &ok).unwrap_err();
        assert_eq!(why, format!("GET /v1/enroll/nodes answered {status}"));
    }
    for body in [b"nope".to_vec(), bytes(&json!({ "tokens": [] }))] {
        let got = Enrolled::from_registry(StatusCode::OK, &body);
        assert!(got.is_err(), "{got:?}");
    }
}
