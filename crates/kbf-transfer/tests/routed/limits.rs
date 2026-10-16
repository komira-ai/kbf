//! The limits a data plane keeps.

use kbf_transfer::{Cas, TransferConfig, digest_of};

use super::support::{Fake, plane};

/// Catches requests without a limit: with a limit of two and six reads of different blobs
/// started together, no more than two are at the node at once, the rest wait, and every
/// read still gets its blob. Catches a limit that is not the configured one.
#[tokio::test]
async fn no_more_requests_than_the_limit_are_out_at_once() {
    let fake = Fake::new(b"four");
    let gate = fake.gate();
    let config = TransferConfig {
        in_flight: 2,
        ..TransferConfig::default()
    };
    let cas = plane(&fake, &config);
    assert_eq!(cas.in_flight(), 2);

    let reads = futures::future::join_all((0..6).map(|i| {
        let digest = digest_of(format!("blob {i}").as_bytes());
        let cas = cas.clone();
        async move { cas.get(&digest).await }
    }));
    // The reads are polled before this: the ones that could start are at the node.
    let release = async {
        assert_eq!(fake.active(), 2);
        gate.add_permits(100);
    };
    let (answers, ()) = tokio::join!(reads, release);
    for answer in answers {
        assert_eq!(answer.expect("a blob"), b"four");
    }
    assert_eq!(fake.peak(), 2);
}

/// Catches a limit of zero that would stop everything: it is taken as one.
#[tokio::test]
async fn a_limit_of_zero_is_one() {
    let fake = Fake::new(b"four");
    let config = TransferConfig {
        in_flight: 0,
        ..TransferConfig::default()
    };
    let cas = plane(&fake, &config);
    assert_eq!(cas.in_flight(), 1);
    let digest = digest_of(b"anything");
    assert_eq!(cas.get(&digest).await.expect("a blob"), b"four");
}
