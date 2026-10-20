//! The frontend half of the rig: the state store, the secret directory, the CA and the S3
//! service a farm keeps across a frontend's restart, and [`Frontend`], the processes' worth
//! of listeners one frontend run has.
//!
//! **Two adapters live here because the frontend binary has neither**: [`RegistrySource`]
//! (the `StoreConfigSource` that reads the registry and its secret file) and [`Reports`]
//! (the `AckSource` that reads the nodes' acks). `frontend.rs` wires `NoAcks` and serves no
//! `ColdStore` from the registry today, so a test of the whole path has to build them. They
//! hold no logic of their own beyond reading the rows.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_map::MemberState;
use kbf_objstore::s3::fake::FakeS3;
use kbf_proto::kbf::enroll::v1::StoreConfig;
use kbf_server::enroll::OsRandom;
use kbf_server::members::{Members, MembershipConfig};
use kbf_server::members_rpc::MembershipService;
use kbf_server::object_store::api::{AckSource, ObjectStoreApi, routes};
use kbf_server::object_store::probe::S3Probe;
use kbf_server::object_store::{Registry, RegistryError};
use kbf_server::proxy_auth::{ProxySecret, gate_requests};
use kbf_server::store_config::acks::{Acks, NodeStatus as Reported};
use kbf_server::store_config::service::ColdStoreService;
use kbf_server::store_config::{Snapshot, SourceError, StoreConfigSource};
use kbf_server::token::ApiToken;
use kbf_server::worker_roles::WorkerRolesLayer;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_storage::service::{Admission, ROLE_URI_PREFIX};
use kbf_types::object_store_api::{BindingRole, MemberStanding, NodeConvergence, NodeStatus};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

/// The operator's token.
pub const TOKEN: &str = "kbf-e2e-token-0123456789abcdef0123456789";

/// The nodes' heartbeat period, with members' lease three times it.
pub const BEAT: Duration = Duration::from_millis(90);

/// A key pair the S3 service can be told to accept.
#[derive(Clone, Copy)]
pub struct Keys {
    pub id: &'static str,
    pub secret: &'static str,
}

/// The key pair of the store before a rotation.
pub const OLD: Keys = Keys {
    id: "AKIAOLD",
    secret: "old-secret-7c1f2a9e",
};

/// The key pair a rotation moves to.
pub const NEW: Keys = Keys {
    id: "AKIANEW",
    secret: "new-secret-b4d80e35",
};

/// The bucket and prefix every node's `--cold-store` and the binding name.
pub const BUCKET: &str = "bkt";
pub const PREFIX: &str = "kbf/";

fn unique(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-it-object-store-e2e")
        .join(format!("{label}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// What survives a frontend's death: the state store, the secret directory, the CA, the
/// S3 service, and the address the nodes dial.
pub struct Farm {
    pub store: Arc<Fake>,
    pub s3: FakeS3,
    pub pki: Pki,
    /// The scratch directory of the run: the secrets, the token, the nodes' directories.
    pub root: PathBuf,
    token: PathBuf,
    /// The worker listener's port once a frontend has bound it: a restarted frontend binds
    /// the same one, for the nodes' `--frontend` is fixed at their start.
    worker_port: Mutex<Option<u16>>,
}

impl Farm {
    /// A farm whose S3 service takes only `keys`.
    pub async fn new(label: &str, keys: Keys) -> Self {
        let s3 = FakeS3::start().await;
        s3.require_credentials(keys.id, keys.secret);
        let clock = Arc::new(ManualClock::default());
        let store = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
        let root = unique(label);
        let token = root.join("token");
        std::fs::write(&token, format!("{TOKEN}\n")).expect("write the token");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        Self {
            store,
            s3,
            pki: Pki::new(),
            root,
            token,
            worker_port: Mutex::new(None),
        }
    }

    /// The secret directory every frontend run of this farm shares (the host-local one).
    pub fn secrets(&self) -> PathBuf {
        self.root.join("secrets").join("objstore")
    }

    /// The `--cold-store` a node of this farm is started with.
    pub fn cold_flag(&self) -> String {
        format!("s3://{BUCKET}/{PREFIX}?endpoint={}", self.s3.endpoint())
    }

    /// The S3 service takes `keys` from now on, and no other pair.
    pub fn provider_accepts(&self, keys: Keys) {
        self.s3.require_credentials(keys.id, keys.secret);
    }
}

/// One run of the frontend: the worker listener nodes dial (membership and the cold-store
/// registration, over mutual TLS) and the operator API (plain HTTP on loopback), over the
/// farm's shared state.
pub struct Frontend {
    pub worker: SocketAddr,
    pub api: SocketAddr,
    pub members: Arc<Members>,
    pub registry: Arc<Registry>,
    stop: Vec<oneshot::Sender<()>>,
    done: Vec<JoinHandle<()>>,
}

impl Frontend {
    /// Starts a frontend over `farm`'s state; binds the worker port the farm used before.
    pub async fn start(farm: &Farm) -> Self {
        let store: Arc<dyn Store> = Arc::clone(&farm.store) as Arc<dyn Store>;
        let members = Arc::new(Members::new(
            Arc::clone(&store),
            MembershipConfig {
                ttl: BEAT * 3,
                dwell: Duration::from_secs(600),
                replication: 1,
            },
        ));
        let acks = Arc::new(Acks::new(Arc::clone(&store)));
        let registry = Arc::new(
            Registry::open(Arc::clone(&store), &farm.secrets(), Arc::new(OsRandom))
                .expect("a registry"),
        );
        let source: Arc<dyn StoreConfigSource> = Arc::new(RegistrySource(Arc::clone(&registry)));
        let reports = Arc::new(Reports {
            members: Arc::clone(&members),
            acks: Arc::clone(&acks),
            registry: Arc::clone(&registry),
        });

        let membership = MembershipService::new(Arc::clone(&members), Admission::Certified)
            .with_store_config(Arc::clone(&source), acks)
            .server();
        let cold = ColdStoreService::new(source, Admission::Certified)
            .with_denied_rows(Some(Arc::clone(&members)))
            .server();
        let port = *farm.worker_port.lock().expect("port");
        let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0))))
            .expect("bind the worker listener");
        let worker = incoming.local_addr().expect("addr");
        *farm.worker_port.lock().expect("port") = Some(worker.port());
        let server = Server::builder()
            .tls_config(farm.pki.server())
            .expect("TLS")
            .layer(WorkerRolesLayer::new(Admission::Certified))
            .add_service(membership)
            .add_service(cold);
        let (stop_worker, worker_stopped) = oneshot::channel::<()>();
        let worker_done = tokio::spawn(async move {
            server
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = worker_stopped.await;
                })
                .await
                .expect("the worker listener serves");
        });

        let api = ObjectStoreApi::new(
            Arc::clone(&registry),
            Arc::new(S3Probe::new(tokio::runtime::Handle::current())),
            reports,
        );
        let token = ApiToken::from_file(&farm.token).expect("a usable token");
        let router = routes(Some(token), Some(Arc::new(api))).layer(
            axum::middleware::from_fn_with_state(None::<Arc<ProxySecret>>, gate_requests),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the API");
        let api_addr = listener.local_addr().expect("addr");
        let (stop_api, api_stopped) = oneshot::channel::<()>();
        let api_done = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = api_stopped.await;
            })
            .await
            .expect("the API serves");
        });

        Self {
            worker,
            api: api_addr,
            members,
            registry,
            stop: vec![stop_worker, stop_api],
            done: vec![worker_done, api_done],
        }
    }

    /// Stops both listeners and waits until the worker port is free again: the frontend
    /// process is gone, and a node's next call finds nobody.
    pub async fn kill(mut self) {
        for stop in self.stop.drain(..) {
            let _ = stop.send(());
        }
        for done in self.done.drain(..) {
            done.await.expect("a listener task");
        }
    }

    /// Every member `Active`: the nodes here never say they are whole, and a member that is
    /// not `Active` is not counted by the registration's convergence.
    pub fn promote(&self) {
        let Some(map) = self.members.refresh().ok().flatten() else {
            return;
        };
        for member in map.members() {
            if member.state != MemberState::Active {
                self.members
                    .set_state(member.id, MemberState::Active)
                    .expect("promote");
            }
        }
    }
}

impl Drop for Frontend {
    fn drop(&mut self) {
        for done in &self.done {
            done.abort();
        }
    }
}

/// The registry as the nodes' source: the connection the `cold` binding names, its bucket
/// and prefix and its secret file, read as one generation.
pub struct RegistrySource(Arc<Registry>);

impl RegistrySource {
    fn read(&self, generation: u64) -> Result<Option<StoreConfig>, RegistryError> {
        let Some(binding) = self
            .0
            .bindings()?
            .into_iter()
            .find(|b| b.role == BindingRole::Cold)
        else {
            return Ok(None);
        };
        let connection = self
            .0
            .connection(&binding.connection)?
            .ok_or(RegistryError::NoSuchConnection)?;
        let secret = self.0.secret(&binding.connection)?;
        Ok(Some(StoreConfig {
            connection: connection.name.to_string(),
            role: "cold".to_owned(),
            endpoint: connection.endpoint,
            region: connection.region,
            bucket: binding.bucket,
            prefix: binding.prefix,
            conditional_put: connection.conditional_put,
            min_put_rate: connection.min_put_rate.map_or(0, std::num::NonZeroU64::get),
            access_key_id: connection.access_key_id,
            secret_access_key: secret.expose().to_owned(),
            secret_version: generation,
        }))
    }
}

impl StoreConfigSource for RegistrySource {
    fn generation(&self) -> Result<u64, SourceError> {
        self.0.generation().map_err(|_| SourceError::Store)
    }

    fn snapshot(&self) -> Result<Snapshot, SourceError> {
        // The pieces are read apart: a write between them changes the generation, and the
        // reading is made again.
        for _ in 0..8 {
            let before = self.generation()?;
            let config = self.read(before).map_err(|e| match e {
                RegistryError::Secret(_) => SourceError::SecretUnavailable,
                _ => SourceError::Store,
            })?;
            if self.generation()? == before {
                return Ok(Snapshot {
                    generation: if config.is_some() { before } else { 0 },
                    config,
                });
            }
        }
        Err(SourceError::Store)
    }
}

/// The nodes' acks as the operator API reads them: the members of the map, named by the
/// address they registered, with what they last reported.
pub struct Reports {
    members: Arc<Members>,
    acks: Arc<Acks>,
    registry: Arc<Registry>,
}

impl AckSource for Reports {
    fn nodes(&self) -> Vec<NodeConvergence> {
        let Some(map) = self.members.refresh().ok().flatten() else {
            return Vec::new();
        };
        let generation = self.registry.generation().unwrap_or(0);
        let Ok(got) = self.acks.convergence(&map, generation) else {
            return Vec::new();
        };
        got.nodes
            .iter()
            .filter_map(|ack| {
                let member = map.members().iter().find(|m| m.id == ack.member)?;
                Some(NodeConvergence {
                    id: member.addr.clone(),
                    state: match ack.state {
                        MemberState::Draining => MemberStanding::Draining,
                        _ => MemberStanding::Active,
                    },
                    applied_generation: ack.applied_generation,
                    status: match ack.status {
                        Reported::Ok => NodeStatus::Ok,
                        Reported::Pending => NodeStatus::Pending,
                        Reported::ProbeFailed => NodeStatus::ProbeFailed,
                        Reported::IdentityMismatch => NodeStatus::IdentityMismatch,
                        Reported::Unsupported => NodeStatus::Unsupported,
                    },
                    last_seen_secs: ack.last_seen.map(|d| d.as_secs()),
                })
            })
            .collect()
    }
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("IA5"))
}

/// A CA that mints the nodes' storage-role certificates and the frontend's server one.
pub struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name.push(DnType::CommonName, "e2e CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self {
            ca: CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA"),
        }
    }

    fn server(&self) -> ServerTlsConfig {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        ServerTlsConfig::new()
            .identity(Identity::from_pem(cert.pem(), key.serialize_pem()))
            .client_ca_root(Certificate::from_pem(self.ca.pem()))
    }

    /// The CA certificate, PEM.
    pub fn ca_pem(&self) -> String {
        self.ca.pem()
    }

    /// A storage-role certificate and key for a node named `name`, PEM.
    pub fn storage_pem(&self, name: &str) -> (String, String) {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![dns(name)];
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}storage").try_into().expect("IA5"),
        ));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    }
}

/// The files of `dir`, sorted.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("a directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}
