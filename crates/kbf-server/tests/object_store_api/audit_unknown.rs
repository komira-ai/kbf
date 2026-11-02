//! The audit answer for a row written by a newer build.

use kbf_state::{Cond, Store as _, Txn};
use kbf_types::object_store_api::{AuditAction, AuditList};
use prost::Message as _;

use super::harness::{Farm, registered};

/// `objstore/audit/<seq>` as the registry stores it.
#[derive(Clone, PartialEq, prost::Message)]
struct Row {
    #[prost(uint64, tag = "1")]
    at: u64,
    #[prost(int32, tag = "2")]
    action: i32,
    #[prost(string, tag = "3")]
    subject: String,
    #[prost(int32, tag = "4")]
    outcome: i32,
    #[prost(uint64, tag = "5")]
    generation: u64,
    #[prost(string, tag = "6")]
    detail: String,
}

// Catches an audit listing that answers 500 `corrupt_rows` because one row has an action code
// from a newer build, an unknown row spelled in a way a client cannot read back, and a known
// action whose spelling changed: known actions keep their snake_case names, an unknown one is
// `unknown (N)` with its subject and detail.
#[tokio::test]
async fn an_audit_row_with_an_unknown_action_lists_as_unknown_with_its_code() {
    let farm = Farm::new();
    registered(&farm).await; // audit rows 1 and 2
    let row = Row {
        at: 5,
        action: 9,
        subject: "future-subject".into(),
        outcome: 2,
        generation: 3,
        detail: "future detail".into(),
    };
    farm.store
        .txn(Txn::new().put(
            "objstore/audit/00000000000000000003",
            row.encode_to_vec(),
            Cond::Absent,
        ))
        .unwrap()
        .unwrap();
    let reply = farm.call("GET", "/v1/object-stores/audit", "").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let value: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    let entries = value["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["action"], "unknown (9)");
    assert_eq!(entries[0]["subject"], "future-subject");
    assert_eq!(entries[0]["detail"], "future detail");
    assert_eq!(entries[1]["action"], "put_binding");
    assert_eq!(entries[2]["action"], "put_connection");
    let list: AuditList = reply.json();
    assert_eq!(list.entries[0].action, AuditAction::Unknown(9));
}
