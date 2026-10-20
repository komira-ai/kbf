//! What a frontend hands `/readyz` from the state store `bind_frontend` opened.

use std::sync::Arc;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};

use super::store_of;
use crate::StateStore;
use crate::farm::EpochPolicy;

/// Catches a frontend that probes a different store than the one it opened (or none when
/// one is open): `/readyz` must ask the very store the scheduler writes to, and ask
/// nothing when there is no store.
#[test]
fn readyz_probes_the_store_that_was_opened() {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = StateStore {
        store: Arc::clone(&store),
        epoch: EpochPolicy::Keep,
        members: crate::members::MembershipConfig::default(),
        history: None,
    };
    let probed = store_of(Some(&state)).expect("an open store is probed");
    assert!(Arc::ptr_eq(&probed, &store));
    assert!(store_of(None).is_none());
}

/// The flags of a frontend, from `more` on top of a storage node and listeners.
fn parsed(more: &[&str]) -> Result<super::FrontendFlags, clap::Error> {
    let mut argv = vec![
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:18980",
        "--worker-listen=127.0.0.1:18981",
        "--api-listen=127.0.0.1:18982",
    ];
    argv.extend_from_slice(more);
    super::parse(&super::command().try_get_matches_from(argv)?)
}

const HANDOVER: [&str; 2] = [
    "--handover-socket=/run/kbf/control.sock",
    "--state-store=sqlite:///x.db",
];

fn ports(flags: &super::FrontendFlags) -> [u16; 3] {
    let server = &flags.server;
    [
        server.listen.port(),
        server.worker_listen.port(),
        server.api_listen.expect("an API").port(),
    ]
}

/// Catches: instances that listen on the same ports (the second could not start beside
/// the first), or on a block that is not the one Envoy's configuration names: instance a
/// listens where the flags say, instance b ten ports above each (REAPI, worker and API
/// together), a port the system chooses stays chosen by it, and a port that cannot be
/// shifted is refused with the flag named, as is an instance without the socket. Mutants:
/// the offset applied to a only; one flag left out of the shift.
#[test]
fn instance_b_listens_ten_ports_above_instance_a() {
    let plain = parsed(&[]).expect("flags");
    assert_eq!(ports(&plain), [18980, 18981, 18982]);
    assert!(plain.handover.is_none());

    let [socket, state] = HANDOVER;
    let a = parsed(&[socket, state, "--instance=a"]).expect("flags");
    assert_eq!(ports(&a), [18980, 18981, 18982]);
    assert_eq!(
        a.handover.expect("a handover").instance,
        Some(super::Instance::A)
    );
    let b = parsed(&[socket, state, "--instance=b"]).expect("flags");
    assert_eq!(ports(&b), [18990, 18991, 18992]);
    assert_eq!(
        b.handover.expect("a handover").instance,
        Some(super::Instance::B)
    );
    let none = parsed(&[socket, state]).expect("flags");
    assert_eq!(none.handover.expect("a handover").instance, None);

    let any = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        socket,
        state,
        "--instance=b",
    ];
    let any = super::parse(&super::command().try_get_matches_from(any).expect("flags"));
    assert_eq!(ports(&any.expect("flags")), [0, 0, 0]);

    for (flag, high) in [
        (
            "listen",
            [
                "--listen=127.0.0.1:65535",
                "--worker-listen=127.0.0.1:1",
                "--api-listen=127.0.0.1:2",
            ],
        ),
        (
            "worker-listen",
            [
                "--listen=127.0.0.1:1",
                "--worker-listen=127.0.0.1:65530",
                "--api-listen=127.0.0.1:2",
            ],
        ),
        (
            "api-listen",
            [
                "--listen=127.0.0.1:1",
                "--worker-listen=127.0.0.1:2",
                "--api-listen=127.0.0.1:65526",
            ],
        ),
    ] {
        let argv = [
            "frontend",
            "--storage=http://127.0.0.1:1",
            high[0],
            high[1],
            high[2],
            socket,
            state,
            "--instance=b",
        ];
        let error = super::command()
            .try_get_matches_from(argv)
            .map_err(|e| e.to_string())
            .and_then(|m| super::parse(&m).map_err(|e| e.to_string()))
            .expect_err(flag);
        assert!(
            error.contains(&format!("--{flag}")) && error.contains("65535"),
            "{flag}: {error}"
        );
    }

    let lone = parsed(&["--instance=a"]).expect_err("no socket");
    assert!(lone.to_string().contains("--handover-socket"), "{lone}");
    let wrong = parsed(&[socket, state, "--instance=c"]).expect_err("not an instance");
    assert!(wrong.to_string().contains("a or b"), "{wrong}");
}

/// Catches: the settle read from the wrong flags, or not at all: the defaults are the box
/// render's (1 s interval, 1 s timeout, 2 failures: 1 x 2 + 1 + 1 = 4 s), each flag moves
/// it, a zero is refused, and the flags need the socket. Mutants: the plus-one-interval
/// dropped; the threshold not multiplied.
#[test]
fn the_settle_comes_from_envoys_check_settings() {
    let [socket, state] = HANDOVER;
    let default = parsed(&[socket, state])
        .expect("flags")
        .handover
        .expect("a handover");
    assert_eq!(default.envoy.settle(), std::time::Duration::from_secs(4));
    assert_eq!(default.watchdog, std::time::Duration::from_secs(15));
    let tuned = parsed(&[
        socket,
        state,
        "--envoy-check-interval-ms=500",
        "--envoy-check-timeout-ms=200",
        "--envoy-unhealthy-threshold=3",
        "--handover-watchdog-secs=7",
        "--handover-peer-name=kbf.example.internal",
    ])
    .expect("flags")
    .handover
    .expect("a handover");
    assert_eq!(
        tuned.envoy.settle(),
        std::time::Duration::from_millis(500 * 3 + 200 + 500)
    );
    assert_eq!(tuned.watchdog, std::time::Duration::from_secs(7));
    assert_eq!(tuned.peer_name.as_deref(), Some("kbf.example.internal"));
    for refused in [
        "--envoy-check-interval-ms=0",
        "--envoy-check-timeout-ms=0",
        "--envoy-unhealthy-threshold=0",
        "--handover-watchdog-secs=0",
    ] {
        assert!(parsed(&[socket, state, refused]).is_err(), "{refused}");
    }
    for lone in [
        "--envoy-check-interval-ms=5",
        "--handover-watchdog-secs=5",
        "--handover-peer-name=x",
    ] {
        assert!(parsed(&[lone]).is_err(), "{lone}");
    }
}

/// Catches: a successor reached the wrong way: plain text to a successor whose REAPI
/// listener has no certificate; and, for one that has, a channel made without the CA and
/// the name (it would send callers' credentials to anything), which is refused with the
/// flags it needs named.
#[tokio::test]
async fn the_successor_is_dialled_over_tls_only_with_a_ca_and_a_name() {
    let addr: std::net::SocketAddr = "127.0.0.1:18990".parse().expect("address");
    let [socket, state] = HANDOVER;
    let plain = parsed(&[socket, state]).expect("flags");
    let dial = super::dial_of(&plain).expect("a dialler");
    dial(addr).expect("a plain channel");

    // Storage over TLS does not make the REAPI listener's hop TLS: plain text still.
    let dir = crate::test_dir::scratch_dir("kbf-dial-plain");
    let file = |name: &str| {
        let path = dir.join(name);
        std::fs::write(&path, "pem").expect("write");
        path.display().to_string()
    };
    let argv = [
        "frontend".to_owned(),
        "--storage=https://storage.test:1".to_owned(),
        format!("--storage-client-cert={}", file("cert.pem")),
        format!("--storage-client-key={}", file("key.pem")),
        format!("--storage-ca={}", file("ca.pem")),
        socket.to_owned(),
        state.to_owned(),
    ];
    let matches = super::command().try_get_matches_from(argv).expect("flags");
    let storage_tls = super::parse(&matches).expect("a frontend's flags");
    let dial = super::dial_of(&storage_tls).expect("a dialler");
    dial(addr).expect("a plain channel");

    let tls = parsed(&[
        socket,
        state,
        "--reapi-tls-cert=/c.pem",
        "--reapi-tls-key=/k.pem",
    ])
    .expect("flags");
    let dial = super::dial_of(&tls).expect("a dialler");
    let refused = dial(addr).expect_err("no CA and no name");
    assert!(
        refused.to_string().contains("--handover-peer-name"),
        "{refused}"
    );
}

/// An `Execution` service that answers every `Execute` with one operation named for it,
/// the successor of a frontend that forwards its calls.
struct Stub;

#[tonic::async_trait]
impl kbf_proto::reapi::execution_server::Execution for Stub {
    type ExecuteStream = futures::stream::Iter<
        std::vec::IntoIter<Result<kbf_proto::google::longrunning::Operation, tonic::Status>>,
    >;
    type WaitExecutionStream = Self::ExecuteStream;

    async fn execute(
        &self,
        _: tonic::Request<kbf_proto::reapi::ExecuteRequest>,
    ) -> Result<tonic::Response<Self::ExecuteStream>, tonic::Status> {
        let operation = kbf_proto::google::longrunning::Operation {
            name: "operations/the-successor".to_owned(),
            ..Default::default()
        };
        Ok(tonic::Response::new(futures::stream::iter(vec![Ok(
            operation,
        )])))
    }

    async fn wait_execution(
        &self,
        _: tonic::Request<kbf_proto::reapi::WaitExecutionRequest>,
    ) -> Result<tonic::Response<Self::WaitExecutionStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("not scripted"))
    }
}

/// Catches: a frontend that forwards its calls over TLS to a successor it has not
/// verified (a process that took the successor's port could read every caller's
/// credentials): the channel `dial_of` makes trusts the `--storage-ca` and the name
/// `--handover-peer-name` gives, so a successor whose certificate is for that name is
/// reached, and one named something else is not. Mutants: no CA given to the endpoint (the
/// call fails); the domain name left out (the address is checked against the certificate
/// and the call fails); verification off (the other name is reached).
#[tokio::test]
async fn a_successor_over_tls_is_reached_only_under_the_name_its_certificate_has() {
    use kbf_proto::reapi::ExecuteRequest;
    use kbf_proto::reapi::execution_client::ExecutionClient;
    use kbf_proto::reapi::execution_server::ExecutionServer;
    use tonic::transport::server::TcpIncoming;
    use tonic::transport::{Identity, Server, ServerTlsConfig};

    let dir = crate::test_dir::scratch_dir("kbf-dial");
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let issue = |names: &[&str]| {
        let names = names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let params = rcgen::CertificateParams::new(names).expect("params");
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    };
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (server_cert, server_key) = issue(&["successor.test"]);
    let (client_cert, client_key) = issue(&["frontend.test"]);

    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("address")).expect("bind");
    let addr = incoming.local_addr().expect("address");
    let tls = ServerTlsConfig::new().identity(Identity::from_pem(server_cert, server_key));
    tokio::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("TLS")
            .add_service(ExecutionServer::new(Stub))
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });

    let flags = |name: &str| {
        let argv = [
            "frontend".to_owned(),
            "--storage=https://storage.test:1".to_owned(),
            format!(
                "--storage-client-cert={}",
                write("client.pem", &client_cert)
            ),
            format!("--storage-client-key={}", write("client.key", &client_key)),
            format!("--storage-ca={}", write("ca.pem", &ca.pem())),
            "--reapi-tls-cert=/server.pem".to_owned(),
            "--reapi-tls-key=/server.key".to_owned(),
            HANDOVER[0].to_owned(),
            HANDOVER[1].to_owned(),
            format!("--handover-peer-name={name}"),
        ];
        let matches = super::command().try_get_matches_from(argv).expect("flags");
        super::parse(&matches).expect("a frontend's flags")
    };
    let call = |name: &'static str| async move {
        let dial = super::dial_of(&flags(name)).expect("a dialler");
        let channel = dial(addr).expect("a channel");
        ExecutionClient::new(channel)
            .execute(ExecuteRequest::default())
            .await
            .map(tonic::Response::into_inner)
    };
    let mut stream = call("successor.test")
        .await
        .expect("the right name is reached");
    let operation = stream
        .message()
        .await
        .expect("a message")
        .expect("one operation");
    assert_eq!(operation.name, "operations/the-successor");
    let refused = call("other.test")
        .await
        .expect_err("another name is refused");
    assert_eq!(refused.code(), tonic::Code::Unavailable, "{refused}");
}

/// Catches: a frontend that starts with a listener it could not bind, for the operator API
/// as for the others (it would serve without the endpoint Envoy and operators ask): the
/// start fails naming the address. Mutant: the API socket's bind error dropped.
#[tokio::test]
async fn an_operator_api_address_in_use_fails_the_start() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let api = held.local_addr().expect("address");
    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        format!("--api-listen={api}"),
    ];
    let matches = super::command().try_get_matches_from(argv).expect("flags");
    let flags = super::parse(&matches).expect("a frontend's flags");
    let error = super::Prepared::new(&flags).err().expect("in use");
    assert!(
        error.to_string().contains(&format!("bind {api}")),
        "{error}"
    );
}

/// Catches: a frontend that does not bind the three listeners it was asked for, or binds
/// the operator API's on the REAPI's or worker's port. The unit-test build of `Prepared::new`
/// otherwise only ever sees its failure, so the success path (and the coverage of it in this
/// build) is this test's. Mutant: the API socket not bound (`api: None`).
#[tokio::test]
async fn a_frontend_binds_its_three_listeners_on_their_own_ports() {
    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--api-listen=127.0.0.1:0".to_owned(),
    ];
    let matches = super::command().try_get_matches_from(argv).expect("flags");
    let flags = super::parse(&matches).expect("a frontend's flags");
    let prepared = super::Prepared::new(&flags).expect("free ports bind");
    let api = prepared.sockets.api.as_ref().expect("the API is bound");
    let ports = [
        prepared.sockets.reapi.local.port(),
        prepared.sockets.worker.local.port(),
        api.local.port(),
    ];
    assert!(ports.iter().all(|port| *port != 0), "{ports:?}");
    assert!(
        ports[0] != ports[1] && ports[1] != ports[2] && ports[0] != ports[2],
        "{ports:?}"
    );
}
