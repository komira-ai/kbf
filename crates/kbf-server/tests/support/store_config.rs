//! A worker listener serving `Membership` and `ColdStore` behind the role table over
//! mutual TLS, a CA that mints role certificates, and a registration source a test edits.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use crate::hooked::Hooked;
use kbf_enroll_registry::{EnrollRequest, IssuedRole, TokenSpec, create_token, enroll_txn};
use kbf_proto::kbf::enroll::v1::cold_store_client::ColdStoreClient;
use kbf_proto::kbf::enroll::v1::{GetStoreConfigRequest, StoreConfig};
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL};
use kbf_server::members_rpc::MembershipService;
use kbf_server::store_config::acks::Acks;
use kbf_server::store_config::service::ColdStoreService;
use kbf_server::store_config::{Snapshot, SourceError, StoreConfigSource};
use kbf_server::worker_roles::WorkerRolesLayer;
use kbf_state::Store;
use kbf_storage::service::{Admission, ROLE_URI_PREFIX};
use kbf_types::{NodeId, Role, RoleSet, TokenId, TokenSecret};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{
    Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig,
};

/// A secret no output may contain.
pub const SENTINEL: &str = "SENTINEL-secret-4be1d07a93c6";

pub fn config(generation: u64) -> StoreConfig {
    StoreConfig {
        connection: "main".to_owned(),
        role: "cold".to_owned(),
        endpoint: "http://s3.example:9000".to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "bkt".to_owned(),
        prefix: "kbf/".to_owned(),
        conditional_put: true,
        min_put_rate: 0,
        access_key_id: "AKIATEST".to_owned(),
        secret_access_key: SENTINEL.to_owned(),
        secret_version: generation,
    }
}

/// A registration a test sets and breaks.
pub struct Source {
    generation: Mutex<u64>,
    broken: AtomicBool,
    mute: AtomicBool,
    /// The registration served instead of [`config`], whatever its secret.
    custom: Mutex<Option<StoreConfig>>,
    /// While true a snapshot waits (a node's fetch is held half way).
    held: (Mutex<bool>, Condvar),
    /// The registration is gone from the snapshots, though the generation still names it.
    taken_back: AtomicBool,
    /// How many snapshots have been asked for.
    asked: AtomicU64,
}

impl Source {
    pub fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            generation: Mutex::new(generation),
            broken: AtomicBool::new(false),
            mute: AtomicBool::new(false),
            custom: Mutex::new(None),
            held: (Mutex::new(false), Condvar::new()),
            taken_back: AtomicBool::new(false),
            asked: AtomicU64::new(0),
        })
    }

    /// How many times a node has asked for the registration.
    pub fn asked(&self) -> u64 {
        self.asked.load(Ordering::SeqCst)
    }

    /// Serves `config` (its `secret_version` and generation follow the source's) from now on.
    pub fn serve(&self, config: StoreConfig) {
        *self.custom.lock().unwrap_or_else(PoisonError::into_inner) = Some(config);
    }

    /// Makes every snapshot wait until [`Source::release`].
    pub fn hold(&self) {
        *self.held.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    /// From now on a snapshot says nothing is registered, while `generation()` still names
    /// the registration: what a node sees when it is taken back between its registration
    /// and its fetch.
    pub fn take_back(&self) {
        self.taken_back.store(true, Ordering::SeqCst);
    }

    pub fn release(&self) {
        *self.held.0.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.held.1.notify_all();
    }

    pub fn set(&self, generation: u64) {
        *self
            .generation
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = generation;
    }

    pub fn break_secret(&self) {
        self.broken.store(true, Ordering::SeqCst);
    }

    /// The source can no longer say its generation (a failing store).
    pub fn go_mute(&self) {
        self.mute.store(true, Ordering::SeqCst);
    }

    fn read(&self) -> u64 {
        *self
            .generation
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl StoreConfigSource for Source {
    fn generation(&self) -> Result<u64, SourceError> {
        if self.mute.load(Ordering::SeqCst) {
            return Err(SourceError::Store);
        }
        Ok(self.read())
    }

    fn snapshot(&self) -> Result<Snapshot, SourceError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mut held = self.held.0.lock().unwrap_or_else(PoisonError::into_inner);
        while *held {
            held = self
                .held
                .1
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(held);
        if self.broken.load(Ordering::SeqCst) {
            return Err(SourceError::SecretUnavailable);
        }
        if self.taken_back.load(Ordering::SeqCst) {
            return Ok(Snapshot {
                generation: 0,
                config: None,
            });
        }
        let generation = self.read();
        let custom = self
            .custom
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Ok(Snapshot {
            generation,
            config: (generation > 0).then(|| {
                custom.map_or_else(
                    || config(generation),
                    |mut custom| {
                        custom.secret_version = generation;
                        custom
                    },
                )
            }),
        })
    }
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("IA5"))
}

pub struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    pub fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name.push(DnType::CommonName, "test CA");
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

    /// A client certificate for the node `names` with `role` (none: no role name at all).
    pub fn client(&self, role: Option<&str>, names: &[&str]) -> Identity {
        let (cert, key) = self.client_pem(role, names);
        Identity::from_pem(cert, key)
    }

    /// [`Pki::client`]'s certificate and key, PEM, for a node that reads them from files.
    pub fn client_pem(&self, role: Option<&str>, names: &[&str]) -> (String, String) {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = names.iter().map(|n| dns(n)).collect();
        if let Some(role) = role {
            params.subject_alt_names.push(SanType::URI(
                format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
            ));
        }
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    }

    pub async fn channel(&self, addr: SocketAddr, identity: Identity) -> Channel {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(identity);
        Endpoint::from_shared(format!("https://{addr}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .expect("connect")
    }
}

/// A frontend's worker listener, as far as the registration goes.
pub struct Farm {
    pub addr: SocketAddr,
    pub pki: Pki,
    pub store: Arc<Hooked>,
    /// The state store the acks are kept in: apart from the members' so a test breaks one.
    pub ack_store: Arc<Hooked>,
    pub members: Arc<Members>,
    pub acks: Arc<Acks>,
    pub source: Arc<Source>,
}

/// Serves `source`; with `registry`, the service also checks the enrollment registry in the
/// same state store.
pub async fn farm(source: Arc<Source>, registry: bool) -> Farm {
    farm_denying(source, registry, None).await
}

/// [`farm`], with the worker listener's deny list file, read again at every call.
pub async fn farm_denying(
    source: Arc<Source>,
    registry: bool,
    deny_list: Option<kbf_server::DenyList>,
) -> Farm {
    farm_beating(source, registry, deny_list, Duration::from_secs(600)).await
}

/// [`farm_denying`] with members that must beat every `ttl / 3`, for a test with a real
/// storage node (the state store's clock is manual, so no member lapses).
pub async fn farm_beating(
    source: Arc<Source>,
    registry: bool,
    deny_list: Option<kbf_server::DenyList>,
    ttl: Duration,
) -> Farm {
    let store = Hooked::new();
    let members = Arc::new(Members::new(
        Arc::clone(&store) as Arc<dyn Store>,
        MembershipConfig {
            ttl,
            dwell: Duration::from_secs(600),
            replication: 1,
        },
    ));
    let ack_store = Hooked::new();
    let acks = Arc::new(Acks::new(Arc::clone(&ack_store) as Arc<dyn Store>));
    let pki = Pki::new();
    let membership = MembershipService::new(Arc::clone(&members), Admission::Certified)
        .with_store_config(
            Arc::clone(&source) as Arc<dyn StoreConfigSource>,
            Arc::clone(&acks),
        )
        .server();
    let cold = ColdStoreService::new(
        Arc::clone(&source) as Arc<dyn StoreConfigSource>,
        Admission::Certified,
    )
    .with_deny_list(deny_list)
    .with_denied_rows(Some(Arc::clone(&members)))
    .with_registry(registry.then(|| Arc::clone(&store) as Arc<dyn Store>))
    .server();
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    let server = Server::builder()
        .tls_config(pki.server())
        .expect("TLS")
        .layer(WorkerRolesLayer::new(Admission::Certified))
        .add_service(membership)
        .add_service(cold);
    tokio::spawn(server.serve_with_incoming(incoming));
    Farm {
        addr,
        pki,
        store,
        ack_store,
        members,
        acks,
        source,
    }
}

pub fn registration(name: &str) -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: format!("{name}:8982"),
        addr: format!("{name}:8982"),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        ..Default::default()
    }
}

pub fn membership(channel: &Channel) -> MembershipClient<Channel> {
    MembershipClient::new(channel.clone())
}

pub fn cold(channel: &Channel) -> ColdStoreClient<Channel> {
    ColdStoreClient::new(channel.clone())
}

pub fn want(have: u64) -> GetStoreConfigRequest {
    GetStoreConfigRequest {
        have_generation: have,
    }
}

/// A self-signed certificate (DER) with these names and role: what a service sees of a caller.
pub fn der(role: Option<&str>, names: &[&str]) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = names.iter().map(|n| dns(n)).collect();
    if let Some(role) = role {
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        ));
    }
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

/// Enrolls node `name` in the registry held in `store` (a daemon-role row: the registry check
/// reads the row's state, not its roles).
pub fn enroll(store: &dyn Store, name: &str) {
    let secret = TokenSecret::from_bytes([7; 32]);
    let token = TokenId::parse("aaaaaaaaaaaa").expect("a token id");
    let spec = TokenSpec {
        roles: RoleSet::new([Role::Daemon]).expect("roles"),
        host_role: None,
        max_uses: 5,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "t".to_owned(),
        created_by: "t".to_owned(),
    };
    create_token(store, &token, &secret, &spec).expect("token");
    let node: NodeId = name.parse().expect("a node id");
    let now = store.now().as_duration().as_secs();
    let mut sign = || {
        Ok(vec![IssuedRole {
            role: Role::Daemon,
            spki_sha256: [1; 32],
            serial: vec![1],
            not_before: now - 300,
            not_after: now + 86_400,
            cert_der: vec![1],
        }])
    };
    enroll_txn(
        store,
        &EnrollRequest {
            token_id: &token,
            secret: secret.as_bytes(),
            node_id: &node,
            csrs: &[(Role::Daemon, name.as_bytes())],
            advertise: "",
            zone: "z",
        },
        &mut sign,
    )
    .map_err(|e| format!("{e:?}"))
    .expect("enrolled");
}
