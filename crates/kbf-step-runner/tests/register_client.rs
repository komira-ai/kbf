//! The registration client (`update-register`) against the real registration server, over a unix
//! socket in a directory of the test.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use kbf_step_runner::register::{
    Declared, Grant, HealthAddr, LINE_DEADLINE_SECS, MAX_LINE, Refused, Registry, Reported, Server,
    Why,
};
use support::{FakeClock, Scratch};
use update_register::{Error, Health, Refusal, register};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

struct Table(BTreeMap<String, Grant>);

impl Declared for Table {
    fn grant(&self, name: &str) -> Option<Grant> {
        self.0.get(name).cloned()
    }
}

/// A server on a socket, serving on a thread; a connection's thread is kept, so that a test can
/// wait for the server to have seen a connection close.
struct Live {
    scratch: Scratch,
    registry: Arc<Registry>,
    sock: PathBuf,
    stop: Arc<AtomicBool>,
    serving: Option<JoinHandle<()>>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Live {
    /// `svc` is declared for `uid` with the loopback ports 9301 and 9302.
    fn start(name: &str, uid: u32, tune: impl FnOnce(Server) -> Server) -> Live {
        let scratch = Scratch::new(name);
        let dir = scratch.path().join("run");
        let state = scratch.path().join("state");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
        let grant = Grant {
            uid,
            health_ports: vec![9301, 9302],
            registration_ttl_secs: 300,
        };
        let table = Arc::new(Table(BTreeMap::from([("svc".to_owned(), grant)])));
        let registry = Arc::new(Registry::load(&state, FakeClock::new(), &*table).unwrap());
        let sock = dir.join("s");
        let connections = Arc::new(Mutex::new(Vec::new()));
        let kept = connections.clone();
        let group = rustix::process::getegid().as_raw();
        let server = Server::bind(&sock, me(), group, registry.clone(), table)
            .unwrap()
            .with_spawner(Box::new(move |job| {
                let handle = std::thread::Builder::new().spawn(job)?;
                kept.lock().unwrap().push(handle);
                Ok(())
            }));
        let server = Arc::new(tune(server));
        let stop = Arc::new(AtomicBool::new(false));
        let serving = {
            let stop = stop.clone();
            std::thread::spawn(move || server.serve(&stop).unwrap())
        };
        Live {
            scratch,
            registry,
            sock,
            stop,
            serving: Some(serving),
            connections,
        }
    }

    fn register(
        &self,
        version: &str,
        health: Option<&Health>,
    ) -> Result<update_register::Registration, Error> {
        register(&self.sock, "svc", version, health)
    }

    /// Waits until every connection the server took has been served to its end; the clients of
    /// those connections must have been dropped.
    fn settle(&self) {
        let taken: Vec<_> = self.connections.lock().unwrap().drain(..).collect();
        for handle in taken {
            handle.join().unwrap();
        }
    }

    fn reported(&self, expected: Option<&str>) -> Reported {
        self.registry.report("svc", expected)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // One connection wakes the accept, which sees the flag.
        let _ = UnixStream::connect(&self.sock);
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
    }
}

fn health(addr: &str, path: &str) -> Health {
    Health::new(addr.parse().unwrap(), path)
}

fn refusal(result: Result<update_register::Registration, Error>) -> Refusal {
    match result {
        Err(Error::Refused(refusal)) => refusal,
        other => panic!("not a refusal: {other:?}"),
    }
}

/// A service that registers without a health address is registered, and is reported healthy for
/// its version while it holds the connection: not `Absent`, and with no health address kept.
#[test]
fn a_service_without_a_health_address_is_not_absent() {
    let live = Live::start("regc-nohealth", me(), |s| s);
    assert_eq!(live.reported(Some("2.0.0")), Reported::Absent);
    let held = live.register("2.0.0", None).unwrap();
    assert_eq!(live.reported(Some("2.0.0")), Reported::Healthy);
    assert_eq!(live.reported(None), Reported::Healthy);
    assert_eq!(live.registry.health_addr("svc"), None);
    drop(held);
}

/// The health address and path the client sends are the ones the registry keeps, for IPv4 and
/// IPv6 loopback.
#[test]
fn the_health_address_arrives_as_sent() {
    for (addr, port) in [("127.0.0.1:9301", 9301), ("[::1]:9302", 9302)] {
        let live = Live::start("regc-health", me(), |s| s);
        let held = live
            .register("1.0.0", Some(&health(addr, "/readyz")))
            .unwrap();
        let kept = live.registry.health_addr("svc").unwrap();
        assert_eq!(
            kept,
            HealthAddr {
                addr: addr.to_owned(),
                path: "/readyz".to_owned()
            }
        );
        assert_eq!(
            kept.addr.parse::<std::net::SocketAddr>().unwrap().port(),
            port
        );
        assert_eq!(live.reported(Some("1.0.0")), Reported::Healthy);
        drop(held);
    }
}

/// The version the client sends is the one believed: another staged version is a mismatch.
#[test]
fn a_version_other_than_the_staged_one_is_a_mismatch() {
    let live = Live::start("regc-version", me(), |s| s);
    let held = live.register("1.9.0", None).unwrap();
    assert_eq!(
        live.reported(Some("2.0.0")),
        Reported::Unhealthy(Why::VersionMismatch {
            running: "1.9.0".to_owned()
        })
    );
    drop(held);
}

/// The registration is the connection: dropping it is the service stopping, and registering
/// again brings it back.
#[test]
fn dropping_the_registration_is_the_service_stopping() {
    let live = Live::start("regc-drop", me(), |s| s);
    let held = live.register("2.0.0", None).unwrap();
    assert_eq!(live.reported(Some("2.0.0")), Reported::Healthy);
    drop(held);
    live.settle();
    assert_eq!(
        live.reported(Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );

    let again = live.register("2.0.0", None).unwrap();
    assert_eq!(live.reported(Some("2.0.0")), Reported::Healthy);
    drop(again);
    live.settle();
    assert_eq!(
        live.reported(Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
}

/// A client whose user is not the one the declaration names is refused, and nothing is recorded.
#[test]
fn a_client_with_the_wrong_uid_is_refused() {
    let live = Live::start("regc-uid", me().wrapping_add(1), |s| s);
    assert_eq!(refusal(live.register("2.0.0", None)), Refusal::WrongUser);
    assert_eq!(live.reported(Some("2.0.0")), Reported::Absent);
}

/// A name no declaration names is refused.
#[test]
fn a_service_nothing_declares_is_refused() {
    let live = Live::start("regc-unknown", me(), |s| s);
    let got = register(&live.sock, "stranger", "1.0.0", None);
    assert_eq!(refusal(got), Refusal::UnknownService);
    assert_eq!(live.reported(None), Reported::Absent);
}

/// A health address that is not loopback, a path that is not one, and a port the declaration
/// does not grant are each their own refusal.
#[test]
fn a_health_address_the_declaration_does_not_allow_is_refused() {
    let live = Live::start("regc-badhealth", me(), |s| s);
    let cases = [
        (health("0.0.0.0:9301", "/readyz"), Refusal::HealthAddress),
        (health("127.0.0.1:9301", "readyz"), Refusal::HealthAddress),
        (
            health("127.0.0.1:9301", "/ready?x=1"),
            Refusal::HealthAddress,
        ),
        (health("127.0.0.1:9999", "/readyz"), Refusal::HealthPort),
    ];
    for (bad, want) in cases {
        assert_eq!(refusal(live.register("2.0.0", Some(&bad))), want, "{bad:?}");
    }
    assert_eq!(live.reported(None), Reported::Absent);
}

/// A server at its limit answers `busy`, and the client reads it as that.
#[test]
fn a_full_server_is_busy() {
    let live = Live::start("regc-busy", me(), |s| s.with_max_connections(0));
    assert_eq!(refusal(live.register("2.0.0", None)), Refusal::Busy);
    assert_eq!(live.reported(None), Reported::Absent);
}

/// A server that cannot keep the registration (its state directory is gone) says so, and the
/// registration is not believed.
#[test]
fn a_server_that_cannot_keep_the_registration_says_so() {
    let live = Live::start("regc-internal", me(), |s| s);
    fs::remove_dir_all(live.scratch.path().join("state")).unwrap();
    assert_eq!(refusal(live.register("2.0.0", None)), Refusal::Internal);
    live.settle();
    assert_eq!(live.reported(None), Reported::Absent);
}

/// The reasons the client knows are the ones the server writes: each token is the same text.
#[test]
fn the_client_and_the_server_agree_on_every_reason() {
    let pairs = [
        (Refusal::Malformed, Refused::Malformed),
        (Refusal::UnknownService, Refused::UnknownService),
        (Refusal::WrongUser, Refused::WrongUser),
        (Refusal::HealthAddress, Refused::HealthAddress),
        (Refusal::HealthPort, Refused::HealthPort),
        (Refusal::Busy, Refused::Busy),
    ];
    for (client, server) in pairs {
        assert_eq!(client.token(), server.token());
    }
}

/// A registration of exactly the longest line the server reads goes through; the client's limit
/// is the server's, and one byte more is refused by the client without a connection to read it.
#[test]
fn the_longest_line_the_server_reads_is_the_longest_the_client_sends() {
    assert_eq!(update_register::MAX_LINE, MAX_LINE);
    let live = Live::start("regc-long", me(), |s| s);
    let overhead = r#"{"health":null,"name":"svc","version":""}"#.len() + 1;
    let version = "9".repeat(MAX_LINE - overhead);
    let held = live.register(&version, None).unwrap();
    assert_eq!(live.reported(Some(&version)), Reported::Healthy);
    drop(held);

    let over = "9".repeat(MAX_LINE - overhead + 1);
    assert!(matches!(
        live.register(&over, None),
        Err(Error::TooLong { len }) if len == MAX_LINE + 1
    ));
}

/// A socket path nothing listens on is a connect error that names it.
#[test]
fn a_socket_nobody_listens_on_is_a_connect_error() {
    let scratch = Scratch::new("regc-nosock");
    let sock: &Path = &scratch.path().join("none");
    match register(sock, "svc", "1.0.0", None) {
        Err(Error::Connect { path, .. }) => assert_eq!(path, sock),
        other => panic!("{other:?}"),
    }
}

/// The client waits for the answer as long as the server takes to read a line.
#[test]
fn the_client_waits_as_long_as_the_server_reads_a_line() {
    assert_eq!(update_register::ANSWER_DEADLINE_SECS, LINE_DEADLINE_SECS);
}
