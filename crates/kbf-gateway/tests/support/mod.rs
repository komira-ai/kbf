//! A farm CA, certificates of each kind, a gateway on loopback with a clock the test
//! moves, and clients of both its services.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use envoy_types::pb::envoy::service::discovery::v3::aggregated_discovery_service_client::AggregatedDiscoveryServiceClient;
use kbf_gateway::gateway::{self, Config};
use kbf_gateway::registry::{Clock, Registry};
use kbf_proto::gateway::registration_client::RegistrationClient;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use tokio::sync::oneshot;
use tonic::transport::{
    Certificate, Channel, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig,
};

/// How long registrations live in the gateways of the tests.
pub const TTL: Duration = Duration::from_secs(6);

/// A certificate and its key, PEM.
#[derive(Clone)]
pub struct Leaf {
    pub cert: String,
    pub key: String,
}

/// The farm CA.
pub struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    pub fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf gateway test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca =
            CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("CA key")).expect("CA");
        Self { ca }
    }

    pub fn ca_pem(&self) -> String {
        self.ca.pem()
    }

    /// A certificate naming `names` with `usages`, signed by the CA.
    pub fn leaf(&self, names: &[&str], usages: Vec<ExtendedKeyUsagePurpose>) -> Leaf {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("leaf params");
        params.extended_key_usages = usages;
        let key = KeyPair::generate().expect("leaf key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        Leaf {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    /// A server certificate: serverAuth and clientAuth, one DNS name, the node's.
    pub fn server(&self, name: &str) -> Leaf {
        self.leaf(
            &[name],
            vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
        )
    }

    /// A daemon's certificate: clientAuth only.
    pub fn daemon(&self, node: &str) -> Leaf {
        self.leaf(&[node], vec![ExtendedKeyUsagePurpose::ClientAuth])
    }
}

/// A clock the test moves.
#[derive(Clone)]
pub struct Manual {
    base: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl Manual {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    pub fn clock(&self) -> Clock {
        let this = self.clone();
        Clock::from_fn(move || this.base + *this.elapsed.lock().expect("clock"))
    }

    pub fn advance(&self, by: Duration) {
        *self.elapsed.lock().expect("clock") += by;
    }
}

/// A gateway serving on loopback until dropped.
pub struct Running {
    pub registration: SocketAddr,
    pub xds: SocketAddr,
    pub registry: Arc<Registry>,
    pub clock: Manual,
    pub ca: String,
    _stop: oneshot::Sender<()>,
}

/// Starts a gateway whose certificate is `localhost`'s and whose callers must chain to
/// `pki`'s CA.
pub fn start(pki: &Pki) -> Running {
    let identity = pki.leaf(&["localhost"], vec![ExtendedKeyUsagePurpose::ServerAuth]);
    let config = Config {
        register_listen: "127.0.0.1:0".parse().expect("address"),
        tls: ServerTlsConfig::new()
            .identity(Identity::from_pem(identity.cert, identity.key))
            .client_ca_root(Certificate::from_pem(pki.ca_pem())),
        xds_listen: "127.0.0.1:0".parse().expect("address"),
        ttl: TTL,
    };
    let clock = Manual::new();
    let (stop, stopped) = oneshot::channel::<()>();
    let bound = gateway::bind(config, clock.clock(), async move {
        let _ = stopped.await;
    })
    .expect("bind the gateway");
    let (registration, xds, registry) = (bound.registration, bound.xds, bound.registry.clone());
    tokio::spawn(bound.serve());
    Running {
        registration,
        xds,
        registry,
        clock,
        ca: pki.ca_pem(),
        _stop: stop,
    }
}

impl Running {
    /// A registration client presenting `identity`. It connects at its first call, so a
    /// handshake the gateway refuses is that call's error.
    pub fn registrar(&self, identity: &Leaf) -> RegistrationClient<Channel> {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.ca))
            .identity(Identity::from_pem(&identity.cert, &identity.key))
            .domain_name("localhost");
        let channel = Endpoint::from_shared(format!("https://{}", self.registration))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect_lazy();
        RegistrationClient::new(channel)
    }

    /// An ADS client, as the local Envoy would be.
    pub async fn ads(&self) -> AggregatedDiscoveryServiceClient<Channel> {
        let channel = Endpoint::from_shared(format!("http://{}", self.xds))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect to the xDS listener");
        AggregatedDiscoveryServiceClient::new(channel)
    }
}
