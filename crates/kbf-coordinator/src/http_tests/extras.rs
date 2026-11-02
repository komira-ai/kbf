//! A heartbeat with a bad optional fact over HTTP: answered, and the fact counted by name.

use super::*;

/// Catches: the reader's names not reaching the counter. A beat with a bad `step` and a bad
/// `commit` is answered 200 and counts one for each; a good beat counts nothing.
#[tokio::test]
async fn a_bad_optional_fact_is_answered_and_counted_by_name() {
    let rig = Rig::new("http-extras-count");
    let who = updater("d1", "daemon");
    rig.at(2_000);
    let beat = |extra: &str| {
        format!(r#"{{"platform":"linux-x86_64","version":"1.0","health":"healthy"{extra}}}"#)
    };
    for (body, want) in [
        (beat(""), Value::Null),
        (
            beat(r#","step":{"name":"verify"},"commit":"maybe""#),
            json!({"commit": 1, "step": 1}),
        ),
    ] {
        let (code, _) = call(
            &rig,
            Surface::Heartbeat,
            &who,
            request("POST", HEARTBEAT_PATH, None, &body),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(status(&rig).await["extras_dropped"], want);
    }
}
