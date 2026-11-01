//! The Storage page of `kbf-ui` over this server's real `GET /v1/storage`, through a
//! listener, with a state store: the document the route answers is the one the page
//! parses, a heartbeat's write-tier health reaches the page in the page's units, and
//! what the route says it does not know is "unknown" on the page, never zero
//! (`docs/design/kbf-ui-v1.md` 3.1).
//!
//! Catches: a key the page reads that the route does not answer, or answers in another
//! form (the page would be `502`); a member the route lists missing from the page; a
//! value shown that the heartbeat did not carry; an unknown shown as zero.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_map::MemberState;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::Store;
use kbf_ui::{CONTENT_SECURITY_POLICY, Fetched, Source, SourceError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(600),
        dwell: Duration::from_secs(1200),
        replication: 2,
    }
}

struct Server {
    api: SocketAddr,
    worker: SocketAddr,
    members: Members,
}

fn start() -> Server {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
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
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let api = Api {
        listen: loopback(),
        token: None,
        local_cache: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners,
        Some(api),
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    let api = bound.api.expect("an API address");
    let worker = bound.worker;
    tokio::spawn(bound.serving);
    Server {
        api,
        worker,
        members: Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config()),
    }
}

/// The operator API as the page's source, remembering each URL and its status.
struct Asked<'a> {
    server: &'a Server,
    answered: Mutex<Vec<(String, u16)>>,
}

impl Source for Asked<'_> {
    async fn get(&self, url: &str) -> Result<Fetched, SourceError> {
        let mut tcp = TcpStream::connect(self.server.api).await.expect("connect");
        let request = format!("GET {url} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
        tcp.write_all(request.as_bytes()).await.expect("send");
        let mut response = Vec::new();
        tcp.read_to_end(&mut response).await.expect("read");
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a head and a body");
        let head = String::from_utf8_lossy(&response[..split]).into_owned();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .expect("a status code");
        self.answered
            .lock()
            .expect("not poisoned")
            .push((url.to_owned(), status));
        Ok(Fetched {
            status,
            body: response[split + 4..].to_vec(),
            headers: Vec::new(),
        })
    }
}

/// The text between the first `<tr><th scope="row">{id}</th>` under `heading` and its `</tr>`.
fn row(html: &str, heading: &str, id: u64) -> String {
    let from = html.find(&format!("<h2>{heading}</h2>")).expect("heading");
    let table = &html[from..];
    let start = table
        .find(&format!("<tr><th scope=\"row\">{id}</th>"))
        .expect("a row for the member");
    let end = table[start..].find("</tr>").expect("row end") + start;
    table[start..end].to_owned()
}

/// Over a farm of no member, then a member that beat with its write tier and one that
/// registered and is silent: the page parses the route, shows the heartbeat's values in its
/// units, shows nothing the heartbeat did not carry as "unknown", and reads `/v1/storage`
/// only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_page_renders_the_real_route() {
    use kbf_proto::kbf::map::v1 as pb;
    use kbf_proto::kbf::map::v1::membership_client::MembershipClient;

    let s = start();
    let api = Asked {
        server: &s,
        answered: Mutex::new(Vec::new()),
    };

    // No member yet: the route answers, the page says so.
    let empty = kbf_ui::respond(&api, "/ui/storage", "").await;
    let html = String::from_utf8(empty.body().to_vec()).expect("UTF-8");
    assert_eq!(empty.status(), 200, "{html}");
    assert!(html.contains("No storage member is registered."), "{html}");
    assert!(!html.contains("<td>0"), "{html}");

    // Member 1 registers over the wire, is promoted, and beats with its write tier;
    // member 2 only registers and says nothing.
    let mut client = MembershipClient::connect(format!("http://{}", s.worker))
        .await
        .expect("connects");
    let joined = client
        .register(pb::RegisterRequest {
            role: pb::Role::Storage.into(),
            instance_id: "node-1".to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            weight: 1,
            protocol: PROTOCOL,
            ..pb::RegisterRequest::default()
        })
        .await
        .expect("registers")
        .into_inner();
    s.members
        .set_state(kbf_map::MemberId(joined.member_id), MemberState::Active)
        .expect("promoted");
    client
        .heartbeat(pb::HeartbeatRequest {
            member_id: joined.member_id,
            boot: joined.boot,
            repair_debts: 3,
            debts_known: true,
            unflushed_bytes: Some(1_048_576),
            unflushed_blocks: Some(2),
            oldest_unflushed_ms: Some(4200),
            budget_bytes: Some(107_374_182_400),
            used_bytes: Some(2_097_152),
            flush_failures: Some(0),
            flush_conflicts: Some(5),
            ..pb::HeartbeatRequest::default()
        })
        .await
        .expect("beats");
    s.members
        .register(&Registrant {
            instance_id: "node-2".to_owned(),
            addr: "192.0.2.2:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: PROTOCOL,
            progress: Progress::CaughtUp,
        })
        .expect("registers");

    let answer = kbf_ui::respond(&api, "/ui/storage", "").await;
    let html = String::from_utf8(answer.body().to_vec()).expect("UTF-8");
    assert_eq!(answer.status(), 200, "{html}");
    assert!(
        answer
            .headers()
            .contains(&("content-security-policy", CONTENT_SECURITY_POLICY)),
        "{html}"
    );
    let first = row(&html, "Write tier", 1);
    assert_eq!(
        first,
        "<tr><th scope=\"row\">1</th><td>1.0 MiB</td><td>2</td><td>4.2 s</td>\
         <td>100.0 GiB</td><td>2.0 MiB</td><td>0</td><td>5</td>",
        "{html}"
    );
    // Member 2 sent nothing: unknown in every cell, and its silence is marked stale.
    let second = row(&html, "Write tier", 2);
    assert_eq!(second.matches("<td>unknown</td>").count(), 7, "{second}");
    let members = row(&html, "Members", 2);
    assert!(members.contains("stale</span>"), "{members}");
    assert!(row(&html, "Members", 1).contains("fresh</span>"), "{html}");
    // One member is silent: the number is unknown, and the page does not say zero.
    assert!(
        html.contains("<dt>Under-replicated blobs</dt><dd>unknown</dd>"),
        "{html}"
    );
    assert!(!html.contains("<script"), "{html}");

    let answered = api.answered.lock().expect("not poisoned").clone();
    assert_eq!(
        answered,
        [
            ("/v1/storage".to_owned(), 200),
            ("/v1/storage".to_owned(), 200)
        ]
    );
}
