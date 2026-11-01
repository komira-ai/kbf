//! `POST /v1/denied` and `DELETE /v1/denied?entry=...` over a real listener: an operator
//! adds an entry to the storage deny list or takes it off, through the `Members` the
//! `Membership` service registers into, behind the same gates as every other write. The
//! change is in the map when the call answers.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{DenyEntry, Map};
use kbf_objstore::MemoryStore;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::Store;
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const OPERATOR: &str = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\nContent-Type: application/json\r\n";

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 1,
    }
}

/// The token file is in a directory of its own, removed once the token is read.
fn token() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    let dir =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-api-denied");
    let path = dir.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
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
        tick: Duration::from_millis(50),
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
        token: Some(token()),
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

/// A server over a store with one registered member, and another frontend over the same
/// store to read what the server wrote.
fn served() -> (Server, Members) {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let other = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());
    other
        .register(&Registrant {
            instance_id: "node-1".to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: PROTOCOL,
            progress: Progress::CaughtUp,
        })
        .expect("register");
    (start(Some(state)), other)
}

fn map_of(members: &Members) -> Map {
    members.current_map().expect("read").expect("a map")
}

/// One HTTP/1.1 request; the status code and the body.
async fn request(
    api: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         {headers}Connection: close\r\n\r\n{body}"
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
    (code, body.to_owned())
}

fn deny_body(entry: &str, reason: &str) -> String {
    serde_json::json!({ "entry": entry, "reason": reason }).to_string()
}

/// Catches an operator call that answers 200 without reaching the store, one that leaves
/// the entry out of the map until the next beat (the answer must carry the generation of a
/// map that holds it), an entry stored as typed rather than canonically, a removal that
/// does not remove, and a second call that is not idempotent. The first member is
/// registered at generation 1: a `node` entry, then a `serial` typed with colons and
/// capitals, each turn the generation by one and are in the map read through another
/// frontend; the same entry again answers 200 and the same generation; a removal typed
/// differently from how it was added takes the serial off at a new generation, and a second
/// removal is 404.
///
/// Catches: the handlers not refreshing (the generation is the old one);
/// `DELETE` adding instead of removing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_denies_an_entry_and_takes_it_off_again() {
    let (server, other) = served();
    let g = map_of(&other).generation();
    let (code, body) = request(
        server.api,
        "POST",
        "/v1/denied",
        OPERATOR,
        &deny_body("node mac-07", "retired"),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let said: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        said,
        serde_json::json!({"entry": "node mac-07", "generation": g + 1})
    );
    assert_eq!(
        map_of(&other).denied(),
        [DenyEntry::Node("mac-07".to_owned())]
    );

    let (code, body) = request(
        server.api,
        "POST",
        "/v1/denied",
        OPERATOR,
        &deny_body("serial 0A:1B", "a lost laptop"),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let said: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        said,
        serde_json::json!({"entry": "serial a1b", "generation": g + 2})
    );
    assert_eq!(
        map_of(&other).denied(),
        [
            DenyEntry::Serial("a1b".to_owned()),
            DenyEntry::Node("mac-07".to_owned())
        ]
    );

    let (code, body) = request(
        server.api,
        "POST",
        "/v1/denied",
        OPERATOR,
        &deny_body("serial a1b", "again"),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains(&format!("\"generation\":{}", g + 2)),
        "{body}"
    );

    // The entry as a browser or curl would encode it: spaces as `+`, colons as `%3A`.
    let (code, body) = request(
        server.api,
        "DELETE",
        "/v1/denied?entry=serial+0A%3A1B",
        OPERATOR,
        "",
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains(&format!("\"generation\":{}", g + 3)),
        "{body}"
    );
    assert_eq!(
        map_of(&other).denied(),
        [DenyEntry::Node("mac-07".to_owned())]
    );
    let (code, body) = request(
        server.api,
        "DELETE",
        "/v1/denied?entry=serial%20a1b",
        OPERATOR,
        "",
    )
    .await;
    assert_eq!(code, 404, "{body}");
    assert_eq!(map_of(&other).generation(), g + 3);
}

/// Catches the deny list writable without the gates the other writes pass: no token, a
/// wrong token, a browser's `Origin`, and a body that is not JSON must each be refused on
/// `POST` and on `DELETE`, and nothing may be listed afterwards (or removed from a list that
/// has an entry).
///
/// Catches: the `gate` call removed from `deny_entry`; the same from `allow_entry`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deny_list_write_is_refused_without_the_gates() {
    let (server, other) = served();
    other
        .deny(&DenyEntry::Node("listed".to_owned()), "to be kept")
        .expect("deny");
    let wrong = "Authorization: Bearer not-the-token-not-the-token-000000000\r\n\
                 Content-Type: application/json\r\n";
    let origin = format!("{OPERATOR}Origin: http://evil.example\r\n");
    let plain = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\n";
    let body = deny_body("node mac-07", "retired");
    for (headers, refused) in [
        ("Content-Type: application/json\r\n", 401),
        (wrong, 401),
        (origin.as_str(), 403),
        (plain, 415),
    ] {
        let (code, text) = request(server.api, "POST", "/v1/denied", headers, &body).await;
        assert_eq!(code, refused, "POST {headers}: {text}");
        let (code, text) = request(
            server.api,
            "DELETE",
            "/v1/denied?entry=node%20listed",
            headers,
            "",
        )
        .await;
        assert_eq!(code, refused, "DELETE {headers}: {text}");
    }
    assert_eq!(
        other.denied().expect("list"),
        [DenyEntry::Node("listed".to_owned())]
    );
}

/// Catches a node id written as typed, when a certificate's DNS names are compared with it
/// up to ASCII case: `Mac-08` is stored, answered and put in the map as `mac-08`, the same
/// entry typed in capitals is the same row (the generation does not move), and a removal in
/// yet another case takes it off. Were it stored as typed, the removal would answer 404 and
/// leave the node denied, or two spellings would be two entries to take off.
///
/// Catches: `entry_of` leaving a node id as typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_id_is_one_entry_whatever_its_case() {
    let (server, other) = served();
    let g = map_of(&other).generation();
    let (code, body) = request(
        server.api,
        "POST",
        "/v1/denied",
        OPERATOR,
        &deny_body("node Mac-08", "retired"),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let said: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(
        said,
        serde_json::json!({"entry": "node mac-08", "generation": g + 1})
    );
    let (code, body) = request(
        server.api,
        "POST",
        "/v1/denied",
        OPERATOR,
        &deny_body("node MAC-08", "again"),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains(&format!("\"generation\":{}", g + 1)),
        "{body}"
    );
    assert_eq!(
        map_of(&other).denied(),
        [DenyEntry::Node("mac-08".to_owned())]
    );

    let (code, body) = request(
        server.api,
        "DELETE",
        "/v1/denied?entry=node+mAc-08",
        OPERATOR,
        "",
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(map_of(&other).denied(), []);
    assert_eq!(map_of(&other).generation(), g + 2);
}

/// Catches an entry or a reason that is taken when it is not one: an entry of an unknown
/// kind, a serial that is not hex, a node with no id, a key hash of the wrong length, an
/// empty reason or none, an extra field, no body and a body that is not JSON are each 400
/// on `POST`, and so are a `DELETE` with no `entry` or an entry that is none; a value over
/// 253 bytes and a reason over 512 are 400 as well (the store's bounds). So is a node that
/// would name no certificate, because only DNS names are compared with it: an IP address
/// (v4 or v6), a `host:port` as a member's address is written, and a name with a control
/// character in it. Nothing is listed afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deny_list_write_names_an_entry_and_a_reason() {
    let (server, other) = served();
    let long_node = deny_body(&format!("node {}", "n".repeat(254)), "long");
    let long_reason = deny_body("node mac-07", &"r".repeat(513));
    let bodies = [
        deny_body("device mac-07", "a kind this build does not know"),
        deny_body("serial not-hex", "x"),
        deny_body("node", "x"),
        deny_body("node 192.0.2.7", "an IP address names no certificate"),
        deny_body("node 2001:db8::7", "nor does an IPv6 one"),
        deny_body("node mac-07:8982", "nor a member's address"),
        deny_body("node mac\u{0}07", "nor a name with a NUL"),
        deny_body(&format!("spki-sha256 {}", "a".repeat(63)), "x"),
        deny_body("node mac-07", ""),
        deny_body("node mac-07", "   "),
        long_node,
        long_reason,
        r#"{"entry":"node mac-07"}"#.to_owned(),
        r#"{"entry":"node mac-07","reason":"x","extra":1}"#.to_owned(),
        r#"{"reason":"x"}"#.to_owned(),
        String::new(),
        "not json".to_owned(),
    ];
    for body in bodies {
        let (code, text) = request(server.api, "POST", "/v1/denied", OPERATOR, &body).await;
        assert_eq!(code, 400, "{body}: {text}");
    }
    for query in [
        "",
        "?entry=",
        "?entry=nonsense",
        "?other=node%20x",
        "?entry=serial%20zz",
    ] {
        let path = format!("/v1/denied{query}");
        let (code, text) = request(server.api, "DELETE", &path, OPERATOR, "").await;
        assert_eq!(code, 400, "{path}: {text}");
    }
    assert_eq!(other.denied().expect("list"), []);
}

/// Catches a server without a state store answering a deny write as if it had a list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_members_answers_404() {
    let server = start(None);
    let (code, body) = request(
        server.api,
        "POST",
        "/v1/denied",
        OPERATOR,
        &deny_body("node mac-07", "retired"),
    )
    .await;
    assert_eq!(code, 404, "{body}");
    let (code, body) = request(
        server.api,
        "DELETE",
        "/v1/denied?entry=node%20mac-07",
        OPERATOR,
        "",
    )
    .await;
    assert_eq!(code, 404, "{body}");
}
