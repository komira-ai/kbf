//! An audit row whose action code this build does not know still lists.

use std::sync::Arc;

use kbf_state::{Cond, Store, Txn};
use kbf_types::object_store_api::AuditAction;
use prost::Message as _;

use super::*;
use crate::object_store::rows::AuditRow;

fn plant(store: &Arc<dyn Store>, seq: u64, action: i32, subject: &str, detail: &str) {
    let row = AuditRow {
        at: 1,
        action,
        subject: subject.into(),
        outcome: 2,
        generation: seq,
        detail: detail.into(),
    };
    store
        .txn(Txn::new().put(
            format!("objstore/audit/{seq:020}"),
            row.encode_to_vec(),
            Cond::Absent,
        ))
        .unwrap()
        .unwrap();
}

// Catches a reader that fails the whole audit listing on one row written by a newer build:
// the row lists as `Unknown(code)` with its subject and detail, and the rows around it list
// as they are, newest first.
#[test]
fn a_row_with_an_unknown_action_lists_and_the_others_still_do() {
    let store = as_store(&fake());
    let root = scratch("audit-unknown");
    let reg = open(&store, &root.join("s"));
    plant(&store, 1, 1, "cold", "");
    plant(&store, 2, 9, "future-subject", "future detail");
    plant(&store, 3, 4, "cold", "");
    let list = reg.audit(std::num::NonZeroUsize::new(10).unwrap()).unwrap();
    let seen: Vec<_> = list
        .iter()
        .map(|e| (e.seq, e.action, e.subject.as_str(), e.detail.as_deref()))
        .collect();
    assert_eq!(
        seen,
        [
            (3, AuditAction::DeleteBinding, "cold", None),
            (
                2,
                AuditAction::Unknown(9),
                "future-subject",
                Some("future detail")
            ),
            (1, AuditAction::PutConnection, "cold", None),
        ]
    );
}

// Pins the codes of the row codec (arrived green): an unknown action keeps its code through
// the codec, a code too large for the row field saturates, and a negative code is no action.
#[test]
fn the_row_codec_keeps_an_unknown_code_and_refuses_a_negative_one() {
    use crate::object_store::rows::{action_code, action_of};
    assert_eq!(action_code(AuditAction::Unknown(9)), 9);
    assert_eq!(action_code(AuditAction::Unknown(u32::MAX)), i32::MAX);
    assert_eq!(action_of(9).unwrap(), AuditAction::Unknown(9));
    assert_eq!(action_of(5).unwrap(), AuditAction::Repoint);
    assert!(action_of(-1).is_err());
}
