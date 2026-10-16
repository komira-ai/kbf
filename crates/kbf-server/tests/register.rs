//! `--register`: the server calls a client box's gateway over mutual TLS with its REAPI
//! certificate, says where its REAPI listener is and whether it leads, and withdraws when
//! the role goes or a stop signal comes. The gateway is the real one (`kbf-gateway`).

use std::future::pending;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_gateway::gateway::{self, Config};
use kbf_gateway::registry::{Clock, Leader, Registry};
use kbf_server::register::run;
use kbf_server::{Args, Readiness, start};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use tokio::sync::watch;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// The node name in the server's certificate.
const NODE: &str = "kbf1.farm.test";
/// Where the server says its REAPI listener is.
const ADVERTISED: &str = "192.0.2.10:8980";
/// A bound on waiting for a registration to show, so a broken test fails and does not
/// hang; nothing waits for it to pass.
const BOUND: Duration = Duration::from_secs(60);

/// A gateway on loopback and the files a server needs to register with it.
struct Farm {
    dir: PathBuf,
    gateway: SocketAddr,
    registry: Arc<Registry>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

fn pem(dir: &std::path::Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    path.to_str().expect("UTF-8").to_owned()
}

/// A CA, a gateway on loopback that trusts it, and the files `server.pem` (a server
/// certificate for [`NODE`]: serverAuth and clientAuth), `server.key` and `ca.pem`.
fn farm(name: &str) -> Farm {
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-server-register-{name}"));
    std::fs::create_dir_all(&dir).expect("directory");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "register test CA");
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("CA key")).expect("CA");
    let leaf = |name: &str, usages: Vec<ExtendedKeyUsagePurpose>| {
        let mut params = CertificateParams::new(vec![name.to_owned()]).expect("params");
        params.extended_key_usages = usages;
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    };
    let (gateway_cert, gateway_key) = leaf("localhost", vec![ExtendedKeyUsagePurpose::ServerAuth]);
    let (server_cert, server_key) = leaf(
        NODE,
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ],
    );
    pem(&dir, "ca.pem", &ca.pem());
    pem(&dir, "server.pem", &server_cert);
    pem(&dir, "server.key", &server_key);

    let config = Config {
        register_listen: "127.0.0.1:0".parse().expect("address"),
        tls: ServerTlsConfig::new()
            .identity(Identity::from_pem(gateway_cert, gateway_key))
            .client_ca_root(Certificate::from_pem(ca.pem())),
        xds_listen: "127.0.0.1:0".parse().expect("address"),
        ttl: Duration::from_secs(3600),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let bound = gateway::bind(config, Clock::system(), async move {
        let _ = stopped.await;
    })
    .expect("bind the gateway");
    let (gateway, registry) = (bound.registration, Arc::clone(&bound.registry));
    tokio::spawn(bound.serve());
    Farm {
        dir,
        gateway,
        registry,
        _stop: stop,
    }
}

impl Farm {
    fn path(&self, name: &str) -> String {
        self.dir.join(name).to_str().expect("UTF-8").to_owned()
    }

    /// The flags of a server that registers with this gateway every 100 ms.
    fn flags(&self) -> Vec<String> {
        let mut flags: Vec<String> = [
            "kbf-server",
            "--listen",
            "127.0.0.1:0",
            "--worker-listen",
            "127.0.0.1:0",
            "--reapi-tls-cert",
            self.path("server.pem").as_str(),
            "--reapi-tls-key",
            self.path("server.key").as_str(),
            "--register-ca-cert",
            self.path("ca.pem").as_str(),
            "--register-server-name",
            "localhost",
            "--register-interval-ms",
            "100",
            "--advertise-reapi",
            ADVERTISED,
            "--register",
        ]
        .map(str::to_owned)
        .into();
        flags.push(self.gateway.to_string());
        flags
    }
}

async fn leader_where(
    leaders: &mut watch::Receiver<Option<Leader>>,
    what: &str,
    condition: impl Fn(&Option<Leader>) -> bool,
) -> Option<Leader> {
    tokio::time::timeout(BOUND, leaders.wait_for(condition))
        .await
        .unwrap_or_else(|_| panic!("{what}: not seen within {BOUND:?}"))
        .expect("the registry is alive")
        .clone()
}

/// Catches: the registration call carrying anything but the certificate's node name, the
/// advertised address, the role and the term; a role change or a stop signal waiting for
/// the next interval (a leader that stopped leading would stay the leader until its
/// entry expired); and the flags not producing a working mutual-TLS client.
#[tokio::test]
async fn the_server_registers_its_role_and_withdraws() {
    let farm = farm("role");
    let args = Args::parse_from(farm.flags());
    let registration = args
        .registration()
        .expect("flags")
        .expect("registration on");
    assert_eq!(registration.interval, Duration::from_millis(100));
    let readiness = Arc::new(Readiness::default());
    let task = tokio::spawn(run(registration, Arc::clone(&readiness), 9));
    let mut leaders = farm.registry.subscribe();

    let led = leader_where(&mut leaders, "the leader", Option::is_some).await;
    assert_eq!(
        led,
        Some(Leader {
            name: NODE.to_owned(),
            address: ADVERTISED.parse().expect("address"),
            term: 9,
        })
    );
    readiness.set_leader(false);
    leader_where(&mut leaders, "the role given up", Option::is_none).await;
    readiness.set_leader(true);
    leader_where(&mut leaders, "the role taken again", Option::is_some).await;
    readiness.stop();
    leader_where(&mut leaders, "the stop signal", Option::is_none).await;
    assert_eq!(
        farm.registry.servers(),
        [NODE],
        "still registered, as a follower"
    );
    task.abort();
}

/// Catches: `start` not starting the registration it is given (the flags parse and the
/// config builds, but nothing calls the gateway).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_started_server_registers() {
    let farm = farm("start");
    let mut flags = farm.flags();
    flags.extend([
        "--cold-store".to_owned(),
        format!("file://{}", farm.path("cold")),
        "--meta-dir".to_owned(),
        farm.path("meta"),
    ]);
    let args = Args::parse_from(flags);
    let server = start(&args, || Ok(pending::<()>())).await.expect("start");
    tokio::spawn(async move { server.serve().await });
    let mut leaders = farm.registry.subscribe();
    let led = leader_where(&mut leaders, "the started server", Option::is_some)
        .await
        .expect("a leader");
    assert_eq!(led.name, NODE);
    assert_eq!(
        led.address,
        ADVERTISED.parse::<SocketAddr>().expect("address")
    );
}

/// Catches: registration on by default, or half configured: with no `--register` there
/// is no registration at all, and `--register` without the address to advertise, the
/// certificate to present or the CA to trust is refused when the flags are parsed.
#[test]
fn registration_is_off_unless_asked_for_and_needs_everything() {
    let off = Args::parse_from(["kbf-server"]);
    assert!(off.registration().expect("flags").is_none());
    let only_advertise = Args::try_parse_from(["kbf-server", "--advertise-reapi", ADVERTISED]);
    assert!(only_advertise.is_err(), "advertising without registering");

    let all = [
        "--register",
        "127.0.0.1:1",
        "--advertise-reapi",
        ADVERTISED,
        "--reapi-tls-cert",
        "server.pem",
        "--reapi-tls-key",
        "server.key",
        "--register-ca-cert",
        "ca.pem",
    ];
    assert!(Args::try_parse_from(["kbf-server"].into_iter().chain(all)).is_ok());
    for flag in [
        "--advertise-reapi",
        "--reapi-tls-cert",
        "--register-ca-cert",
    ] {
        let mut without = Vec::new();
        let mut words = all.iter();
        while let Some(word) = words.next() {
            if *word == flag {
                words.next();
            } else {
                without.push(*word);
            }
        }
        let parsed = Args::try_parse_from(["kbf-server"].into_iter().chain(without));
        assert!(parsed.is_err(), "--register without {flag} was accepted");
    }
}

/// Catches: a registration built from flags that parse but are unusable going unreported
/// until the first call: a missing CA file, and flags that bypassed the parser's checks.
#[test]
fn unusable_registration_flags_are_reported_when_the_config_is_built() {
    let mut args = Args::parse_from([
        "kbf-server",
        "--register",
        "127.0.0.1:1",
        "--advertise-reapi",
        ADVERTISED,
        "--reapi-tls-cert",
        "/nonexistent/server.pem",
        "--reapi-tls-key",
        "/nonexistent/server.key",
        "--register-ca-cert",
        "/nonexistent/ca.pem",
    ]);
    let error = args.registration().expect_err("the CA file is missing");
    assert!(error.to_string().contains("ca.pem"), "{error}");
    args.advertise_reapi = None;
    let error = args.registration().expect_err("nothing to advertise");
    assert!(error.to_string().contains("--advertise-reapi"), "{error}");
}
