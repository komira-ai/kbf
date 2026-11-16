//! The signature of each call, as the fake checks it the way the node does.

mod support;

use std::time::{Duration, SystemTime};

use kbf_trees_client::fake::{KEY_ID, REGION, SECRET};
use kbf_trees_client::{Client, Key, Reason};
use support::{create, fake, fixed, job, whole};
use tonic::Code;

fn sixteen_minutes_late() -> SystemTime {
    fixed() + Duration::from_secs(16 * 60)
}

fn fourteen_minutes_early() -> SystemTime {
    fixed() - Duration::from_secs(14 * 60)
}

/// Asserts each call carries `x-amz-date`, `x-amz-content-sha256` and an `Authorization` that
/// names the key, the scope `<date>/<region>/s3` and the three signed headers, and that the
/// secret is in no header of any call. Catches a client that sends its key in the clear.
#[tokio::test]
async fn every_call_is_signed_and_the_secret_is_never_sent() {
    let (server, config) = fake().await;
    let client = Client::new(config).unwrap();
    let created = client
        .create_tree(create(vec![whole("a", 1, 1)], vec![job("j")]))
        .await
        .unwrap();
    client
        .get_placement(created.placement(), None)
        .await
        .unwrap();
    client.delete_tree(created.placement()).await.unwrap();
    let heads = server.heads();
    assert_eq!(heads.len(), 3);
    for head in heads {
        let text = |name: &str| head.get(name).unwrap().to_str().unwrap().to_owned();
        assert_eq!(text("x-amz-date"), "20270115T080000Z");
        assert_eq!(text("x-amz-content-sha256").len(), 64);
        let auth = text("authorization");
        let prefix = format!(
            "AWS4-HMAC-SHA256 Credential={KEY_ID}/20270115/{REGION}/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
        );
        assert!(auth.starts_with(&prefix), "{auth}");
        for (name, value) in &head {
            let value = String::from_utf8_lossy(value.as_bytes());
            assert!(!value.contains(SECRET), "{name}: {value}");
        }
    }
}

/// Asserts a call signed with a wrong secret, in another region, or at a time more than 15
/// minutes from the node's is refused `unauthenticated` with the fake's reason, a definite
/// refusal; one 14 minutes early passes. Catches a signing that only works on a node whose
/// clock is the client's.
#[tokio::test]
async fn a_call_the_node_cannot_verify_is_refused_unauthenticated() {
    let (server, config) = fake().await;
    let request = create(vec![whole("a", 1, 1)], vec![job("j")]);
    let wrong_secret = config.clone();
    let cases = [
        (
            Client::new(
                kbf_trees_client::Config::new(
                    wrong_secret.node().clone(),
                    Key::new(KEY_ID, "not-the-secret").unwrap(),
                    REGION,
                )
                .unwrap()
                .with_clock(fixed),
            ),
            "the signature does not match",
        ),
        (
            Client::new(
                kbf_trees_client::Config::new(
                    config.node().clone(),
                    Key::new(KEY_ID, SECRET).unwrap(),
                    "elsewhere",
                )
                .unwrap()
                .with_clock(fixed),
            ),
            "an unknown key",
        ),
        (
            Client::new(config.clone().with_clock(sixteen_minutes_late)),
            "the time is too far from the node's",
        ),
    ];
    for (client, why) in cases {
        let error = client
            .unwrap()
            .create_tree(request.clone())
            .await
            .unwrap_err();
        let refusal = error.refusal().expect("a refusal");
        assert_eq!(
            (refusal.reason(), refusal.about(), refusal.code()),
            (Reason::Unauthenticated, Some(why), Code::Unauthenticated)
        );
        assert!(!error.may_have_applied());
    }
    assert!(server.calls().is_empty(), "a refused call is not read");
    let early = Client::new(config.with_clock(fourteen_minutes_early)).unwrap();
    assert!(early.create_tree(request).await.is_ok());
}
