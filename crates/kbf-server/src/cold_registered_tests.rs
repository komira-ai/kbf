//! How a node started with `--cold-store registered` waits for the registration: it asks
//! until a store is named, waits out a frontend that cannot answer, and stops at a refusal.
//! The clock is paused, so a wait is a turn of the runtime, never a delay of the test.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_proto::kbf::enroll::v1::{GetStoreConfigResponse, StoreConfig};
use tonic::{Code, Status};

use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore as _};
use tonic::transport::Endpoint;

use super::{ColdApplyError, ColdPick, Fetch, RegisteredOnly, registration_with};
use crate::join::{Identity, Joined, Joiner, RETRY_BASE};

fn named(generation: u64) -> GetStoreConfigResponse {
    GetStoreConfigResponse {
        generation,
        config: Some(StoreConfig {
            connection: "main".to_owned(),
            role: "cold".to_owned(),
            endpoint: "http://objects.example:9000".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "bkt".to_owned(),
            prefix: "kbf/".to_owned(),
            conditional_put: true,
            min_put_rate: 0,
            access_key_id: "AKIATEST".to_owned(),
            secret_access_key: "secret".to_owned(),
            secret_version: generation,
        }),
    }
}

fn nothing(generation: u64) -> GetStoreConfigResponse {
    GetStoreConfigResponse {
        generation,
        config: None,
    }
}

fn wait(_: u32) -> Duration {
    Duration::from_secs(1)
}

/// What the frontend answers to each call in turn, and how many calls it had.
fn frontend(
    answers: Vec<Result<GetStoreConfigResponse, Code>>,
) -> (Arc<Mutex<usize>>, impl FnMut() -> Fetch<'static>) {
    let calls = Arc::new(Mutex::new(0_usize));
    let counted = Arc::clone(&calls);
    let call = move || -> Fetch<'static> {
        let mut calls = counted.lock().unwrap_or_else(PoisonError::into_inner);
        let answer = answers
            .get(*calls)
            .or(answers.last())
            .cloned()
            .expect("an answer");
        *calls += 1;
        Box::pin(async move { answer.map_err(|code| Status::new(code, "canned")) })
    };
    (calls, call)
}

fn calls(counted: &Arc<Mutex<usize>>) -> usize {
    *counted.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Catches a node that starts on a registration that names nothing (defect: the first
/// answer is returned whatever it holds), one that stops asking (defect: an answer with no
/// store ends the wait), and one that reads a generation with no store as a registration
/// (a registration taken back is still announced by its generation): the answers are
/// "nothing", "generation 9 with no store", then a store; the node gets the third, with its
/// generation, after three calls.
#[tokio::test(start_paused = true)]
async fn a_registered_node_asks_until_a_store_is_named() {
    let (counted, mut call) = frontend(vec![Ok(nothing(0)), Ok(nothing(9)), Ok(named(10))]);
    let (generation, config) = registration_with(&mut call, &wait)
        .await
        .expect("a store is named at last");
    assert_eq!(generation, 10);
    assert_eq!(config.bucket, "bkt");
    assert_eq!(calls(&counted), 3);
}

/// Catches a node that gives up on a frontend that cannot answer (defect: the first failure
/// ends the wait), as a node that must have a store has no other way to run: three failures
/// the frontend's, then a store.
#[tokio::test(start_paused = true)]
async fn a_registered_node_waits_out_a_frontend_that_cannot_answer() {
    let down = || -> Result<GetStoreConfigResponse, Code> { Err(Code::Unavailable) };
    let answers = vec![down(), down(), Ok(nothing(0)), down(), Ok(named(2))];
    let (counted, mut call) = frontend(answers);
    let (generation, _) = registration_with(&mut call, &wait)
        .await
        .expect("answered at last");
    assert_eq!(generation, 2);
    assert_eq!(calls(&counted), 5);
}

/// Catches a node that asks for ever after the frontend refused it (defect: every code is
/// waited out), which would hold a denied node silent: one call, and the refusal's code.
#[tokio::test(start_paused = true)]
async fn a_refusal_ends_the_wait_for_a_registration() {
    let (counted, mut call) = frontend(vec![Err(Code::PermissionDenied)]);
    let refused = registration_with(&mut call, &wait)
        .await
        .expect_err("refused");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(calls(&counted), 1);
}

fn only(prefix: &str) -> RegisteredOnly {
    let mut config = named(3).config.expect("a store");
    prefix.clone_into(&mut config.prefix);
    RegisteredOnly {
        generation: 3,
        config,
    }
}

/// Catches a registered store that is run with no frontend (defect: `alone` opens a store):
/// it has none of its own, so it is refused, and says what it needs.
#[test]
fn a_registered_store_has_nothing_to_run_on_without_a_frontend() {
    let Err(refused) = only("kbf/").alone() else {
        panic!("a registered store opened with no frontend");
    };
    assert!(
        matches!(
            refused.downcast_ref::<ColdApplyError>(),
            Some(ColdApplyError::NoFrontend)
        ),
        "{refused}"
    );
}

/// Catches a log opened under another prefix than the registered one (defects: the prefix
/// taken as written, or the default for a registered one): the log's objects go under the
/// registered prefix as a store URI spells its own, ending in `/`, `kbf/` when empty.
#[test]
fn the_log_of_a_registered_node_keeps_its_objects_under_the_registered_prefix() {
    assert_eq!(only("farm-a/").prefix_root(), "farm-a/");
    assert_eq!(only("farm-a").prefix_root(), "farm-a/");
    assert_eq!(only("").prefix_root(), "kbf/");
}

/// Catches a node that exits on a frontend too old to serve the cold store (defect:
/// `Unimplemented` is fatal as it is for a node that has a store of its own): during a
/// rolling upgrade the frontend is updated first, so a node can meet one that does not know
/// `ColdStore` yet, and waiting is what `registered` means. The node waits through two such
/// answers and takes the store the third names.
#[tokio::test(start_paused = true)]
async fn a_frontend_too_old_for_the_cold_store_is_waited_for() {
    let old = || -> Result<GetStoreConfigResponse, Code> { Err(Code::Unimplemented) };
    let (counted, mut call) = frontend(vec![old(), old(), Ok(named(5))]);
    let (generation, config) = registration_with(&mut call, &wait)
        .await
        .expect("a store is named once the frontend can say");
    assert_eq!(generation, 5);
    assert_eq!(config.bucket, "bkt");
    assert_eq!(calls(&counted), 3);
}

/// Pins the refusals that waiting does not change, each ending the wait at the first call:
/// `PermissionDenied` and `Unauthenticated` (the node's certificate or standing: a later ask
/// is refused the same way and the operator must act), `InvalidArgument` and
/// `FailedPrecondition` (a request this build makes wrongly, the same on every try) and
/// `NotFound` (no such service on a frontend that answers). Only a frontend that is down or
/// busy, and one too old to know the service, is worth asking again.
#[tokio::test(start_paused = true)]
async fn the_refusals_that_waiting_does_not_change_still_end_the_wait() {
    for code in [
        Code::PermissionDenied,
        Code::Unauthenticated,
        Code::InvalidArgument,
        Code::FailedPrecondition,
        Code::NotFound,
    ] {
        let (counted, mut call) = frontend(vec![Err(code)]);
        let refused = registration_with(&mut call, &wait)
            .await
            .expect_err("refused");
        assert_eq!(refused.code(), code);
        assert_eq!(calls(&counted), 1, "{code:?}");
    }
}

const ID: &str = "AKIAOK";
const SECRET: &str = "right-secret";

/// A registration naming the store of `s3`, with the pair the service takes.
fn on(s3: &FakeS3) -> GetStoreConfigResponse {
    let mut answer = named(5);
    let Some(config) = answer.config.as_mut() else {
        panic!("named holds a store");
    };
    config.endpoint = s3.endpoint().to_owned();
    ID.clone_into(&mut config.access_key_id);
    SECRET.clone_into(&mut config.secret_access_key);
    answer
}

/// Catches a node that, after waiting out a frontend too old for the cold store, does not
/// run on the store it was then told (defect: the answer taken but the store not opened on
/// it): the node asks twice (`Unimplemented`, then a store), probes the registered keys, and
/// the store it holds takes writes signed with the registered pair, which the service
/// refuses any other.
#[tokio::test]
async fn after_an_old_frontend_the_node_runs_on_the_registered_store() {
    let s3 = FakeS3::start().await;
    s3.require_credentials(ID, SECRET);
    let old = || -> Result<GetStoreConfigResponse, Code> { Err(Code::Unimplemented) };
    let (_, mut call) = frontend(vec![old(), Ok(on(&s3))]);
    let (generation, config) = registration_with(&mut call, &|_| Duration::ZERO)
        .await
        .expect("a store is named");
    let only = RegisteredOnly { generation, config };
    let channel = Endpoint::from_static("http://frontend.invalid:8981").connect_lazy();
    let who = Identity {
        instance_id: "node".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let joiner = Joiner::new(channel, who, RETRY_BASE);
    let joined = Joined {
        member_id: 1,
        boot: 1,
        interval: Duration::from_secs(1),
        generation: 1,
        objstore_generation: 0,
    };
    let (store, applied) = only
        .registered(&joiner, &joined)
        .await
        .expect("opens on the registration");
    assert_eq!(
        applied.for_beat(5),
        (5, kbf_proto::kbf::map::v1::ApplyStatus::Ok)
    );
    let key = ObjectKey::new("kbf/cas/after-old".to_owned()).expect("a key");
    let range = ByteRange::new(0, 1).expect("a range");
    store
        .put_new(&key, bytes::Bytes::from_static(b"x"), None)
        .await
        .expect("a write signed with the registered pair");
    store.get_range(&key, range).await.expect("read back");
}
