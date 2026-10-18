//! The decisions of a node applying the cold-store registration that need no frontend:
//! what counts as the same store, what a heartbeat says, how the fetch waits, and what a
//! node with and without a key pair of its own does when the frontend cannot be reached.

use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_objstore::{Capabilities, MemoryStore};
use tokio::time::Instant;
use tonic::codegen::http::Uri;
use tonic::transport::Endpoint;

use super::*;
use crate::join::{Identity, RETRY_BASE};

fn uri(bucket: &str, prefix: &str, endpoint: &str) -> S3Uri {
    S3Uri {
        bucket: bucket.to_owned(),
        prefix: prefix.to_owned(),
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        conditional_put: false,
        min_put_rate: NonZeroU64::MIN,
    }
}

fn config(bucket: &str, prefix: &str, endpoint: &str) -> StoreConfig {
    StoreConfig {
        bucket: bucket.to_owned(),
        prefix: prefix.to_owned(),
        endpoint: endpoint.to_owned(),
        // Not identity: a registration may change these and stay the same store.
        region: "eu-west-1".to_owned(),
        conditional_put: true,
        min_put_rate: 7,
        access_key_id: "other-id".to_owned(),
        secret_access_key: "other-secret".to_owned(),
        secret_version: 9,
        connection: "main".to_owned(),
        role: "cold".to_owned(),
    }
}

/// Catches an identity that ignores a part of the address (mutants: the bucket, the
/// prefix or the endpoint left out of the comparison), one that takes keys, region or the
/// put settings for identity (the same store re-keyed would be refused), and one that
/// reads spellings of the same address as different stores (a trailing slash, a capital in
/// the host, a prefix without its closing slash, the empty prefix and the default one).
#[test]
fn a_store_is_its_endpoint_bucket_and_prefix() {
    let flag = uri("bkt", "kbf/", "http://s3.example:9000");
    for (registered, same) in [
        (config("bkt", "kbf/", "http://s3.example:9000"), true),
        (config("bkt", "kbf/", "http://s3.example:9000/"), true),
        (config("bkt", "kbf/", "HTTP://S3.Example:9000"), true),
        (config("bkt", "kbf", "http://s3.example:9000"), true),
        (config("other", "kbf/", "http://s3.example:9000"), false),
        (config("bkt", "other/", "http://s3.example:9000"), false),
        (config("bkt", "kbf/", "http://s3.example:9001"), false),
        (config("bkt", "kbf/", "http://s3.other:9000"), false),
    ] {
        assert_eq!(same_store(&flag, &registered), same, "{registered:?}");
    }
    assert!(same_store(
        &uri("bkt", DEFAULT_S3_PREFIX, "http://s3.example:9000"),
        &config("bkt", "", "http://s3.example:9000")
    ));
}

/// Catches heartbeats that say the wrong thing about the registration (mutants: a stale
/// status kept when the frontend moved on, in either direction; a node that reports
/// nothing made to report; `0` taken as a new generation): a node says what it decided,
/// `pending` for any other generation the frontend announces, and keeps its word when the
/// frontend cannot say (0). A node that does not apply registrations stays silent.
#[test]
fn a_heartbeat_says_what_was_decided_unless_the_frontend_moved_on() {
    let running = Applied::running(5);
    assert_eq!(running.for_beat(5), (5, ApplyStatus::Ok));
    assert_eq!(running.for_beat(0), (5, ApplyStatus::Ok));
    assert_eq!(running.for_beat(6), (5, ApplyStatus::Pending));
    assert_eq!(
        running.for_beat(2),
        (5, ApplyStatus::Pending),
        "a lower one too"
    );

    let probe = Applied::kept(4, ApplyStatus::ProbeFailed);
    assert_eq!(probe.for_beat(4), (0, ApplyStatus::ProbeFailed));
    assert_eq!(probe.for_beat(0), (0, ApplyStatus::ProbeFailed));
    assert_eq!(probe.for_beat(5), (0, ApplyStatus::Pending));
    let mismatch = Applied::kept(4, ApplyStatus::IdentityMismatch);
    assert_eq!(mismatch.for_beat(4), (0, ApplyStatus::IdentityMismatch));

    let none = Applied::nothing();
    assert_eq!(none.for_beat(0), (0, ApplyStatus::Ok));
    assert_eq!(
        none.for_beat(3),
        (0, ApplyStatus::Pending),
        "a registration appeared"
    );

    let silent = Applied::unreported();
    for announced in [0, 3] {
        assert_eq!(silent.for_beat(announced), (0, ApplyStatus::Unspecified));
    }
}

/// Catches a fetch that gives up on a refusal waiting cannot change, and one that
/// retries the node's own standing: only an unreachable or failing frontend is tried again.
#[test]
fn only_a_frontend_that_cannot_answer_is_worth_asking_again() {
    for code in [
        Code::Unavailable,
        Code::DeadlineExceeded,
        Code::Unknown,
        Code::Internal,
        Code::Aborted,
        Code::ResourceExhausted,
        Code::Cancelled,
    ] {
        assert!(retriable(code), "{code:?}");
    }
    for code in [
        Code::PermissionDenied,
        Code::Unauthenticated,
        Code::InvalidArgument,
        Code::NotFound,
        Code::Unimplemented,
        Code::FailedPrecondition,
        Code::Ok,
    ] {
        assert!(!retriable(code), "{code:?}");
    }
}

fn doubling(failures: u32) -> Duration {
    Duration::from_secs(1 << failures)
}

/// Catches a fetch that hammers a frontend that is down (mutant: no sleep between
/// tries: every gap is zero), one that tries the wrong number of times (mutants: one too
/// few or too many), and one that forgets how many tries it has had (the waits double
/// from the first): four tries wait 1 s, 2 s and 4 s on the paused clock.
#[tokio::test(start_paused = true)]
async fn a_fetch_waits_between_tries_and_stops_when_the_tries_are_spent() {
    let at = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let seen = Arc::clone(&at);
    let answer: Result<(), Status> = fetch_with(
        || {
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Instant::now());
            async { Err(Status::unavailable("down")) }
        },
        doubling,
        Some(4),
    )
    .await;
    assert_eq!(answer.expect_err("spent").code(), Code::Unavailable);
    let at = at.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        gaps,
        [1, 2, 4].map(Duration::from_secs),
        "{} tries",
        at.len()
    );
}

/// Catches a fetch that keeps asking after the frontend refused the node (mutant: every
/// code is retried), which would hold a denied node in a loop: one call, no wait.
#[tokio::test(start_paused = true)]
async fn a_refusal_ends_the_fetch_at_once() {
    let calls = Arc::new(Mutex::new(0_u32));
    let counted = Arc::clone(&calls);
    let before = Instant::now();
    let answer: Result<(), Status> = fetch_with(
        || {
            *counted.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            async { Err(Status::permission_denied("no")) }
        },
        doubling,
        None,
    )
    .await;
    assert_eq!(answer.expect_err("refused").code(), Code::PermissionDenied);
    assert_eq!(*calls.lock().unwrap_or_else(PoisonError::into_inner), 1);
    assert_eq!(Instant::now(), before);
}

/// Catches a fetch with no limit that stops anyway (mutant: `None` treated as a bound of
/// one): a node that must have the registration asks until the frontend answers, and the
/// answer it gets is the one it returns.
#[tokio::test(start_paused = true)]
async fn a_fetch_with_no_limit_goes_on_until_the_frontend_answers() {
    let calls = Arc::new(Mutex::new(0_u32));
    let counted = Arc::clone(&calls);
    let answer = fetch_with(
        || {
            let mut calls = counted.lock().unwrap_or_else(PoisonError::into_inner);
            *calls += 1;
            let ready = *calls > 9;
            async move {
                if ready {
                    Ok(42)
                } else {
                    Err(Status::unavailable("down"))
                }
            }
        },
        |_| Duration::from_secs(1),
        None,
    )
    .await;
    assert_eq!(answer.expect("answered"), 42);
    assert_eq!(*calls.lock().unwrap_or_else(PoisonError::into_inner), 10);
}

/// A connector that fails every dial, noting when.
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

/// A joiner whose frontend cannot be reached, and the dials it makes.
fn unreachable_frontend() -> (Joiner, Dials) {
    let dials = Dials::default();
    let channel = Endpoint::from_static("http://frontend.invalid:8981")
        .connect_with_connector_lazy(dials.clone());
    let who = Identity {
        instance_id: "node".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    (Joiner::new(channel, who, RETRY_BASE).with_unit(half), dials)
}

fn joined(objstore_generation: u64) -> Joined {
    Joined {
        member_id: 1,
        boot: 1,
        interval: Duration::from_secs(1),
        generation: 1,
        objstore_generation,
    }
}

fn keys(name: &str) -> Option<String> {
    Some(format!("value-of-{name}"))
}

fn no_keys(_: &str) -> Option<String> {
    None
}

fn flag() -> S3Uri {
    uri("bkt", "kbf/", "http://127.0.0.1:9")
}

/// Catches a node with a key pair of its own that waits for ever on a frontend that is
/// down (mutant: no bound for a node with its own store), or that starts on its own store
/// and says nothing of why (mutant: `ok` instead of `pending`): it asks four times, on the
/// jittered waits, then starts on its own store and reports the generation `pending`.
#[tokio::test(start_paused = true)]
async fn a_node_with_its_own_keys_starts_on_them_when_the_frontend_is_down() {
    let (joiner, dials) = unreachable_frontend();
    let cold = Registrable::new(&flag(), keys).expect("a store");
    let before = Instant::now();
    let (_store, applied) = cold
        .registered(&joiner, &joined(3))
        .await
        .expect("starts on its own store");
    assert_eq!(applied.for_beat(3), (0, ApplyStatus::Pending));
    let at = dials
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(at.len(), FALLBACK_ATTEMPTS as usize);
    let waits: Duration = (0..FALLBACK_ATTEMPTS - 1)
        .map(|failures| joiner.pause(failures))
        .sum();
    assert_eq!(Instant::now() - before, waits);
}

/// Catches a node with nothing of its own that starts anyway or gives up (mutants: the
/// bound applied to every node; the own store assumed): it asks on, past any number of
/// tries, and has not returned.
#[tokio::test(start_paused = true)]
async fn a_node_with_no_keys_asks_until_the_frontend_answers() {
    let (joiner, dials) = unreachable_frontend();
    let cold = Registrable::new(&flag(), no_keys).expect("a store");
    let joined = joined(3);
    let asking = cold.registered(&joiner, &joined);
    let mut asking = std::pin::pin!(asking);
    let outcome = tokio::time::timeout(Duration::from_secs(600), &mut asking).await;
    assert!(outcome.is_err(), "still asking after ten minutes");
    let dialled = dials.0.lock().unwrap_or_else(PoisonError::into_inner).len();
    assert!(dialled > FALLBACK_ATTEMPTS as usize + 4, "{dialled} tries");
}

/// Catches a node that calls the frontend although nothing is registered (mutant: the
/// zero generation fetched), and one whose missing keys are no error when nothing is
/// registered to supply them: it keeps its flags' own store, says nothing is wrong, and
/// dials nobody; without keys it says which variable is missing.
#[tokio::test(start_paused = true)]
async fn with_nothing_registered_the_node_runs_on_its_flags_or_says_what_is_missing() {
    let (joiner, dials) = unreachable_frontend();
    let cold = Registrable::new(&flag(), keys).expect("a store");
    let (_store, applied) = cold
        .registered(&joiner, &joined(0))
        .await
        .expect("its own store");
    assert_eq!(applied, Applied::nothing());
    assert!(
        dials
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );

    let cold = Registrable::new(&flag(), no_keys).expect("a store");
    let Err(error) = cold.registered(&joiner, &joined(0)).await else {
        panic!("a node with no keys and no registration starts");
    };
    let text = error.to_string();
    assert!(text.contains("AWS_ACCESS_KEY_ID"), "{text}");
    assert!(matches!(
        error.downcast_ref::<ColdApplyError>(),
        Some(ColdApplyError::MissingKeys(_))
    ));
    assert!(
        dials
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
}

/// Catches a node with no frontend that cannot start on its own keys, or that starts
/// without them.
#[test]
fn a_node_with_no_frontend_needs_its_own_keys() {
    assert!(
        Registrable::new(&flag(), keys)
            .expect("a store")
            .alone()
            .is_ok()
    );
    let Err(error) = Registrable::new(&flag(), no_keys).expect("a store").alone() else {
        panic!("a store with no keys opens");
    };
    assert!(error.to_string().contains("AWS_ACCESS_KEY_ID"));
    // A refused configuration is not a missing key.
    let bad = uri("bkt", "kbf/", "https://s3.example");
    assert!(Registrable::new(&bad, keys).is_err());
}

/// Catches a `file://` node that reports a registration as its own (mutant: `running`) or
/// stays silent about one.
#[tokio::test(start_paused = true)]
async fn a_directory_store_is_never_a_registration() {
    let (joiner, dials) = unreachable_frontend();
    let local = || Fixed(MemoryStore::new(Capabilities::default()));
    let (_, applied) = local()
        .registered(&joiner, &joined(0))
        .await
        .expect("starts");
    assert_eq!(applied, Applied::nothing());
    let (_, applied) = local()
        .registered(&joiner, &joined(6))
        .await
        .expect("starts");
    assert_eq!(applied.for_beat(6), (0, ApplyStatus::IdentityMismatch));
    assert!(
        dials
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
}
