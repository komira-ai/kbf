//! The registration socket: liveness and version, and nothing a peer can use to run anything.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use kbf_step_runner::register::{
    Declared, Grant, HealthAddr, MAX_LINE, REGISTRATIONS_FILE, Refused, Registration, Registry,
    RegistryError, Reported, Server, Why,
};
use support::{FakeClock, Scratch};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

struct Table(Mutex<BTreeMap<String, Grant>>);

impl Declared for Table {
    fn grant(&self, name: &str) -> Option<Grant> {
        self.0.lock().unwrap().get(name).cloned()
    }
}

fn grant(uid: u32) -> Grant {
    Grant {
        uid,
        health_ports: vec![9301, 9302],
        registration_ttl_secs: 300,
    }
}

struct Rig {
    scratch: Scratch,
    clock: Arc<FakeClock>,
    registry: Arc<Registry>,
    server: Arc<Server>,
    sock: PathBuf,
}

impl Rig {
    fn new(name: &str, uid: u32) -> Rig {
        let scratch = Scratch::new(name);
        let dir = scratch.path().join("run");
        let state = scratch.path().join("state");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
        let clock = FakeClock::new();
        let registry = Arc::new(Registry::load(&state, clock.clone()).unwrap());
        let table = Table(Mutex::new(BTreeMap::from([("svc".to_owned(), grant(uid))])));
        let sock = dir.join("s");
        let server =
            Arc::new(Server::bind(&sock, me(), None, registry.clone(), Arc::new(table)).unwrap());
        Rig {
            scratch,
            clock,
            registry,
            server,
            sock,
        }
    }

    /// One connection served on a thread, over a socket pair.
    fn connect(&self) -> (UnixStream, JoinHandle<Result<(), RegistryError>>) {
        let (client, served) = UnixStream::pair().unwrap();
        let server = self.server.clone();
        (
            client,
            std::thread::spawn(move || server.serve_connection(served)),
        )
    }
}

fn say(client: &UnixStream, line: &str) -> serde_json::Value {
    let mut out = client;
    out.write_all(line.as_bytes()).unwrap();
    out.flush().unwrap();
    answer(client)
}

fn answer(client: &UnixStream) -> serde_json::Value {
    let mut reply = String::new();
    BufReader::new(client).read_line(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

fn registration(version: &str, health: Option<(&str, &str)>) -> String {
    let reg = Registration {
        name: "svc".to_owned(),
        version: version.to_owned(),
        health: health.map(|(addr, path)| HealthAddr {
            addr: addr.to_owned(),
            path: path.to_owned(),
        }),
    };
    format!("{}\n", serde_json::to_string(&reg).unwrap())
}

fn refused(rig: &Rig, line: &str) -> String {
    let (client, served) = rig.connect();
    let reply = say(&client, line);
    served.join().unwrap().unwrap();
    assert_eq!(reply["ok"], false, "{line}");
    reply["error"].as_str().unwrap().to_owned()
}

/// Catches a registry that does not follow the connection: never registered is `Absent`; a service
/// that registered is `Healthy` for its staged version and `Unhealthy(VersionMismatch)` for
/// another (a version is never believed); when the connection closes it is `Unhealthy(Stopped)`.
#[test]
fn liveness_and_version_are_what_the_updater_reports() {
    let rig = Rig::new("reg-live", me());
    assert_eq!(rig.registry.report("svc", Some("2.0.0")), Reported::Absent);
    let (client, served) = rig.connect();
    assert_eq!(say(&client, &registration("2.0.0", None))["ok"], true);
    assert_eq!(rig.registry.report("svc", Some("2.0.0")), Reported::Healthy);
    assert_eq!(rig.registry.report("svc", None), Reported::Healthy);
    assert_eq!(
        rig.registry.report("svc", Some("3.0.0")),
        Reported::Unhealthy(Why::VersionMismatch {
            running: "2.0.0".to_owned()
        })
    );
    assert_eq!(
        rig.registry.report("other", Some("2.0.0")),
        Reported::Absent
    );
    drop(client);
    served.join().unwrap().unwrap();
    assert_eq!(
        rig.registry.report("svc", Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
}

/// Catches a connection that closes after its registration was replaced stopping the new one: the
/// second registration of the name wins, and only its connection's close stops the service.
#[test]
fn a_second_registration_replaces_the_first() {
    let rig = Rig::new("reg-replace", me());
    let (first, first_served) = rig.connect();
    say(&first, &registration("2.0.0", None));
    let (second, second_served) = rig.connect();
    say(&second, &registration("2.0.1", None));
    drop(first);
    first_served.join().unwrap().unwrap();
    assert_eq!(rig.registry.report("svc", Some("2.0.1")), Reported::Healthy);
    drop(second);
    second_served.join().unwrap().unwrap();
    assert_eq!(
        rig.registry.report("svc", Some("2.0.1")),
        Reported::Unhealthy(Why::Stopped)
    );
}

/// Catches a peer of another user being believed: the declaration names the user a service
/// registers as, `SO_PEERCRED` is compared with it, and a mismatch is refused and registers
/// nothing. A name nothing declares is refused too.
#[test]
fn a_peer_that_is_not_the_declared_user_is_refused() {
    let rig = Rig::new("reg-user", me().wrapping_add(1));
    assert_eq!(refused(&rig, &registration("2.0.0", None)), "wrong_user");
    assert_eq!(rig.registry.report("svc", None), Reported::Absent);
    let rig = Rig::new("reg-unknown", me());
    let line = r#"{"name":"nobody","version":"1"}"#;
    assert_eq!(refused(&rig, &format!("{line}\n")), "unknown_service");
}

/// Catches a registration that is accepted although it is not one JSON object within the limit:
/// garbage, an object missing a field, a command in a field the message does not have, an
/// empty message, bytes that are not text, and a line over the limit are all `malformed`.
#[test]
fn what_is_not_a_registration_is_malformed() {
    let rig = Rig::new("reg-malformed", me());
    for line in ["garbage\n", "{\"name\":\"svc\"}\n", "\n", "[1,2]\n"] {
        assert_eq!(refused(&rig, line), "malformed", "{line}");
    }
    // An empty message: the peer closes its writing side at once.
    let (client, served) = rig.connect();
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(answer(&client)["error"], "malformed");
    served.join().unwrap().unwrap();
    // Bytes that are not text.
    let (client, served) = rig.connect();
    (&client).write_all(&[0xff, 0xfe, b'\n']).unwrap();
    assert_eq!(answer(&client)["error"], "malformed");
    served.join().unwrap().unwrap();
    // A line over the limit, never ended.
    let (client, served) = rig.connect();
    (&client).write_all(&vec![b'a'; MAX_LINE + 100]).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(answer(&client)["error"], "malformed");
    served.join().unwrap().unwrap();
    assert_eq!(rig.registry.report("svc", None), Reported::Absent);
    for refusal in [
        Refused::Malformed,
        Refused::UnknownService,
        Refused::WrongUser,
        Refused::HealthAddress,
        Refused::HealthPort,
    ] {
        assert!(!refusal.to_string().is_empty());
        assert!(!refusal.token().contains(' '));
    }
}

/// Catches a health address that is not confined: it must be loopback with a plain path and a port
/// the declaration grants; IPv4 and IPv6 loopback are accepted and the address is kept.
#[test]
fn the_health_address_must_be_loopback_and_granted() {
    let rig = Rig::new("reg-health", me());
    for (addr, path, error) in [
        ("10.0.0.1:9301", "/ready", "health_address"),
        ("localhost:9301", "/ready", "health_address"),
        ("0.0.0.0:9301", "/ready", "health_address"),
        ("127.0.0.1:9301", "ready", "health_address"),
        ("127.0.0.1:9301", "/ready?x=1", "health_address"),
        ("127.0.0.1:9301", "/a b", "health_address"),
        ("127.0.0.1:9301", "/a#b", "health_address"),
        ("127.0.0.1:9999", "/ready", "health_port"),
    ] {
        let line = registration("2.0.0", Some((addr, path)));
        assert_eq!(refused(&rig, &line), error, "{addr} {path}");
    }
    assert_eq!(rig.registry.health_addr("svc"), None);
    for addr in ["127.0.0.1:9301", "[::1]:9302"] {
        let (client, served) = rig.connect();
        assert_eq!(
            say(&client, &registration("2.0.0", Some((addr, "/ready"))))["ok"],
            true
        );
        assert_eq!(
            rig.registry.health_addr("svc"),
            Some(HealthAddr {
                addr: addr.to_owned(),
                path: "/ready".to_owned()
            })
        );
        drop(client);
        served.join().unwrap().unwrap();
    }
    assert_eq!(rig.registry.health_addr("nobody"), None);
}

/// Catches a registration that does not survive an updater restart, or is believed for ever
/// afterwards: it is written durably, loaded as last known, `Unhealthy(Stopped)` once
/// `registration_ttl_secs` have passed without the service reconnecting, and `Healthy` again when
/// it does.
#[test]
fn a_registration_is_kept_across_a_restart_until_its_ttl() {
    let rig = Rig::new("reg-persist", me());
    let (client, served) = rig.connect();
    say(
        &client,
        &registration("2.0.0", Some(("127.0.0.1:9301", "/ready"))),
    );
    drop(client);
    served.join().unwrap().unwrap();

    let state = rig.scratch.path().join("state");
    let after = Arc::new(Registry::load(&state, rig.clock.clone()).unwrap());
    assert_eq!(after.report("svc", Some("2.0.0")), Reported::Healthy);
    assert_eq!(
        after.health_addr("svc").map(|h| h.addr),
        Some("127.0.0.1:9301".to_owned())
    );
    rig.clock.advance(299);
    assert_eq!(after.report("svc", Some("2.0.0")), Reported::Healthy);
    rig.clock.advance(1);
    assert_eq!(
        after.report("svc", Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
    after
        .register(
            me(),
            &serde_json::from_str(registration("2.0.0", None).trim()).unwrap(),
            &grant(me()),
        )
        .unwrap();
    assert_eq!(after.report("svc", Some("2.0.0")), Reported::Healthy);
}

/// Catches a registry file that is read as empty when it is damaged or unreadable.
#[test]
fn a_damaged_registrations_file_is_an_error() {
    let dir = Scratch::new("reg-damaged");
    let clock = FakeClock::new();
    assert!(Registry::load(dir.path(), clock.clone()).is_ok());
    fs::write(dir.path().join(REGISTRATIONS_FILE), "{").unwrap();
    let err = Registry::load(dir.path(), clock.clone()).err().unwrap();
    assert!(matches!(err, RegistryError::Corrupt(_)));
    assert!(err.to_string().contains("not readable"));
    fs::remove_file(dir.path().join(REGISTRATIONS_FILE)).unwrap();
    fs::create_dir(dir.path().join(REGISTRATIONS_FILE)).unwrap();
    let err = Registry::load(dir.path(), clock).err().unwrap();
    assert!(matches!(err, RegistryError::Io(_)));
}

/// Catches an accepted registration that could not be written being acknowledged: the service
/// is told `internal` and the connection ends in an error.
#[test]
fn a_registration_that_cannot_be_kept_is_not_acknowledged() {
    let rig = Rig::new("reg-unwritable", me());
    fs::remove_dir_all(rig.scratch.path().join("state")).unwrap();
    let (client, served) = rig.connect();
    assert_eq!(
        say(&client, &registration("2.0.0", None))["error"],
        "internal"
    );
    assert!(matches!(served.join().unwrap(), Err(RegistryError::Io(_))));
}

/// Catches a socket directory that others can enter, that another user owns, that is a link, or is
/// missing; a socket another server answers on; and a path the system cannot connect to.
#[test]
fn the_socket_directory_is_checked() {
    let scratch = Scratch::new("reg-dir");
    let state = scratch.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let clock = FakeClock::new();
    let registry = Arc::new(Registry::load(&state, clock).unwrap());
    let table: Arc<dyn Declared> = Arc::new(Table(Mutex::new(BTreeMap::new())));
    let bind =
        |path: &Path, owner: u32| Server::bind(path, owner, None, registry.clone(), table.clone());

    let open = scratch.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    let err = bind(&open.join("s"), me()).err().unwrap();
    assert!(matches!(err, RegistryError::SocketDir(_)));
    assert!(err.to_string().contains("socket directory"));

    let closed = scratch.path().join("closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(matches!(
        bind(&closed.join("s"), me().wrapping_add(1)).err().unwrap(),
        RegistryError::SocketDir(_)
    ));

    let link = scratch.path().join("link");
    std::os::unix::fs::symlink(&closed, &link).unwrap();
    assert!(matches!(
        bind(&link.join("s"), me()).err().unwrap(),
        RegistryError::SocketDir(_)
    ));
    assert!(matches!(
        bind(&scratch.path().join("absent").join("s"), me())
            .err()
            .unwrap(),
        RegistryError::Io(_)
    ));

    // A socket nobody answers on is stale and replaced; one that answers is not.
    let sock = closed.join("s");
    drop(UnixListener::bind(&sock).unwrap());
    let live = bind(&sock, me()).unwrap();
    let err = bind(&sock, me()).err().unwrap();
    assert!(matches!(err, RegistryError::AlreadyServing(_)));
    assert!(err.to_string().contains("already answers"));
    assert_eq!(
        fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o660
    );
    drop(live);

    // A path too long for a socket address cannot even be tried.
    let long = closed.join("x".repeat(150));
    assert!(matches!(
        bind(&long, me()).err().unwrap(),
        RegistryError::Io(_)
    ));
}

/// Catches a group that is not applied to the socket: with a group given the socket is owned by it.
#[test]
fn the_socket_takes_the_given_group() {
    let scratch = Scratch::new("reg-group");
    let state = scratch.path().join("state");
    let run = scratch.path().join("run");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o750)).unwrap();
    let registry = Arc::new(Registry::load(&state, FakeClock::new()).unwrap());
    let table: Arc<dyn Declared> = Arc::new(Table(Mutex::new(BTreeMap::new())));
    let gid = rustix::process::getegid().as_raw();
    let _server = Server::bind(&run.join("s"), me(), Some(gid), registry, table).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(fs::metadata(run.join("s")).unwrap().gid(), gid);
}

/// Catches an accept loop that does not serve, or does not end when told: connections to the real
/// socket are served on threads, and the loop returns once it is asked to stop and woken.
#[test]
fn the_server_accepts_on_the_socket_until_stopped() {
    let rig = Rig::new("reg-serve", me());
    let stop = Arc::new(AtomicBool::new(false));
    let (server, flag) = (rig.server.clone(), stop.clone());
    let loop_thread = std::thread::spawn(move || server.serve(&flag));
    let client = UnixStream::connect(&rig.sock).unwrap();
    assert_eq!(say(&client, &registration("2.0.0", None))["ok"], true);
    assert_eq!(rig.registry.report("svc", Some("2.0.0")), Reported::Healthy);
    stop.store(true, Ordering::Release);
    drop(UnixStream::connect(&rig.sock).unwrap());
    loop_thread.join().unwrap().unwrap();
    drop(client);
}
