//! The `x-kbf-qos` header on Execute: the level the scheduler is handed.

use kbf_front::QosAllow;
use tonic::metadata::MetadataValue;

use super::*;

/// Executes `job` with the header `qos` (none if `None`) and returns the level the
/// dispatch was handed, and the action digest it was handed it for.
async fn level_for(allow: QosAllow, qos: Option<&str>, names: &[&str]) -> Vec<(Qos, String)> {
    let script = Arc::new(Script::default());
    let farm = Farm::with_qos_allow(Arc::clone(&script), allow).await;
    for name in names {
        let job = job(name, &[], false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        let mut request = tonic::Request::new(ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(job.action.proto.clone()),
            ..Default::default()
        });
        if let Some(qos) = qos {
            request
                .metadata_mut()
                .insert("x-kbf-qos", MetadataValue::try_from(qos).expect("ascii"));
        }
        farm.exec().execute(request).await.expect("Execute");
    }
    script
        .submitted()
        .into_iter()
        .map(|s| {
            let key = ActionKey::try_from(&s.request.key).expect("an action's key");
            (s.request.qos, key.action.hash_hex())
        })
        .collect()
}

async fn one(allow: QosAllow, qos: Option<&str>) -> Qos {
    level_for(allow, qos, &["a"]).await.remove(0).0
}

/// Catches: a call without the header leaving `ci` (it is every existing client).
#[tokio::test]
async fn no_header_submits_at_ci() {
    assert_eq!(one(QosAllow::DEFAULT, None).await, Qos::Ci);
}

/// Catches: the header read but not passed on (every call stays `ci`), and `ci`
/// itself not accepted.
#[tokio::test]
async fn ci_and_batch_headers_reach_the_scheduler() {
    assert_eq!(one(QosAllow::DEFAULT, Some("ci")).await, Qos::Ci);
    assert_eq!(one(QosAllow::DEFAULT, Some("batch")).await, Qos::Batch);
}

/// Catches: a client taking the most urgent level by asking for it, and a refused
/// level failing the call (the build must go on, at `ci`).
#[tokio::test]
async fn interactive_is_ci_unless_the_operator_allows_it() {
    assert_eq!(one(QosAllow::DEFAULT, Some("interactive")).await, Qos::Ci);
    let open = "ci,batch,interactive".parse().unwrap();
    assert_eq!(one(open, Some("interactive")).await, Qos::Interactive);
}

/// Catches: a value that is no level failing the Execute or becoming a level; and
/// the case of the value mattering.
#[tokio::test]
async fn garbage_is_ci_and_case_is_ignored() {
    assert_eq!(one(QosAllow::DEFAULT, Some("urgent")).await, Qos::Ci);
    assert_eq!(one(QosAllow::DEFAULT, Some("")).await, Qos::Ci);
    assert_eq!(one(QosAllow::DEFAULT, Some("Batch")).await, Qos::Batch);
}

/// Catches: the operator's list ignored (`batch` granted when the list is `ci`).
#[tokio::test]
async fn the_operators_list_decides() {
    let only_ci = "ci".parse().unwrap();
    assert_eq!(one(only_ci, Some("batch")).await, Qos::Ci);
}

/// Catches: the level leaking into what the cache sees: the same action sent at `ci`
/// and at `batch` is the same action digest (the header is no platform property, so
/// the cache is shared between PR checks and a batch release build).
#[tokio::test]
async fn the_level_does_not_change_the_action_digest() {
    let at_ci = level_for(QosAllow::DEFAULT, None, &["same"]).await;
    let at_batch = level_for(QosAllow::DEFAULT, Some("batch"), &["same"]).await;
    assert_eq!(
        (at_ci[0].0.clone(), at_batch[0].0.clone()),
        (Qos::Ci, Qos::Batch)
    );
    assert_eq!(at_ci[0].1, at_batch[0].1);
}
