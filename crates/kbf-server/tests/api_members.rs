//! `GET /v1/members` over a real listener: the storage members of the current map, read
//! from the same `Members` the `Membership` service registers into.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/looks.rs"]
mod looks;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{MemberId, MemberState};
use kbf_objstore::MemoryStore;
use kbf_server::members::{
    Fill, Members, MembershipConfig, NodeFacts, PROTOCOL, Progress, Registrant,
};
use kbf_server::storage_health::Reported;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use prost::Message as _;
use serde_json::{Value, json};
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TTL: Duration = Duration::from_secs(30);
const DWELL: Duration = Duration::from_secs(120);

/// The tick of every server here: one that does not come within a test. A server task that
/// looks at every tick would meet the faults that a test arms for a request (`support/looks.rs`
/// says why). No test here waits for a tick.
const TICK: Duration = Duration::from_secs(3600);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: TTL,
        dwell: DWELL,
        replication: 1,
    }
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: TICK,
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        scheduler_core: kbf_server::SchedulerCore::under_test(),
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        usage_profiles: kbf_server::UsageProfiles::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// A server with the operator API, writes off (no token), over `state` if given.
struct Server {
    api: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start(state: Option<StateStore>) -> Server {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        state,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
        stop: Some(stop),
    }
}

/// A store on a clock the test moves, and the state a server over it is given.
fn store() -> (Arc<ManualClock>, Arc<Fake>, StateStore) {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let state = StateStore {
        store: Arc::clone(&fake) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    (clock, fake, state)
}

/// Another frontend over the same store: what storage nodes register with.
fn other_frontend(fake: &Arc<Fake>) -> Members {
    Members::new(Arc::clone(fake) as Arc<dyn Store>, config())
}

fn registrant(i: u32, weight: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// One HTTP/1.1 request; the status code, the response head and the body.
async fn http(api: SocketAddr, method: &str, path: &str, headers: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n\
         {headers}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, head.to_owned(), body.to_owned())
}

/// The view before the first member has registered: no map, so no version known and none
/// unknown.
fn empty() -> Value {
    json!({ "generation": 0, "members": [], "oldest_protocol": null, "protocol_unknown": 0 })
}

async fn members(api: SocketAddr) -> Value {
    let (code, head, body) = http(api, "GET", "/v1/members", "").await;
    assert_eq!(code, 200, "{body}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{head}"
    );
    serde_json::from_str(&body).expect("a JSON body")
}

/// Catches a members view that invents a map before the first member registers (the
/// script that waits for "active" must see an empty list, not an error), and one that
/// answers a server with no state store as if it had an empty farm: that server keeps
/// no members at all, and says so.
#[tokio::test]
async fn no_map_is_empty_and_no_state_store_is_not_found() {
    let (_clock, _fake, state) = store();
    let with = start(Some(state));
    assert_eq!(members(with.api).await, empty());

    let without = start(None);
    let (code, _, body) = http(without.api, "GET", "/v1/members", "").await;
    assert_eq!(code, 404, "{body}");
    let why = serde_json::from_str::<Value>(&body).expect("JSON")["error"]
        .as_str()
        .expect("an error")
        .to_owned();
    assert!(why.contains("--state-store"), "{why}");
}

/// Catches a view that reads another source than the members the `Membership` service
/// writes (a registered member must appear, with the id the registration answered), one
/// that drops or renames a field (`id`, `addr`, `state`, `weight`, `generation`, and the
/// node facts, which read `null` for a node that said none: an object with a key missing
/// is another shape than one with it `null`), and one
/// that names a state other than the map's: a member the operator set `joining` is
/// `joining` until promoted, and `active` after (a registration that is caught up is
/// `active` at once).
#[tokio::test]
async fn a_registered_member_is_listed_and_active_once_promoted() {
    let (_clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    let a = frontend.register(&registrant(1, 1)).expect("a");
    let b = frontend.register(&registrant(2, 3)).expect("b");
    let registered = members(server.api).await;
    assert_eq!(registered["members"][0]["state"], json!("active"));
    assert_eq!(registered["members"][1]["state"], json!("active"));
    frontend.set_state(a.id, MemberState::Joining).expect("a");
    let b_map = frontend.set_state(b.id, MemberState::Joining).expect("b");

    let joined = members(server.api).await;
    assert_eq!(joined["generation"], json!(b_map.generation()));
    assert_eq!(
        joined["members"],
        json!([
            { "id": a.id.0, "addr": "192.0.2.1:8982", "state": "joining", "weight": 1,
              "protocol": PROTOCOL, "runs_log": null, "log_version": null,
              "build_commit": null },
            { "id": b.id.0, "addr": "192.0.2.2:8982", "state": "joining", "weight": 3,
              "protocol": PROTOCOL, "runs_log": null, "log_version": null,
              "build_commit": null },
        ])
    );

    let promoted = frontend
        .set_state(a.id, MemberState::Active)
        .expect("promoted");
    let after = members(server.api).await;
    assert_eq!(after["generation"], json!(promoted.generation()));
    assert_eq!(after["members"][0]["state"], json!("active"));
    assert_eq!(after["members"][1]["state"], json!("joining"));
}

/// Catches a state spelled other than the map's five (`catching_up` and `draining` are
/// the ones a hand-written mapping gets wrong), and a Down member dropped from the view
/// at once: it is in the map for the dwell, and leaves only when the map drops it.
#[tokio::test]
async fn a_draining_or_expired_member_shows_its_state_until_it_leaves() {
    let (clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    let a = frontend.register(&registrant(1, 1)).expect("a");
    let b = frontend.register(&registrant(2, 1)).expect("b");
    let c = frontend.register(&registrant(3, 1)).expect("c");
    frontend.set_state(c.id, MemberState::Joining).expect("c");

    let state_of = |view: &Value, id: MemberId| -> Option<String> {
        view["members"]
            .as_array()
            .expect("a list")
            .iter()
            .find(|m| m["id"] == json!(id.0))
            .map(|m| m["state"].as_str().expect("a state").to_owned())
    };

    frontend
        .set_state(a.id, MemberState::CatchingUp)
        .expect("a");
    frontend.set_state(b.id, MemberState::Draining).expect("b");
    let view = members(server.api).await;
    assert_eq!(state_of(&view, a.id).as_deref(), Some("catching_up"));
    assert_eq!(state_of(&view, b.id).as_deref(), Some("draining"));
    assert_eq!(state_of(&view, c.id).as_deref(), Some("joining"));

    // Only `a` and `b` keep beating: `c` lapses, and is Down in the map.
    clock.advance(TTL - Duration::from_secs(1));
    frontend
        .heartbeat(
            a.id,
            a.boot,
            Progress::CatchingUp,
            Reported::default(),
            None,
        )
        .expect("beat a");
    frontend
        .heartbeat(b.id, b.boot, Progress::CaughtUp, Reported::default(), None)
        .expect("beat b");
    clock.advance(Duration::from_secs(2));
    frontend.refresh().expect("refreshed");
    let view = members(server.api).await;
    assert_eq!(state_of(&view, c.id).as_deref(), Some("down"));
    assert_eq!(state_of(&view, a.id).as_deref(), Some("catching_up"));

    // Past the dwell the map drops it, and so does the view.
    clock.advance(DWELL + Duration::from_secs(1));
    frontend.refresh().expect("refreshed");
    let view = members(server.api).await;
    assert_eq!(state_of(&view, c.id), None, "{view}");
}

/// Storage node `i` whose newest protocol is `protocol`. Only the oldest end of a build's
/// range judges a registration, so a node newer than the frontend's own constant is
/// accepted, and the versions a test registers can differ from it and from each other.
fn speaking(i: u32, protocol: u32) -> Registrant {
    Registrant {
        protocol,
        ..registrant(i, 1)
    }
}

/// What each member's entry says of its protocol, in the map's order.
fn protocols_of(view: &Value) -> Value {
    let members = view["members"].as_array().expect("a list");
    Value::Array(members.iter().map(|m| m["protocol"].clone()).collect())
}

/// Catches: a registration's protocol not written to the row (every member would show
/// none), shown for another member, or shown as the frontend's own constant; a heartbeat
/// or a state change that rewrites the row without it; a new registration of the same
/// member that does not replace it (a node restarted on a newer build keeps its old
/// number); the envelope's oldest taken as the greatest, the first or the last; and the
/// envelope fields nested under a key of their own. The least is the middle member's.
#[tokio::test]
async fn a_member_lists_the_protocol_it_registered_naming() {
    let (_clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    frontend.register(&speaking(1, 3)).expect("a");
    let b = frontend.register(&speaking(2, 2)).expect("b");
    frontend.register(&speaking(3, 5)).expect("c");
    let view = members(server.api).await;
    assert_eq!(protocols_of(&view), json!([3, 2, 5]), "{view}");
    assert_eq!(view["oldest_protocol"], json!(2), "{view}");
    assert_eq!(view["protocol_unknown"], json!(0), "{view}");

    // A heartbeat and an administrator's state change rewrite the row, and keep it.
    frontend
        .heartbeat(b.id, b.boot, Progress::CaughtUp, Reported::default(), None)
        .expect("beat");
    frontend.set_state(b.id, MemberState::Draining).expect("b");
    let view = members(server.api).await;
    assert_eq!(protocols_of(&view), json!([3, 2, 5]), "{view}");

    // The node restarts on a newer build: its registration replaces the number.
    let again = frontend.register(&speaking(2, 4)).expect("b again");
    assert_eq!((again.id, again.boot > b.boot), (b.id, true));
    let view = members(server.api).await;
    assert_eq!(protocols_of(&view), json!([3, 4, 5]), "{view}");
    assert_eq!(view["oldest_protocol"], json!(3), "{view}");
}

/// A member row as a build that predates the protocol field wrote it: the same fields
/// without tag 4.
#[derive(Clone, PartialEq, prost::Message)]
struct RowBeforeProtocol {
    #[prost(message, optional, tag = "1")]
    member: Option<kbf_proto::kbf::map::v1::Member>,
    #[prost(uint64, tag = "2")]
    boot: u64,
    /// The debts report, whose layout this test does not read.
    #[prost(bytes = "vec", optional, tag = "3")]
    report: Option<Vec<u8>>,
}

/// Catches: a row written before the field existed shown as a node that speaks version 0
/// (it would be the oldest, below every real version, and not counted), shown as the
/// frontend's own version, left out of the unknown count, or counted as known; and a
/// fixture that is not the old layout (the row must differ from the one a registration
/// writes now, or the test proves nothing).
#[tokio::test]
async fn a_row_written_before_the_field_is_unknown_and_never_the_oldest() {
    let (_clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    frontend.register(&speaking(1, 3)).expect("a");
    let b = frontend.register(&speaking(2, 2)).expect("b");
    frontend.register(&speaking(3, 5)).expect("c");

    let key = format!("member/{}", b.id.0);
    let row = fake.get(&key).expect("store").expect("a live row");
    let old = RowBeforeProtocol::decode(row.value.as_slice()).expect("a row");
    assert_ne!(old.encode_to_vec(), row.value, "the field is on the row");
    fake.txn(Txn::new().put_ttl(&key, old.encode_to_vec(), TTL, Cond::Version(row.version)))
        .expect("store")
        .expect("no conflict");

    let view = members(server.api).await;
    assert_eq!(protocols_of(&view), json!([3, null, 5]), "{view}");
    assert_eq!(view["oldest_protocol"], json!(3), "{view}");
    assert_eq!(view["protocol_unknown"], json!(1), "{view}");
}

/// Catches: the protocol moved to another tag, or to another wire type, of the stored
/// row. The rows live in the store across builds, so the number one build writes is the
/// number the next reads; only bytes spelled out by hand pin that, because a test that
/// writes and reads through the same `Row` follows the tag wherever it goes (a row
/// written at tag 5 round-trips, and reads as unknown to every build that has tag 4).
/// The protocol is field 4 as a plain varint: the key byte is `4 << 3 | 0`, 0x20.
#[test]
fn the_protocol_is_field_four_of_the_stored_row_as_a_plain_varint() {
    let (_clock, fake, _state) = store();
    let frontend = other_frontend(&fake);
    frontend.register(&speaking(1, 3)).expect("a");
    let b = frontend.register(&speaking(2, 2)).expect("b");
    let key = format!("member/{}", b.id.0);
    let row = fake.get(&key).expect("store").expect("a live row");
    let old = RowBeforeProtocol::decode(row.value.as_slice()).expect("a row");

    // What a registration writes: the fields of the old layout, then field 4 holding 2.
    let mut written = old.encode_to_vec();
    written.extend_from_slice(&[0x20, 2]);
    assert_eq!(row.value, written, "the protocol is not field 4 of the row");

    // What a row with field 4 holding 5 is read as, whatever wrote it.
    let mut by_hand = old.encode_to_vec();
    by_hand.extend_from_slice(&[0x20, 5]);
    fake.txn(Txn::new().put_ttl(&key, by_hand, TTL, Cond::Version(row.version)))
        .expect("store")
        .expect("no conflict");
    let read = frontend.protocols().expect("rows");
    assert_eq!(read.get(&b.id), Some(&Some(5)), "{read:?}");
}

/// Catches: the end of an operator's planned restart moved to another tag of the stored
/// row. A plan lives only in the row (`planned_until_nanos`), so a build that reads it from
/// another tag than the build that wrote it loses every plan across a frontend restart,
/// and the node is then judged as not planned down. Bytes spelled out by hand, for the
/// reason of the test above: the planned end is field 5 as a plain varint, key byte
/// `5 << 3 | 0`, 0x28, after the fields of the old layout and the protocol (field 4, 0x20).
#[test]
fn the_planned_end_is_field_five_of_the_stored_row_as_a_plain_varint() {
    let (_clock, fake, _state) = store();
    let frontend = other_frontend(&fake);
    frontend.register(&speaking(1, 3)).expect("a");
    let b = frontend.register(&speaking(2, 2)).expect("b");
    let key = format!("member/{}", b.id.0);
    let t0 = fake.now().as_duration().as_nanos();
    frontend
        .plan_down(b.id, Duration::from_secs(600))
        .expect("plan");
    let t1 = fake.now().as_duration().as_nanos();

    let row = fake.get(&key).expect("store").expect("a live row");
    let decoded = RowBeforeProtocol::decode(row.value.as_slice()).expect("a row");
    // The fields before the plan (the member, now Down) as the old layout reads them, then
    // the protocol (field 4) and the key byte of field 5, written by hand.
    let mut prefix = decoded.encode_to_vec();
    prefix.extend_from_slice(&[0x20, 2, 0x28]);
    assert!(
        row.value.starts_with(&prefix),
        "the planned end is not field 5 of the row: {:?}",
        row.value
    );
    // The rest is exactly one plain varint: the end, 600 s after the clock at the call.
    let tail = &row.value[prefix.len()..];
    let (last, init) = tail.split_last().expect("a varint");
    assert!(*last < 0x80 && init.iter().all(|b| *b >= 0x80), "{tail:?}");
    let end = tail
        .iter()
        .rev()
        .fold(0u128, |n, b| (n << 7) | u128::from(b & 0x7f));
    assert!(
        (t0 + 600_000_000_000..=t1 + 600_000_000_000).contains(&end),
        "{end} is not 600 s after {t0}..{t1}"
    );
}

/// Catches: a stored member row that does not decode skipped by the view, which would
/// list the members and name every version it could read as if the farm were whole. A
/// row the other readers of the rows fail on is wrong content in a store that is up:
/// the whole view is `500`, as for a map that does not decode, and the one reading it
/// is `200` again once the row is right.
#[tokio::test]
async fn a_member_row_that_does_not_decode_is_500_for_the_whole_view() {
    let (_clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    frontend.register(&speaking(1, 3)).expect("a");
    let b = frontend.register(&speaking(2, 2)).expect("b");
    assert_eq!(
        members(server.api).await["members"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    let key = format!("member/{}", b.id.0);
    let row = fake.get(&key).expect("store").expect("a live row");
    fake.txn(Txn::new().put_ttl(&key, b"not a row".to_vec(), TTL, Cond::Version(row.version)))
        .expect("store")
        .expect("no conflict");
    let (code, _, body) = http(server.api, "GET", "/v1/members", "").await;
    assert_eq!(code, 500, "{body}");
    let why = serde_json::from_str::<Value>(&body).expect("JSON")["error"]
        .as_str()
        .expect("an error")
        .to_owned();
    assert!(why.contains("member row"), "{why}");

    let fixed = fake.get(&key).expect("store").expect("the bad row");
    fake.txn(Txn::new().put_ttl(&key, row.value, TTL, Cond::Version(fixed.version)))
        .expect("store")
        .expect("no conflict");
    let view = members(server.api).await;
    assert_eq!(protocols_of(&view), json!([3, 2]), "{view}");
}

/// Catches: a member the map still holds as `down`, whose row has expired with its
/// heartbeats, shown with a number it no longer has (a default, or the frontend's own),
/// or left out of the unknown count: it is in the map for the dwell and may come back.
#[tokio::test]
async fn a_member_whose_row_lapsed_names_no_protocol() {
    let (clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    let a = frontend.register(&speaking(1, 3)).expect("a");
    let b = frontend.register(&speaking(2, 2)).expect("b");
    frontend.register(&speaking(3, 1)).expect("c");

    // Only `a` and `b` keep beating: `c` lapses and is `down` in the map.
    clock.advance(TTL - Duration::from_secs(1));
    for registered in [&a, &b] {
        frontend
            .heartbeat(
                registered.id,
                registered.boot,
                Progress::CaughtUp,
                Reported::default(),
                None,
            )
            .expect("beat");
    }
    clock.advance(Duration::from_secs(2));
    frontend.refresh().expect("refreshed");
    let view = members(server.api).await;
    assert_eq!(view["members"][2]["state"], json!("down"), "{view}");
    assert_eq!(protocols_of(&view), json!([3, 2, null]), "{view}");
    assert_eq!(view["oldest_protocol"], json!(2), "{view}");
    assert_eq!(view["protocol_unknown"], json!(1), "{view}");
}

/// Full commits, as a node built from a git checkout reports them.
const COMMIT_A: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
const COMMIT_C: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";
const COMMIT_Z: &str = "ffffffffffffffffffffffffffffffffffffffff";

/// What the entry of member `i` (in the map's order) says of its build and log:
/// `[runs_log, log_version, build_commit]`.
fn facts_of(view: &Value, i: usize) -> Value {
    let m = &view["members"][i];
    json!([m["runs_log"], m["log_version"], m["build_commit"]])
}

/// Catches: the node facts missing from the view, shown for another member, swapped with
/// one another (`runs_log` for `log_version`), or shown as 0 or "" for a node that said
/// none (an older build must read `null`); a heartbeat's log version not reaching the view,
/// or its `runs_log` and `build_commit` replacing the registration's; and a `down` member,
/// whose row lapsed with its heartbeats, shown with facts it no longer has.
#[tokio::test]
async fn a_member_lists_what_it_runs_and_where_its_log_is() {
    let (clock, fake, state) = store();
    let server = start(Some(state));
    let frontend = other_frontend(&fake);
    let said = |runs_log, log_version, commit: &str| NodeFacts {
        runs_log: Some(runs_log),
        log_version: Some(log_version),
        build_commit: Some(commit.to_owned()),
    };
    let a = frontend
        .register_reporting(&registrant(1, 1), None, &said(5, 1, COMMIT_A))
        .expect("a");
    let b = frontend.register(&registrant(2, 1)).expect("b");
    frontend
        .register_reporting(&registrant(3, 1), None, &said(6, 2, COMMIT_C))
        .expect("c");
    let view = members(server.api).await;
    assert_eq!(facts_of(&view, 0), json!([5, 1, COMMIT_A]), "{view}");
    assert_eq!(facts_of(&view, 1), json!([null, null, null]), "{view}");
    assert_eq!(facts_of(&view, 2), json!([6, 2, COMMIT_C]), "{view}");

    // A heartbeat brings the log version the log was raised to, and nothing else.
    let beat = said(9, 4, COMMIT_Z);
    frontend
        .heartbeat_reporting(
            a.id,
            a.boot,
            Progress::CaughtUp,
            Reported::default(),
            (Fill::default(), &beat),
            None,
        )
        .expect("beat a");
    let view = members(server.api).await;
    assert_eq!(facts_of(&view, 0), json!([5, 4, COMMIT_A]), "{view}");

    // Only `a` and `b` keep beating: `c` lapses and is `down` in the map, with no facts.
    clock.advance(TTL - Duration::from_secs(1));
    for registered in [&a, &b] {
        frontend
            .heartbeat(
                registered.id,
                registered.boot,
                Progress::CaughtUp,
                Reported::default(),
                None,
            )
            .expect("beat");
    }
    clock.advance(Duration::from_secs(2));
    frontend.refresh().expect("refreshed");
    let view = members(server.api).await;
    assert_eq!(view["members"][2]["state"], json!("down"), "{view}");
    assert_eq!(facts_of(&view, 2), json!([null, null, null]), "{view}");
}

/// Catches `/v1/members` behind a gate `/v1/nodes` does not have (the operator scripts
/// call both the same way), and a write verb answered on a read path. The server has no
/// token, so a read that needed one could not succeed.
#[tokio::test]
async fn members_is_read_like_nodes_and_takes_no_write() {
    let (_clock, _fake, state) = store();
    let server = start(Some(state));
    let (nodes, _, _) = http(server.api, "GET", "/v1/nodes", "").await;
    let (members, _, _) = http(server.api, "GET", "/v1/members", "").await;
    assert_eq!((nodes, members), (200, 200));
    let (code, _, _) = http(
        server.api,
        "POST",
        "/v1/members",
        "Content-Type: application/json\r\n",
    )
    .await;
    assert_eq!(code, 405);
}

/// The reads of `(update/desired, deny/)` the server makes by itself as it starts, with no
/// release store: nothing reads `update/desired`, and `deny/` is read twice: by the refresh the
/// start makes before it serves, and by the sweeper's first refresh ([`looks::Looks`]). A test that arms a fault first
/// waits for them; the test at the end holds the numbers.
const START_LOOKS: (usize, usize) = (0, 2);

/// A server whose state store is the hooked fake, which the test makes fail.
fn failing(hooked: &Arc<Hooked>) -> Server {
    let state = StateStore {
        store: Arc::clone(hooked) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    start(Some(state))
}

/// The error `GET /v1/members` answers once `arm` has set a fault. The fault is for the
/// request: the server's own readers of the store have read all they do at start, which
/// the caller has waited for (`START_LOOKS`), and not again within the test (`TICK` and the
/// sweeper's beat).
async fn error_of(api: SocketAddr, arm: impl Fn()) -> (u16, String) {
    arm();
    let (code, _, body) = http(api, "GET", "/v1/members", "").await;
    assert_ne!(code, 200, "the fault never reached the request: {body}");
    let why = serde_json::from_str::<Value>(&body).expect("JSON")["error"]
        .as_str()
        .expect("an error")
        .to_owned();
    (code, why)
}

/// Catches a store that cannot be read answered as an empty farm or as a server bug:
/// it is `503` (try again), a stored map that does not decode is `500` (the store is
/// up and its content is wrong), and a store that panics inside the blocking task is
/// `500` naming the task, not a dropped connection. Each one recovers once the fault
/// is gone.
#[tokio::test]
async fn a_store_that_fails_is_503_and_a_map_that_is_wrong_or_a_panic_is_500() {
    let hooked = Hooked::new();
    let looks = looks::Looks::count(&hooked);
    let server = failing(&hooked);
    looks.done(START_LOOKS).await;

    let (code, why) = error_of(server.api, || {
        hooked
            .fail_gets
            .store(1, std::sync::atomic::Ordering::SeqCst);
    })
    .await;
    assert_eq!(code, 503, "{why}");
    assert!(why.contains("injected"), "{why}");
    assert_eq!(members(server.api).await, empty());

    let (code, why) = error_of(server.api, || {
        hooked
            .panic_gets
            .store(1, std::sync::atomic::Ordering::SeqCst);
    })
    .await;
    assert_eq!(code, 500, "{why}");
    assert!(why.contains("membership task"), "{why}");

    hooked
        .fake
        .txn(Txn::new().put("map", b"not a map".to_vec(), Cond::Absent))
        .expect("store")
        .expect("no conflict");
    let (code, why) = error_of(server.api, || ()).await;
    assert_eq!(code, 500, "{why}");
}

/// Catches the wait of `Looks::done` being wrong, which the failing-store test above cannot
/// show by itself (it would flake, or hang): the server's own reads of `update/desired` and
/// `deny/` are `START_LOOKS` at start, and it reads nothing at all by itself after the wait,
/// for as long as a test runs. A reader added beside the sweeper, a read
/// added to its look, or a tick that comes, makes the total more, and a fault armed after
/// the wait would be met by one of them and not by its request; a read fewer makes a count
/// less, and the wait never ends.
///
/// A `START_LOOKS` above the real counts never ends the wait; one below them lets a read
/// through after it.
#[tokio::test]
async fn the_server_reads_the_store_by_itself_only_at_start() {
    let hooked = Hooked::new();
    let looks = looks::Looks::count(&hooked);
    let every_read = hooked.count("");
    let _server = failing(&hooked);
    looks.done(START_LOOKS).await;
    let at_start = every_read.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(looks.seen(), START_LOOKS);
    assert_eq!(every_read.load(Ordering::SeqCst), at_start);
}

/// Catches: a store written by an older build, which holds `update/*` rows of every kind it
/// wrote (the run and its counter, the policy, hosts, the places in use, an order counter and
/// a proposal), that the server cannot start on or reads and changes: the server starts over
/// it, answers, and leaves every update row exactly as it was. The routes that read the run
/// are gone, so they answer 404 rather than from the rows.
#[tokio::test]
async fn a_store_holding_old_update_rows_starts_serves_and_leaves_them_alone() {
    let hooked = Hooked::new();
    let old: [(&str, &[u8]); 8] = [
        ("update/desired", &[0x08, 0x01, 0x12, 0x02, 0x76, 0x32]),
        ("update/generation", &[0x03]),
        ("update/order/0000000000000001", &[]),
        (
            "update/proposed/0123456789abcdef0123456789abcdef01234567",
            &[0x0a, 0x02, 0x76, 0x32],
        ),
        ("update/policy", &[0x08, 0x01, 0x20, 0x02]),
        (
            "update/slot",
            &[0x0a, 0x08, 0x0a, 0x02, 0x73, 0x31, 0x10, 0x05, 0x18, 0x01],
        ),
        ("update/host/s1", &[0x08, 0x0d, 0x10, 0x09, 0x30, 0x07]),
        ("update/host/d1", &[0x08, 0x01]),
    ];
    for (key, bytes) in old {
        hooked
            .fake
            .txn(Txn::new().put(key, bytes.to_vec(), Cond::Absent))
            .expect("store")
            .expect("a new row");
    }
    let looks = looks::Looks::count(&hooked);
    let server = failing(&hooked);
    looks.done(START_LOOKS).await;
    assert_eq!(members(server.api).await, empty());
    for gone in ["/v1/update", "/v1/update/desired", "/v1/update/pause"] {
        let (code, _, body) = http(server.api, "GET", gone, "").await;
        assert_eq!(code, 404, "{gone}: {body}");
    }
    let mut held: Vec<(String, Vec<u8>)> = hooked
        .fake
        .scan_prefix("update/")
        .expect("scan")
        .into_iter()
        .map(|(key, row)| (key, row.value))
        .collect();
    let mut want: Vec<(String, Vec<u8>)> = old
        .iter()
        .map(|(key, bytes)| ((*key).to_owned(), bytes.to_vec()))
        .collect();
    held.sort();
    want.sort();
    assert_eq!(held, want);
}
