//! The waits of a node that cannot reach its frontend: [`backoff`]'s, jittered.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;
use tonic::codegen::http::Uri;
use tonic::transport::Endpoint;

use super::{Identity, Joined, Joiner, RETRY_BASE, RETRY_MAX, backoff, jittered, unit_random};

/// A number in `[0, 1)` from `seed`: splitmix64's output scaled by 2^-53, so a test has
/// the spread of many nodes and knows every draw.
fn unit_of(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    #[allow(clippy::cast_precision_loss)] // 53 bits fit an f64 exactly
    let unit = (z >> 11) as f64 / (1_u64 << 53) as f64;
    unit
}

/// Catches a join wait that is not jittered (mutant: [`jittered`] returns its wait):
/// every node then retries at the same instants after a frontend restart. For every
/// failure count up to the cap, the waits of 64 seeds each lie in `[backoff / 2,
/// backoff]` and are not all one value; the ends of the range are the unit's ends, and a
/// unit outside `[0, 1]` is taken at the nearer end.
#[test]
fn a_jittered_wait_is_in_the_upper_half_and_differs_between_nodes() {
    for failures in 0..8 {
        let wait = backoff(RETRY_BASE, failures);
        let waits: BTreeSet<Duration> = (0..64).map(|seed| jittered(wait, unit_of(seed))).collect();
        assert!(
            waits.iter().all(|w| *w >= wait / 2 && *w <= wait),
            "failures {failures}: {waits:?} outside [{:?}, {wait:?}]",
            wait / 2
        );
        assert!(waits.len() > 32, "failures {failures}: {waits:?}");
        assert_eq!(jittered(wait, 0.0), wait);
        assert_eq!(jittered(wait, 1.0), wait / 2);
        assert_eq!(jittered(wait, -3.0), wait);
        assert_eq!(jittered(wait, 7.0), wait / 2);
    }
    assert_eq!(backoff(RETRY_BASE, 40), RETRY_MAX);
}

/// Catches a random source that is not in `[0, 1)` or that gives one value every time
/// (no jitter in a real node, whatever the wait does with it).
#[test]
fn the_system_source_spreads_over_the_unit_interval() {
    let draws: Vec<f64> = (0..64).map(|_| unit_random()).collect();
    assert!(draws.iter().all(|u| (0.0..1.0).contains(u)), "{draws:?}");
    assert!(draws.iter().any(|u| *u != draws[0]), "{draws:?}");
}

/// A connector that fails every dial as a lookup that found nothing does, noting when.
#[derive(Clone, Default)]
struct Dials(Arc<Mutex<Vec<Instant>>>);

impl tonic::codegen::Service<Uri> for Dials {
    type Response = hyper_util::rt::TokioIo<tokio::net::TcpStream>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<Self::Response>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Instant::now());
        Box::pin(async { Err(std::io::Error::other("dns error: no such host")) })
    }
}

fn half() -> f64 {
    0.5
}

/// Catches a join loop that sleeps [`backoff`] itself rather than the jittered wait
/// (mutant: `join` sleeping `backoff(self.retry, failures)`): on the paused clock the
/// gaps between dials of a registration that always fails are exactly the waits the
/// jitter source says, `backoff * 3/4` for a unit of one half, doubling to the cap.
#[tokio::test(start_paused = true)]
async fn the_join_loop_waits_the_jittered_backoff() {
    let dials = Dials::default();
    let channel = Endpoint::from_static("http://frontend.invalid:8981")
        .connect_with_connector_lazy(dials.clone());
    let who = Identity {
        instance_id: "node".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let mut joiner = Joiner::new(channel, who, RETRY_BASE).with_unit(half);
    let joining = tokio::spawn(async move { joiner.join().await });
    let at = loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let at = dials
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if at.len() >= 8 {
            break at;
        }
    };
    joining.abort();
    let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
    let want: Vec<Duration> = (0..7)
        .map(|failures| jittered(backoff(RETRY_BASE, failures), 0.5))
        .collect();
    assert_eq!(gaps, want);
    assert_eq!(want[0], Duration::from_millis(750));
    assert_eq!(want[6], RETRY_MAX.mul_f64(0.75));
}

/// Catches a heartbeat loop whose failed calls wait [`backoff`] itself (mutant:
/// `beat_until_lapse` sleeping `backoff(self.retry, failures)`): after a frontend
/// restart every registered node would beat again in step. On the paused clock the gap
/// between two failed beats is the heartbeat interval, then the jittered wait.
#[tokio::test(start_paused = true)]
async fn a_failed_heartbeat_waits_the_jittered_backoff() {
    let dials = Dials::default();
    let channel = Endpoint::from_static("http://frontend.invalid:8981")
        .connect_with_connector_lazy(dials.clone());
    let who = Identity {
        instance_id: "node".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let mut joiner = Joiner::new(channel, who, RETRY_BASE).with_unit(half);
    let this = Joined {
        member_id: 1,
        boot: 1,
        interval: Duration::from_secs(2),
        generation: 1,
    };
    let mut asks = joiner.node.maps().asks();
    let beating = tokio::spawn(async move { joiner.beat_until_lapse(&this, &mut asks).await });
    let at = loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let at = dials
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if at.len() >= 5 {
            break at;
        }
    };
    beating.abort();
    let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
    let want: Vec<Duration> = (0..4)
        .map(|failures| Duration::from_secs(2) + jittered(backoff(RETRY_BASE, failures), 0.5))
        .collect();
    assert_eq!(gaps, want);
}
