//! The enrollment routes over a real listener, called by the real client: the paths, the
//! methods and the gate as `kbf token` meets them. (The handlers' own behavior is pinned
//! in `src/enroll/api/tests.rs`.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_server::api_client::OperatorClient;
use kbf_server::enroll::api::{EnrollApi, NODES, TOKENS, routes};
use kbf_server::enroll::{OsRandom, PinError, PinSource};
use kbf_server::token::ApiToken;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Fenced, ManualClock, Store};
use kbf_types::CaPin;
use kbf_types::enroll_api::{NodeList, TokenCreated, TokenList, TokenRevoked};

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

struct FixedPin;

impl PinSource for FixedPin {
    fn pin(&self) -> Result<CaPin, PinError> {
        Ok(CaPin::from_bytes([5; 32]))
    }
}

fn token_file() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-enroll-api");
    std::fs::create_dir_all(&dir).expect("a token directory");
    let path = dir.join(format!("token-{}", std::process::id()));
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

async fn serve(enroll: Option<Arc<EnrollApi>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = routes(Some(token_file()), enroll);
    tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.expect("serve");
    });
    addr
}

fn enrollment() -> Arc<EnrollApi> {
    let clock = Arc::new(ManualClock::default());
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let fenced: Arc<dyn Store> = Arc::new(Fenced::take(fake, None).expect("writer"));
    Arc::new(EnrollApi::new(
        fenced,
        Arc::new(FixedPin),
        Arc::new(OsRandom),
    ))
}

// Catches a route mounted at another path or method than the client calls (create, list,
// DELETE, nodes, node revoke), and a client whose GET or DELETE lacks the Content-Type the
// gate asks of every call: the whole round trip works with the real client, and the plain
// GET (which sends no content type) is refused on both reads.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_reaches_every_route_through_the_gate() {
    let addr = serve(Some(enrollment())).await;
    tokio::task::spawn_blocking(move || {
        let client = OperatorClient::new(addr, Some(TOKEN.to_owned()));
        let created = client
            .post(TOKENS, r#"{"roles":["daemon"],"max_uses":2}"#)
            .expect("create");
        assert_eq!(created.status, 200, "{}", created.body);
        let created: TokenCreated = serde_json::from_str(&created.body).expect("a created token");
        assert!(created.token.starts_with("kbft1."));

        let listed = client.get_gated(TOKENS).expect("list");
        assert_eq!(listed.status, 200, "{}", listed.body);
        let listed: TokenList = serde_json::from_str(&listed.body).expect("a list");
        assert_eq!(listed.tokens, [created.info.clone()]);
        assert!(!listed.tokens[0].id.is_empty());

        for path in [TOKENS, NODES] {
            let plain = client.get(path).expect("plain get");
            assert_eq!(plain.status, 415, "{path}: {}", plain.body);
        }
        let nodes = client.get_gated(NODES).expect("nodes");
        assert_eq!(nodes.status, 200);
        let nodes: NodeList = serde_json::from_str(&nodes.body).expect("nodes");
        assert!(nodes.nodes.is_empty());
        let none = client
            .post("/v1/enroll/nodes/n1/revoke", "")
            .expect("revoke node");
        assert_eq!(none.status, 404, "{}", none.body);

        let path = format!("{TOKENS}/{}", created.info.id);
        let revoked = client.delete(&path).expect("delete");
        assert_eq!(revoked.status, 200, "{}", revoked.body);
        let revoked: TokenRevoked = serde_json::from_str(&revoked.body).expect("revoked");
        assert_eq!(revoked.id, created.info.id);
        let listed = client.get_gated(TOKENS).expect("list again");
        let listed: TokenList = serde_json::from_str(&listed.body).expect("a list");
        assert!(listed.tokens[0].revoked_at.is_some());

        let anonymous = OperatorClient::new(addr, None);
        assert_eq!(anonymous.get_gated(TOKENS).expect("get").status, 401);
        assert_eq!(anonymous.delete(&path).expect("delete").status, 401);
    })
    .await
    .expect("the client side");
}

// Catches a server without enrollment answering anything but 404 to a caller who passed the
// gate, so a client can tell "off" from "refused".
#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_enrollment_answers_404() {
    let addr = serve(None).await;
    tokio::task::spawn_blocking(move || {
        let client = OperatorClient::new(addr, Some(TOKEN.to_owned()));
        assert_eq!(client.get_gated(TOKENS).expect("get").status, 404);
    })
    .await
    .expect("the client side");
}
