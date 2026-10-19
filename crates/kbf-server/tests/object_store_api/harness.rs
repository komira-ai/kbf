//! The farm under test: the real routes over a registry on the state store's fake, a probe
//! that answers what a test says, and nodes that report what a test says. Calls go
//! through the router as a service, with the peer address a test chooses.

#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request};
use kbf_server::enroll::Random;
use kbf_server::object_store::Registry;
use kbf_server::object_store::api::{AckSource, ObjectStoreApi, routes};
use kbf_server::object_store::probe::{Probe, ProbeTarget};
use kbf_server::token::ApiToken;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::object_store_api::{NodeConvergence, Verified};
use serde::de::DeserializeOwned;
use tower_service::Service as _;

pub const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// A string no answer, log line, row or debug print may ever hold.
pub const SENTINEL: &str = "SENTINEL-s3-secret-9f8e7d6c5b4a";

pub fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40_000))
}

fn unique(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-object-store-api")
        .join(format!("{label}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

pub fn token_file() -> ApiToken {
    use std::os::unix::fs::PermissionsExt as _;
    let path = unique("token").join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

/// Every draw is the next number, big-endian: secret versions are 1, 2, 3.
struct Counting(AtomicU64);

impl Random for Counting {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        buf.copy_from_slice(&self.0.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        Ok(())
    }
}

/// What the probe was asked.
#[derive(Clone, Debug)]
pub struct Call {
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret: String,
    pub bucket: String,
    pub prefix: String,
    pub conditional_put: bool,
    pub min_put_rate: Option<NonZeroU64>,
}

/// A probe that records its calls and answers `result`.
pub struct ScriptedProbe {
    /// Makes the next probes panic, as a bug in the blocking task would.
    pub panics: std::sync::atomic::AtomicBool,
    pub result: Mutex<Verified>,
    pub calls: Mutex<Vec<Call>>,
}

impl ScriptedProbe {
    pub fn passing() -> Verified {
        Verified {
            ok: true,
            put: true,
            get: true,
            delete: true,
            conditional_put: false,
        }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    /// The next probes fail at the read.
    pub fn fail_at_get(&self) {
        *self.result.lock().expect("result") = Verified {
            ok: false,
            put: true,
            get: false,
            delete: true,
            conditional_put: false,
        };
    }
}

impl Probe for ScriptedProbe {
    fn probe(&self, target: &ProbeTarget<'_>) -> Verified {
        assert!(!self.panics.load(Ordering::SeqCst), "a probe that panics");
        self.calls.lock().expect("calls").push(Call {
            endpoint: target.endpoint.to_owned(),
            region: target.region.to_owned(),
            access_key_id: target.access_key_id.to_owned(),
            secret: target.secret.expose().to_owned(),
            bucket: target.bucket.to_owned(),
            prefix: target.prefix.to_owned(),
            conditional_put: target.conditional_put,
            min_put_rate: target.min_put_rate,
        });
        *self.result.lock().expect("result")
    }
}

/// Nodes that report what a test sets.
#[derive(Default)]
pub struct Acks(pub Mutex<Vec<NodeConvergence>>);

impl Acks {
    pub fn set(&self, nodes: Vec<NodeConvergence>) {
        *self.0.lock().expect("nodes") = nodes;
    }
}

impl AckSource for Acks {
    fn nodes(&self) -> Vec<NodeConvergence> {
        self.0.lock().expect("nodes").clone()
    }
}

/// A store whose reads or writes fail on demand.
pub struct Flaky {
    inner: Arc<dyn Store>,
    pub fail_reads: std::sync::atomic::AtomicBool,
    pub fail_writes: std::sync::atomic::AtomicBool,
    /// Another writer moves the generation row just before the next write commits.
    pub race_next_write: std::sync::atomic::AtomicBool,
}

impl Flaky {
    pub fn over(inner: Arc<dyn Store>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_reads: false.into(),
            fail_writes: false.into(),
            race_next_write: false.into(),
        })
    }

    fn read(&self) -> Result<(), StoreError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("injected read".to_owned()));
        }
        Ok(())
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("injected write".to_owned()));
        }
        if self.race_next_write.swap(false, Ordering::SeqCst) {
            let rival = Txn::new().put("objstore/gen", vec![1], Cond::Any);
            self.inner.apply_batch(vec![rival])?;
        }
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.read()?;
        self.inner.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.read()?;
        self.inner.scan_prefix(prefix)
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.read()?;
        self.inner.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// An answer.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// The `code` of an error answer.
    pub fn code(&self) -> String {
        let value: serde_json::Value = serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("an error body is JSON ({e}): {}", self.body));
        value["code"]
            .as_str()
            .unwrap_or_else(|| panic!("a closed code in {}", self.body))
            .to_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// One call to `router`: any peer, any headers, any body.
pub async fn send(
    router: &Router,
    peer: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send_body(router, peer, request, Body::from(body.to_vec())).await
}

/// One call whose body is `body`.
pub async fn send_body(
    router: &Router,
    peer: SocketAddr,
    request: axum::http::request::Builder,
    body: Body,
) -> Reply {
    let mut request = request.body(body).expect("a request");
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = router.clone().call(request).await.expect("infallible");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
    Reply {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// The farm: routes, registry, probe, nodes, state.
pub struct Farm {
    pub router: Router,
    pub registry: Arc<Registry>,
    pub probe: Arc<ScriptedProbe>,
    pub acks: Arc<Acks>,
    pub dir: PathBuf,
    pub store: Arc<Fake>,
    pub flaky: Arc<Flaky>,
}

impl Farm {
    pub fn new() -> Self {
        Self::with_token(Some(token_file()))
    }

    pub fn with_token(token: Option<ApiToken>) -> Self {
        let clock = Arc::new(ManualClock::default());
        clock.advance(std::time::Duration::from_secs(1_000));
        let store = Arc::new(Fake::with_flaw(clock, Flaw::None));
        let flaky = Flaky::over(store.clone());
        let dir = unique("secrets").join("objstore");
        let registry = Arc::new(
            Registry::open(flaky.clone(), &dir, Arc::new(Counting(AtomicU64::new(1))))
                .expect("a registry"),
        );
        let probe = Arc::new(ScriptedProbe {
            panics: false.into(),
            result: Mutex::new(ScriptedProbe::passing()),
            calls: Mutex::new(Vec::new()),
        });
        let acks = Arc::new(Acks::default());
        let api = Arc::new(ObjectStoreApi::new(
            registry.clone(),
            probe.clone(),
            acks.clone(),
        ));
        Self {
            router: routes(token, Some(api)),
            registry,
            probe,
            acks,
            dir,
            store,
            flaky,
        }
    }

    /// A call that passes the gate: the bearer and the JSON content type.
    pub async fn call(&self, method: &str, path: &str, body: &str) -> Reply {
        let bearer = format!("Bearer {TOKEN}");
        send(
            &self.router,
            loopback(),
            method,
            path,
            &[
                ("authorization", &bearer),
                ("content-type", "application/json"),
            ],
            body.as_bytes(),
        )
        .await
    }

    /// A call with exactly `headers`.
    pub async fn with(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Reply {
        send(
            &self.router,
            loopback(),
            method,
            path,
            headers,
            body.as_bytes(),
        )
        .await
    }

    pub fn generation(&self) -> u64 {
        self.registry.generation().expect("generation")
    }

    /// The files of the secrets directory, sorted.
    pub fn files(&self) -> Vec<String> {
        listing(&self.dir)
    }

    /// Every key and value in the state store, as text, for a scan for a secret.
    pub fn state_dump(&self) -> String {
        let rows = self.store.scan_prefix("").expect("scan");
        rows.iter()
            .map(|(k, v)| format!("{k}={}", String::from_utf8_lossy(&v.value)))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A PUT connection body with `secret`.
pub fn conn_body(secret: &str) -> String {
    format!(
        r#"{{"endpoint":"http://s3.example:9000","access_key_id":"AKID","secret_access_key":"{secret}"}}"#
    )
}

pub const BIND_BODY: &str = r#"{"connection":"cold","bucket":"farm-bucket","prefix":"cold/"}"#;

/// Creates the connection `cold` and binds the role `cold` to it.
pub async fn registered(farm: &Farm) {
    assert_eq!(
        farm.call(
            "PUT",
            "/v1/object-stores/connections/cold",
            &conn_body(SENTINEL)
        )
        .await
        .status,
        201
    );
    assert_eq!(
        farm.call("PUT", "/v1/object-stores/bindings/cold", BIND_BODY)
            .await
            .status,
        201
    );
}
