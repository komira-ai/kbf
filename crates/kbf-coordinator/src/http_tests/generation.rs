//! The record of the writes over `POST /v1/desired` and `GET /v1/convergence`: the
//! generation every accepted write raises, what the last change of version replaced, the
//! caller's `write_id`, and a write conditional on the generation (`if_generation`).

use super::*;

fn body(version: &str, storage: u32, extra: &str) -> String {
    format!(r#"{{"version":"{version}","min_up":{{"storage":{storage},"frontend":1}}{extra}}}"#)
}

async fn post(rig: &Rig, body: &str) -> (StatusCode, Value) {
    call(
        rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), body),
    )
    .await
}

async fn read(rig: &Rig) -> Value {
    let (code, body) = convergence(rig, Some(&ci_token())).await;
    assert_eq!(code, StatusCode::OK);
    body
}

/// The record fields of an answer.
fn record(answer: &Value) -> Value {
    json!({
        "generation": answer["generation"],
        "replaced": answer["replaced"],
        "write_id": answer["write_id"],
    })
}

/// Catches: a write that does not record what it replaced, or does not raise the generation:
/// the first write has generation 1 and nothing replaced; the second, of another version,
/// has generation 2 and replaced the first's version and minimums. The answer of the POST and
/// the convergence read say the same.
#[tokio::test]
async fn a_write_records_replaced_and_raises_generation() {
    let rig = Rig::new("gen-records");
    assert_eq!(
        record(&read(&rig).await),
        json!({"generation": 0, "replaced": null, "write_id": ""})
    );
    let (code, first) = post(&rig, &body("1.0", 2, "")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        record(&first),
        json!({"generation": 1, "replaced": null, "write_id": ""})
    );
    let (code, second) = post(&rig, &body("2.0", 1, r#","write_id":"run-7""#)).await;
    assert_eq!(code, StatusCode::OK);
    let want = json!({
        "generation": 2,
        "replaced": {"version": "1.0", "min_up": {"frontend": 1, "storage": 2}},
        "write_id": "run-7",
    });
    assert_eq!(record(&second), want);
    let now = read(&rig).await;
    assert_eq!(record(&now), want);
    assert_eq!(now["desired"], "2.0");
}

/// Catches: `replaced` overwritten by a write that sets the version already desired, which
/// would make a restore set this release again; and such a write not raising the generation,
/// which would let a write conditional on a read before it pass.
#[tokio::test]
async fn setting_the_same_version_again_raises_generation_and_keeps_replaced() {
    let rig = Rig::new("gen-same");
    post(&rig, &body("1.0", 2, "")).await;
    post(&rig, &body("2.0", 1, "")).await;
    let (code, again) = post(&rig, &body("2.0", 3, r#","write_id":"again""#)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        record(&again),
        json!({
            "generation": 3,
            "replaced": {"version": "1.0", "min_up": {"frontend": 1, "storage": 2}},
            "write_id": "again",
        })
    );
    assert_eq!(again["min_up"]["storage"], 3);
}

/// Catches: a write made on a generation that is no longer the service's accepted: it is a 409
/// `conflict` that changes neither the state nor the record. That holds when writes went from
/// A to B and back to A since the read, so the version alone is the same as it was read, and
/// it holds for a write of the very version and minimums already desired.
#[tokio::test]
async fn a_stale_if_generation_is_refused_with_409_and_changes_nothing() {
    let rig = Rig::new("gen-stale");
    post(&rig, &body("1.0", 1, "")).await;
    let seen = read(&rig).await["generation"].as_u64().expect("generation");
    post(&rig, &body("2.0", 1, "")).await;
    post(&rig, &body("1.0", 1, "")).await;
    let before = read(&rig).await;
    assert_eq!(before["desired"], "1.0");
    for stale in [
        body(
            "3.0",
            2,
            &format!(r#","if_generation":{seen},"write_id":"late""#),
        ),
        body("1.0", 1, &format!(r#","if_generation":{seen}"#)),
        body("1.0", 1, r#","if_generation":99"#),
    ] {
        let (code, answer) = post(&rig, &stale).await;
        assert_eq!(code, StatusCode::CONFLICT, "{stale}");
        assert_eq!(answer, json!({"error": "conflict"}));
    }
    assert_eq!(read(&rig).await, before);
    assert_eq!(status(&rig).await["desired"]["min_up"]["storage"], 1);

    let now = before["generation"].as_u64().expect("generation");
    let (code, set) = post(
        &rig,
        &body(
            "3.0",
            2,
            &format!(r#","if_generation":{now},"write_id":"mine""#),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(set["generation"], now + 1);
    assert_eq!(set["write_id"], "mine");
    assert_eq!(set["replaced"]["version"], "1.0");
}

/// Catches: a write without `if_generation` or `write_id` refused or changed by the new
/// fields: it sets the state as before, whatever the generation, and its answer is the state
/// with the record after it. A malformed `if_generation` or `write_id` is a 400 that changes
/// nothing.
#[tokio::test]
async fn a_request_without_if_generation_behaves_as_today() {
    let rig = Rig::new("gen-plain");
    for version in ["1.0", "2.0", "2.0", "1.0"] {
        let (code, set) = post(&rig, &body(version, 1, "")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(set["version"], version);
        assert_eq!(set["write_id"], "");
    }
    assert_eq!(read(&rig).await["generation"], 4);
    let long = "x".repeat(crate::writes::MAX_WRITE_ID + 1);
    for bad in [
        r#","if_generation":-1"#.to_owned(),
        r#","if_generation":"4""#.to_owned(),
        r#","write_id":"""#.to_owned(),
        r#","write_id":"two words""#.to_owned(),
        r#","write_id":7"#.to_owned(),
        format!(r#","write_id":"{long}""#),
    ] {
        let (code, answer) = post(&rig, &body("9.0", 1, &bad)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(answer, json!({"error": "bad_request"}));
    }
    let longest = "x".repeat(crate::writes::MAX_WRITE_ID);
    let (code, set) = post(
        &rig,
        &body("1.0", 1, &format!(r#","write_id":"{longest}""#)),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(set["generation"], 5);
    assert_eq!(set["write_id"], longest);
}
