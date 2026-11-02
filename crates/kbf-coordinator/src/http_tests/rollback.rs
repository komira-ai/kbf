//! The desired route when the bad mark of the version being set cannot be cleared.

use super::*;

use crate::desired::StoreError;
use crate::ops::{OpsError, OpsState, OpsStore, ServiceOps};
use crate::testutil::version;

/// Catches: CI told its desired state was set while the mark that keeps the version from
/// being ordered stayed: the answer is 500 `internal` (no path in it) and no desired state
/// is stored.
#[tokio::test]
async fn a_mark_that_cannot_be_cleared_is_a_500_and_sets_nothing() {
    let mut rig = Rig::new("http-unclearable");
    let mut state = OpsState::default();
    state.services.insert(
        "kbf".to_owned(),
        ServiceOps {
            bad: vec![version("2.0")],
            ..ServiceOps::default()
        },
    );
    let ops = OpsStore::with_writer(
        state,
        Box::new(|_| {
            Err(OpsError::Io(StoreError::Io {
                path: "ops".into(),
                source: io::Error::other("disk full"),
            }))
        }),
    );
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_ops(ops);
    let body = r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#;
    let (code, text) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), body),
    )
    .await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text, json!({"error": "internal"}));
    assert_eq!(status(&rig).await["desired"], Value::Null);
}
