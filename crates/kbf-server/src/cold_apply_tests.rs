//! The decisions of a node applying the cold-store registration that need no frontend:
//! what counts as the same store, what a heartbeat says, how the fetch waits, and what a
//! node with and without a key pair of its own does when the frontend cannot be reached.

use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{ByteRange, Capabilities, MemoryStore, ObjectKey, ObjectStoreError};
use kbf_proto::kbf::enroll::v1::GetStoreConfigRequest;
use kbf_proto::kbf::enroll::v1::cold_store_server::{ColdStore, ColdStoreServer};
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

/// Catches an identity that ignores a part of the address (defects: the bucket, the
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

/// Catches heartbeats that say the wrong thing about the registration (defects: a stale
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

fn answer(generation: u64) -> GetStoreConfigResponse {
    GetStoreConfigResponse {
        generation,
        config: None,
    }
}

/// Catches a fetch that hammers a frontend that is down (defect: no sleep between
/// tries: every gap is zero), one that tries the wrong number of times (defects: one too
/// few or too many), and one that forgets how many tries it has had (the waits double
/// from the first): four tries wait 1 s, 2 s and 4 s on the paused clock.
#[tokio::test(start_paused = true)]
async fn a_fetch_waits_between_tries_and_stops_when_the_tries_are_spent() {
    let at = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let seen = Arc::clone(&at);
    let got = fetch_with(
        &mut || {
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Instant::now());
            Box::pin(async { Err(Status::unavailable("down")) })
        },
        &doubling,
        Some(4),
    )
    .await;
    assert_eq!(got.expect_err("spent").code(), Code::Unavailable);
    let at = at.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        gaps,
        [1, 2, 4].map(Duration::from_secs),
        "{} tries",
        at.len()
    );
}

/// Catches a fetch that keeps asking after the frontend refused the node (defect: every
/// code is retried), which would hold a denied node in a loop: one call, no wait.
#[tokio::test(start_paused = true)]
async fn a_refusal_ends_the_fetch_at_once() {
    let calls = Arc::new(Mutex::new(0_u32));
    let counted = Arc::clone(&calls);
    let before = Instant::now();
    let got = fetch_with(
        &mut || {
            *counted.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            Box::pin(async { Err(Status::permission_denied("no")) })
        },
        &doubling,
        None,
    )
    .await;
    assert_eq!(got.expect_err("refused").code(), Code::PermissionDenied);
    assert_eq!(*calls.lock().unwrap_or_else(PoisonError::into_inner), 1);
    assert_eq!(Instant::now(), before);
}

/// Catches a fetch with no limit that stops anyway (defect: `None` treated as a bound of
/// one): a node that must have the registration asks until the frontend answers, and the
/// answer it gets is the one it returns.
#[tokio::test(start_paused = true)]
async fn a_fetch_with_no_limit_goes_on_until_the_frontend_answers() {
    let calls = Arc::new(Mutex::new(0_u32));
    let counted = Arc::clone(&calls);
    let got = fetch_with(
        &mut || {
            let mut calls = counted.lock().unwrap_or_else(PoisonError::into_inner);
            *calls += 1;
            let ready = *calls > 9;
            Box::pin(async move {
                if ready {
                    Ok(answer(42))
                } else {
                    Err(Status::unavailable("down"))
                }
            })
        },
        &|_| Duration::from_secs(1),
        None,
    )
    .await;
    assert_eq!(got.expect("answered").generation, 42);
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
/// down (defect: no bound for a node with its own store), or that starts on its own store
/// and says nothing of why (defect: `ok` instead of `pending`): it asks four times, on the
/// jittered waits, then starts on its own store and reports the generation `pending`.
#[tokio::test(start_paused = true)]
async fn a_node_with_its_own_keys_starts_on_them_when_the_frontend_is_down() {
    let (joiner, dials) = unreachable_frontend();
    let cold = Registrable::new(&flag(), None, keys).expect("a store");
    let before = Instant::now();
    // A bound that is lost would ask for ever: the paused clock would run on, so the
    // test says so after ten virtual minutes instead of hanging.
    let joined = joined(3);
    let asked = tokio::time::timeout(Duration::from_secs(600), cold.registered(&joiner, &joined));
    let (_store, applied) = asked
        .await
        .expect("gave up asking")
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

/// Catches a node with nothing of its own that starts anyway or gives up (defects: the
/// bound applied to every node; the own store assumed), and one that asks without
/// waiting (defect: no sleep between tries, which a paused clock turns into a loop that
/// never lets the time move, so the clock is moved by hand): after ten virtual minutes it
/// is still asking, on the jittered waits, a few dozen times and not thousands.
#[tokio::test(start_paused = true)]
async fn a_node_with_no_keys_asks_until_the_frontend_answers() {
    let (joiner, dials) = unreachable_frontend();
    let cold = Registrable::new(&flag(), None, no_keys).expect("a store");
    let joined = joined(3);
    let mut asking = std::pin::pin!(cold.registered(&joiner, &joined));
    for second in 0..600 {
        if let std::task::Poll::Ready(out) = futures::poll!(asking.as_mut()) {
            panic!("gave up after {second} s: {:?}", out.map(drop));
        }
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    let dialled = dials.0.lock().unwrap_or_else(PoisonError::into_inner).len();
    assert!(dialled > FALLBACK_ATTEMPTS as usize + 4, "{dialled} tries");
    assert!(dialled < 100, "{dialled} tries in ten minutes");
}

/// Catches a node that calls the frontend although nothing is registered (defect: the
/// zero generation fetched), and one whose missing keys are no error when nothing is
/// registered to supply them: it keeps its flags' own store, says nothing is wrong, and
/// dials nobody; without keys it says which variable is missing.
#[tokio::test(start_paused = true)]
async fn with_nothing_registered_the_node_runs_on_its_flags_or_says_what_is_missing() {
    let (joiner, dials) = unreachable_frontend();
    let cold = Registrable::new(&flag(), None, keys).expect("a store");
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

    let cold = Registrable::new(&flag(), None, no_keys).expect("a store");
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
        Registrable::new(&flag(), None, keys)
            .expect("a store")
            .alone()
            .is_ok()
    );
    let Err(error) = Registrable::new(&flag(), None, no_keys)
        .expect("a store")
        .alone()
    else {
        panic!("a store with no keys opens");
    };
    assert!(error.to_string().contains("AWS_ACCESS_KEY_ID"));
    // A refused configuration is not a missing key.
    let bad = uri("bkt", "kbf/", "https://s3.example");
    assert!(Registrable::new(&bad, None, keys).is_err());
}

/// Catches a `file://` node that reports a registration as its own (defect: `running`) or
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

/// Catches registered keys that cannot be built into a store being called a failed probe
/// (defect: `Config` mapped to `Probe`), a store address the client refuses (https) or a
/// prefix that is not a key passing as usable, and the refusal a node with no key pair
/// of its own is given changing with the reason.
#[tokio::test]
async fn a_store_that_cannot_be_built_is_its_own_reason() {
    let flag = flag();
    for (bad, what) in [
        (
            config("bkt", "kbf/", "https://s3.example"),
            "an https endpoint",
        ),
        (
            config("bkt", "kb f/", "http://127.0.0.1:9"),
            "a prefix that is not a key",
        ),
    ] {
        let got = registered_store(&flag, &bad, "n-1").await;
        assert!(matches!(got, Err(Unusable::Config)), "{what}: {got:?}");
    }
    assert!(matches!(
        refusal(Unusable::Config),
        ColdApplyError::Unusable
    ));
    assert!(matches!(
        refusal(Unusable::Probe(ProbeStep::Get)),
        ColdApplyError::Probe(ProbeStep::Get)
    ));
}

/// A frontend's `ColdStore` that answers what a test says, over loopback plain text (the
/// real service refuses plain text; the node's call does not care who answers).
struct Canned(Result<GetStoreConfigResponse, Code>);

#[tonic::async_trait]
impl ColdStore for Canned {
    async fn get_store_config(
        &self,
        _: tonic::Request<GetStoreConfigRequest>,
    ) -> Result<tonic::Response<GetStoreConfigResponse>, Status> {
        match &self.0 {
            Ok(reply) => Ok(tonic::Response::new(GetStoreConfigResponse {
                generation: reply.generation,
                config: reply.config.clone(),
            })),
            Err(code) => Err(Status::new(*code, "canned")),
        }
    }
}

/// A joiner whose frontend answers `reply` to every fetch.
async fn answering(reply: Result<GetStoreConfigResponse, Code>) -> Joiner {
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().expect("addr"))
            .expect("bind");
    let addr = incoming.local_addr().expect("addr");
    let server = tonic::transport::Server::builder()
        .add_service(ColdStoreServer::new(Canned(reply)))
        .serve_with_incoming(incoming);
    tokio::spawn(server);
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy();
    let who = Identity {
        instance_id: "node".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    Joiner::new(channel, who, RETRY_BASE)
}

const ID: &str = "AKIAOK";
const SECRET: &str = "right-secret";

/// The registered store of generation `generation` on `s3`, with this key pair.
fn registered(s3: &FakeS3, generation: u64, secret: &str) -> GetStoreConfigResponse {
    GetStoreConfigResponse {
        generation,
        config: Some(StoreConfig {
            connection: "main".to_owned(),
            role: "cold".to_owned(),
            endpoint: s3.endpoint().to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "bkt".to_owned(),
            prefix: "kbf/".to_owned(),
            conditional_put: true,
            min_put_rate: 0,
            access_key_id: ID.to_owned(),
            secret_access_key: secret.to_owned(),
            secret_version: generation,
        }),
    }
}

/// The node's flag: the store of the fake, and `keys` of its own or none.
fn flagged(s3: &FakeS3, keys: bool) -> Registrable {
    let flag = uri("bkt", "kbf/", s3.endpoint());
    Registrable::new(&flag, None, if keys { own } else { no_keys }).expect("a store")
}

fn own(name: &str) -> Option<String> {
    Some(format!("own-{name}"))
}

/// `fut`, or a failure of the test after twenty real seconds: a node that retries a
/// refusal waits for ever on a loopback frontend, which a hang would not report.
async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
    let patience = Duration::from_secs(20);
    tokio::time::timeout(patience, fut)
        .await
        .expect("the start took too long")
}

async fn s3() -> FakeS3 {
    let s3 = FakeS3::start().await;
    s3.require_credentials(ID, SECRET);
    s3
}

/// Whether the store takes a write signed with the registered pair (and so is the
/// registered store, not the node's own, whose pair the service refuses).
async fn takes_registered_writes(store: &S3Store, name: &str) -> bool {
    let key = ObjectKey::new(format!("kbf/cas/{name}")).expect("a key");
    let range = ByteRange::new(0, 1).expect("a range");
    store
        .put_new(&key, bytes::Bytes::from_static(b"x"), None)
        .await
        .is_ok()
        && store.get_range(&key, range).await.is_ok()
}

/// Catches a node that opens its cache on the registered keys without having probed
/// them (defect: the probe left out of `registered_store`), reports a generation other
/// than the snapshot's (defect: the announced one), or keeps its own store (defect:
/// `registered` returns `self.own`): the returned store takes writes signed with the
/// registered pair and the node says it runs on it.
#[tokio::test]
async fn registered_keys_that_pass_the_probe_are_the_store_the_node_runs_on() {
    let s3 = s3().await;
    let joiner = answering(Ok(registered(&s3, 7, SECRET))).await;
    for keys in [false, true] {
        let (store, applied) = within(flagged(&s3, keys).registered(&joiner, &joined(6)))
            .await
            .expect("opens on the registration");
        assert_eq!(applied.for_beat(7), (7, ApplyStatus::Ok), "own keys {keys}");
        assert!(
            takes_registered_writes(&store, &format!("keys-{keys}")).await,
            "own keys {keys}"
        );
    }
}

/// Catches a registration whose put settings do not reach the store the node runs on
/// (defects: `conditional_put` taken from the flag, `false`, or negated; `min_put_rate`
/// taken from the flag): for each setting of the registration, with the flag set the
/// other way, the address the store is opened on carries the registration's, and a rate
/// of 0 (not set) falls back to the flag's.
#[test]
fn the_registered_put_settings_are_the_stores_not_the_flags() {
    for registered in [true, false] {
        let mut config = config("bkt", "kbf/", "http://127.0.0.1:9");
        config.conditional_put = registered;
        config.min_put_rate = 7;
        let mut flag = uri("bkt", "kbf/", "http://127.0.0.1:9");
        flag.conditional_put = !registered;
        flag.min_put_rate = NonZeroU64::new(3).expect("non-zero");
        let opened = registered_uri(&flag, &config);
        assert_eq!(opened.conditional_put, registered);
        assert_eq!(opened.min_put_rate.get(), 7);
        config.min_put_rate = 0;
        assert_eq!(registered_uri(&flag, &config).min_put_rate.get(), 3);
        assert_eq!(opened.region, "eu-west-1", "the registration's region");
    }
}

/// Catches the same defect one step on, where it matters (defects as above, in
/// `registered_store`): a registration that says conditional puts gives a store that
/// refuses a second `put_new` of a key, and one that says not gives a store that
/// replaces it, whatever the node's flag says.
#[tokio::test]
async fn the_registered_store_puts_as_the_registration_says() {
    let s3 = s3().await;
    for conditional in [true, false] {
        let mut answer = registered(&s3, 7, SECRET);
        answer.config.as_mut().expect("a config").conditional_put = conditional;
        let joiner = answering(Ok(answer)).await;
        let mut flag = uri("bkt", "kbf/", s3.endpoint());
        flag.conditional_put = !conditional;
        let node = Registrable::new(&flag, None, no_keys).expect("a store");
        let (store, _) = within(node.registered(&joiner, &joined(7)))
            .await
            .expect("opens on the registration");
        assert_eq!(store.capabilities().conditional_put, conditional);
        let key = ObjectKey::new(format!("kbf/cas/put-{conditional}")).expect("a key");
        let put = || store.put_new(&key, bytes::Bytes::from_static(b"x"), None);
        put().await.expect("the first put");
        assert_eq!(
            matches!(put().await, Err(ObjectStoreError::AlreadyExists(_))),
            conditional,
            "conditional {conditional}"
        );
    }
}

/// Catches keys that fail the probe being used or reported ok (defect: the failure arm
/// returns the registered store; `Applied::running`): a node with a pair of its own keeps
/// that store, which the service refuses here, and says `probe_failed` with nothing
/// applied; one without ends with the probe's step.
#[tokio::test]
async fn registered_keys_that_fail_the_probe_leave_the_node_on_its_own_or_stop_it() {
    let s3 = s3().await;
    let joiner = answering(Ok(registered(&s3, 7, "wrong-secret"))).await;
    let (store, applied) = within(flagged(&s3, true).registered(&joiner, &joined(7)))
        .await
        .expect("keeps its own store");
    assert_eq!(applied.for_beat(7), (0, ApplyStatus::ProbeFailed));
    assert!(
        !takes_registered_writes(&store, "own").await,
        "the node's own pair is refused here"
    );
    let Err(error) = within(flagged(&s3, false).registered(&joiner, &joined(7))).await else {
        panic!("a node with nothing that works starts");
    };
    assert!(matches!(
        error.downcast_ref::<ColdApplyError>(),
        Some(ColdApplyError::Probe(ProbeStep::Put))
    ));
}

/// Catches a registration for another store being opened (defect: `same_store` true):
/// the node keeps its flag's store, says `identity_mismatch`, and with no pair of its own
/// has nothing to run on.
#[tokio::test]
async fn a_registration_for_another_bucket_leaves_the_node_on_its_flag_or_stops_it() {
    let s3 = s3().await;
    let mut other = registered(&s3, 7, SECRET);
    other.config.as_mut().expect("a config").bucket = "other".to_owned();
    let joiner = answering(Ok(other)).await;
    let (_store, applied) = within(flagged(&s3, true).registered(&joiner, &joined(7)))
        .await
        .expect("keeps its flag's store");
    assert_eq!(applied.for_beat(7), (0, ApplyStatus::IdentityMismatch));
    let Err(error) = within(flagged(&s3, false).registered(&joiner, &joined(7))).await else {
        panic!("a node with nothing to run on starts");
    };
    assert!(matches!(
        error.downcast_ref::<ColdApplyError>(),
        Some(ColdApplyError::MismatchNoKeys)
    ));
    assert!(
        s3.requests().is_empty(),
        "the other store was never touched"
    );
}

/// Catches a registration taken back before the fetch being treated as one (defect: the
/// empty answer opens the registered store): the node runs on its flags and says it
/// applied none of the announced generation; with no pair of its own it says which
/// variable is missing.
#[tokio::test]
async fn a_registration_taken_back_leaves_the_node_on_its_flag_or_stops_it() {
    let s3 = s3().await;
    let joiner = answering(Ok(answer(0))).await;
    let (_store, applied) = within(flagged(&s3, true).registered(&joiner, &joined(7)))
        .await
        .expect("keeps its flag's store");
    assert_eq!(applied.for_beat(7), (0, ApplyStatus::Pending));
    // The frontend's own word is gone with the registration: nothing to apply.
    assert_eq!(applied.for_beat(0), (0, ApplyStatus::Ok));
    let Err(error) = within(flagged(&s3, false).registered(&joiner, &joined(7))).await else {
        panic!("a node with nothing starts");
    };
    assert!(matches!(
        error.downcast_ref::<ColdApplyError>(),
        Some(ColdApplyError::MissingKeys("AWS_ACCESS_KEY_ID"))
    ));
}

/// Catches a node denied by the frontend retrying for ever, or starting as if it had the
/// registration (defect: a refusal treated as a retry): with a pair of its own it starts
/// on that and says `pending`; without one the refusal ends the start.
#[tokio::test]
async fn a_frontend_that_refuses_the_node_leaves_it_on_its_own_keys_or_stops_it() {
    let s3 = s3().await;
    let joiner = answering(Err(Code::PermissionDenied)).await;
    let (_store, applied) = within(flagged(&s3, true).registered(&joiner, &joined(7)))
        .await
        .expect("keeps its flag's store");
    assert_eq!(applied.for_beat(7), (0, ApplyStatus::Pending));
    let Err(error) = within(flagged(&s3, false).registered(&joiner, &joined(7))).await else {
        panic!("a refused node with nothing starts");
    };
    assert!(matches!(
        error.downcast_ref::<ColdApplyError>(),
        Some(ColdApplyError::Refused(Code::PermissionDenied))
    ));
}
