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
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use kbf_step_runner::register::{
    Declared, Grant, HealthAddr, MAX_LINE, REGISTRATIONS_FILE, Refused, Registration, Registry,
    RegistryError, Reported, Server, Why,
};
use support::{FakeClock, Scratch};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The group the test runs as: the group of the directories it makes.
fn my_group() -> u32 {
    rustix::process::getegid().as_raw()
}

struct Table(Mutex<BTreeMap<String, Grant>>);

/// `svc` declared for `uid`.
fn declared(uid: u32) -> Table {
    Table(Mutex::new(BTreeMap::from([("svc".to_owned(), grant(uid))])))
}

/// Nothing declared.
fn nothing() -> Table {
    Table(Mutex::new(BTreeMap::new()))
}

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
    table: Arc<Table>,
}

impl Rig {
    fn new(name: &str, uid: u32) -> Rig {
        Rig::capped(name, uid, None)
    }

    fn capped(name: &str, uid: u32, cap: Option<usize>) -> Rig {
        let scratch = Scratch::new(name);
        let dir = scratch.path().join("run");
        let state = scratch.path().join("state");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();
        let clock = FakeClock::new();
        let table = Arc::new(declared(uid));
        let registry = Arc::new(Registry::load(&state, clock.clone(), &*table).unwrap());
        let sock = dir.join("s");
        let mut server =
            Server::bind(&sock, me(), my_group(), registry.clone(), table.clone()).unwrap();
        if let Some(cap) = cap {
            server = server.with_max_connections(cap);
        }
        let server = Arc::new(server);
        Rig {
            scratch,
            clock,
            registry,
            server,
            sock,
            table,
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
    // Whatever else the peer sends while it is connected changes nothing.
    (&client)
        .write_all(b"{\"name\":\"svc\",\"version\":\"9\"}\n")
        .unwrap();
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
    // A field the message does not have (a command, say) is not passed over.
    let (client, served) = rig.connect();
    let reply = say(
        &client,
        "{\"name\":\"svc\",\"version\":\"2.0.0\",\"exec\":\"reboot\"}\n",
    );
    drop(client);
    served.join().unwrap().unwrap();
    assert_eq!(reply["error"], "malformed", "{reply}");
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
        ("[::]:9301", "/ready", "health_address"),
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
    // The service is connected while the updater restarts.

    let state = rig.scratch.path().join("state");
    let after = Arc::new(Registry::load(&state, rig.clock.clone(), &*rig.table).unwrap());
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
    drop(client);
    served.join().unwrap().unwrap();
}

/// Catches a registry file that is read as empty when it is damaged or unreadable.
#[test]
fn a_damaged_registrations_file_is_an_error() {
    let dir = Scratch::new("reg-damaged");
    let clock = FakeClock::new();
    assert!(Registry::load(dir.path(), clock.clone(), &nothing()).is_ok());
    fs::write(dir.path().join(REGISTRATIONS_FILE), "{").unwrap();
    let err = Registry::load(dir.path(), clock.clone(), &nothing())
        .err()
        .unwrap();
    assert!(matches!(err, RegistryError::Corrupt(_)));
    assert!(err.to_string().contains("not readable"));
    fs::remove_file(dir.path().join(REGISTRATIONS_FILE)).unwrap();
    fs::create_dir(dir.path().join(REGISTRATIONS_FILE)).unwrap();
    let err = Registry::load(dir.path(), clock, &nothing()).err().unwrap();
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
    // The registration that could not be kept is not believed either.
    assert_eq!(rig.registry.report("svc", None), Reported::Absent);
}

/// Catches a registration that was refused for being unwritable replacing the one that stood: the
/// earlier registration is restored, and still reported as it was.
#[test]
fn a_registration_that_cannot_be_kept_leaves_the_earlier_one_standing() {
    let rig = Rig::new("reg-restore", me());
    let reg = |version: &str| -> Registration {
        serde_json::from_str(registration(version, None).trim()).unwrap()
    };
    rig.registry
        .register(me(), &reg("2.0.0"), &grant(me()))
        .unwrap();
    fs::remove_dir_all(rig.scratch.path().join("state")).unwrap();
    let err = rig.registry.register(me(), &reg("2.0.1"), &grant(me()));
    assert!(matches!(err, Err(RegistryError::Io(_))));
    assert_eq!(rig.registry.report("svc", Some("2.0.0")), Reported::Healthy);
    assert_eq!(
        rig.registry.report("svc", Some("2.0.1")),
        Reported::Unhealthy(Why::VersionMismatch {
            running: "2.0.0".to_owned()
        })
    );
}

/// Catches a socket directory that others can enter, that another user owns, that is a link, or is
/// missing; a socket another server answers on; and a path the system cannot connect to.
#[test]
fn the_socket_directory_is_checked() {
    let scratch = Scratch::new("reg-dir");
    let state = scratch.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let clock = FakeClock::new();
    let registry = Arc::new(Registry::load(&state, clock, &nothing()).unwrap());
    let table: Arc<dyn Declared> = Arc::new(nothing());
    let bind = |path: &Path, owner: u32| {
        Server::bind(path, owner, my_group(), registry.clone(), table.clone())
    };

    let open = scratch.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    let err = bind(&open.join("s"), me()).err().unwrap();
    assert!(matches!(err, RegistryError::SocketDir(_)));
    assert!(err.to_string().contains("socket directory"));

    let group_writable = scratch.path().join("gw");
    fs::create_dir(&group_writable).unwrap();
    fs::set_permissions(&group_writable, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(matches!(
        bind(&group_writable.join("s"), me()).err().unwrap(),
        RegistryError::SocketDir(_)
    ));

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

/// Catches a group that is not applied to the socket: the socket is owned by the register group.
#[test]
fn the_socket_takes_the_given_group() {
    let scratch = Scratch::new("reg-group");
    let state = scratch.path().join("state");
    let run = scratch.path().join("run");
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o750)).unwrap();
    let registry = Arc::new(Registry::load(&state, FakeClock::new(), &nothing()).unwrap());
    let table: Arc<dyn Declared> = Arc::new(nothing());
    let gid = my_group();
    let _server = Server::bind(&run.join("s"), me(), gid, registry, table).unwrap();
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

/// Catches a server that serves any number of connections: with a limit of one, a second peer is
/// answered `busy` and closed while the first still holds its registration.
#[test]
fn connections_over_the_limit_are_answered_busy() {
    let rig = Rig::capped("reg-cap", me(), Some(1));
    let stop = Arc::new(AtomicBool::new(false));
    let (server, flag) = (rig.server.clone(), stop.clone());
    let loop_thread = std::thread::spawn(move || server.serve(&flag));
    let first = UnixStream::connect(&rig.sock).unwrap();
    assert_eq!(say(&first, &registration("2.0.0", None))["ok"], true);
    let second = UnixStream::connect(&rig.sock).unwrap();
    second.shutdown(Shutdown::Write).unwrap();
    assert_eq!(answer(&second)["error"], "busy");
    assert_eq!(rig.registry.report("svc", Some("2.0.0")), Reported::Healthy);
    stop.store(true, Ordering::Release);
    drop(UnixStream::connect(&rig.sock).unwrap());
    loop_thread.join().unwrap().unwrap();
    drop(first);
}

/// Catches a registered peer that can talk for ever: more than the size limit sent after the
/// registration ends the connection (the service is then stopped) although the peer never closes it.
#[test]
fn a_registered_peer_that_floods_is_dropped() {
    let rig = Rig::new("reg-flood", me());
    let (client, served) = rig.connect();
    assert_eq!(say(&client, &registration("2.0.0", None))["ok"], true);
    (&client).write_all(&vec![b'a'; MAX_LINE + 100]).unwrap();
    served.join().unwrap().unwrap();
    assert_eq!(
        rig.registry.report("svc", Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
    drop(client);
}

/// Catches `stopped` being forgotten across a restart: a service whose connection closed is still
/// stopped when the registrations are loaded again, not believed healthy until its time-to-live.
#[test]
fn a_stopped_service_is_still_stopped_after_a_restart() {
    let rig = Rig::new("reg-stopped-persist", me());
    let (client, served) = rig.connect();
    say(&client, &registration("2.0.0", None));
    drop(client);
    served.join().unwrap().unwrap();
    let state = rig.scratch.path().join("state");
    let after = Registry::load(&state, rig.clock.clone(), &*rig.table).unwrap();
    assert_eq!(
        after.report("svc", Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
}

/// Catches a close that cannot be recorded passing as handled: the error reaches the caller.
#[test]
fn a_close_that_cannot_be_kept_is_an_error() {
    let rig = Rig::new("reg-stopped-unwritable", me());
    let (client, served) = rig.connect();
    say(&client, &registration("2.0.0", None));
    fs::remove_dir_all(rig.scratch.path().join("state")).unwrap();
    drop(client);
    assert!(matches!(served.join().unwrap(), Err(RegistryError::Io(_))));
}

fn serve_in_thread(rig: &Rig) -> (Arc<AtomicBool>, JoinHandle<std::io::Result<()>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (server, flag) = (rig.server.clone(), stop.clone());
    (stop, std::thread::spawn(move || server.serve(&flag)))
}

fn finish(rig: &Rig, stop: Arc<AtomicBool>, serving: JoinHandle<std::io::Result<()>>) {
    stop.store(true, Ordering::Release);
    drop(UnixStream::connect(&rig.sock).unwrap());
    serving.join().unwrap().unwrap();
}

/// Catches one user using every slot: with a limit of one per user, a second connection of the
/// same user is answered `busy` while the global limit is far off.
#[test]
fn one_user_cannot_hold_every_connection() {
    let rig = Rig::new("reg-per-uid", me());
    let server = Server::bind(
        &rig.scratch.path().join("run").join("p"),
        me(),
        my_group(),
        rig.registry.clone(),
        Arc::new(declared(me())),
    )
    .unwrap()
    .with_max_per_uid(1);
    let path = rig.scratch.path().join("run").join("p");
    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    let (s, flag) = (server.clone(), stop.clone());
    let serving = std::thread::spawn(move || s.serve(&flag));
    let first = UnixStream::connect(&path).unwrap();
    assert_eq!(say(&first, &registration("2.0.0", None))["ok"], true);
    let second = UnixStream::connect(&path).unwrap();
    second.shutdown(Shutdown::Write).unwrap();
    assert_eq!(answer(&second)["error"], "busy");
    stop.store(true, Ordering::Release);
    drop(UnixStream::connect(&path).unwrap());
    serving.join().unwrap().unwrap();
    drop(first);
}

/// Catches a connection whose thread cannot be started being dropped silently: the peer is
/// answered `busy`.
#[test]
fn a_connection_that_gets_no_thread_is_answered_busy() {
    let mut rig = Rig::new("reg-no-thread", me());
    let server = Arc::try_unwrap(rig.server)
        .ok()
        .unwrap()
        .with_spawner(Box::new(|_job| Err(std::io::Error::other("no threads"))));
    rig.server = Arc::new(server);
    let (stop, serving) = serve_in_thread(&rig);
    let client = UnixStream::connect(&rig.sock).unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    assert_eq!(answer(&client)["error"], "busy");
    finish(&rig, stop, serving);
}

/// Catches a line deadline that is only a per-read timeout: a registration line has a total time
/// to arrive, and one that has none left is refused as malformed (a zero deadline, so that
/// nothing waits).
#[test]
fn a_line_that_is_out_of_time_is_refused() {
    let mut rig = Rig::new("reg-deadline", me());
    let server = Arc::try_unwrap(rig.server)
        .ok()
        .unwrap()
        .with_line_deadline(std::time::Duration::ZERO);
    rig.server = Arc::new(server);
    assert_eq!(refused(&rig, &registration("2.0.0", None)), "malformed");
    assert_eq!(rig.registry.report("svc", None), Reported::Absent);
}

/// Catches an accept loop that ends on a passing error: running out of descriptors or an
/// interrupted call is waited out and the loop goes on to serve the next connection; any other
/// error ends it.
#[test]
fn transient_accept_errors_are_retried() {
    use rustix::io::Errno;
    let rig = Rig::new("reg-accept-retry", me());
    let (client, served) = UnixStream::pair().unwrap();
    let mut script: std::collections::VecDeque<std::io::Result<UnixStream>> =
        std::collections::VecDeque::from([
            Err(Errno::MFILE.into()),
            Err(std::io::ErrorKind::Interrupted.into()),
            Ok(served),
            Err(std::io::ErrorKind::PermissionDenied.into()),
        ]);
    let stop = AtomicBool::new(false);
    let ended = rig
        .server
        .serve_with(&mut || script.pop_front().unwrap(), &stop)
        .unwrap_err();
    assert_eq!(ended.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(say(&client, &registration("2.0.0", None))["ok"], true);
    assert_eq!(rig.registry.report("svc", Some("2.0.0")), Reported::Healthy);
}

fn reg(version: &str, health: Option<(&str, &str)>) -> Registration {
    serde_json::from_str(registration(version, health).trim()).unwrap()
}

/// Catches a read that failed being taken for the end of the line: the peer goes away leaving
/// bytes it never read, which the kernel reports to the server as a reset after the part of the
/// line that came; that is an error of the connection, not a line to answer.
#[test]
fn a_read_error_is_an_error_not_the_end_of_the_line() {
    let rig = Rig::new("reg-read-error", me());
    let (client, served) = UnixStream::pair().unwrap();
    (&served).write_all(b"never read").unwrap();
    (&client).write_all(b"{\"name\":").unwrap();
    drop(client);
    let err = rig.server.serve_connection(served).unwrap_err();
    assert!(
        matches!(&err, RegistryError::Io(e) if e.kind() == std::io::ErrorKind::ConnectionReset),
        "{err:?}"
    );
    assert_eq!(rig.registry.report("svc", None), Reported::Absent);
}

/// Catches a read that came back for want of time being an error: a socket that has nothing to
/// read and does not wait answers as a read past the deadline does, and the line is `malformed`.
#[test]
fn a_read_that_runs_out_of_time_is_malformed() {
    let rig = Rig::new("reg-read-timeout", me());
    let (client, served) = UnixStream::pair().unwrap();
    served.set_nonblocking(true).unwrap();
    rig.server.serve_connection(served).unwrap();
    assert_eq!(answer(&client)["error"], "malformed");
    assert_eq!(rig.registry.report("svc", None), Reported::Absent);
}

/// Catches the write of the registrations made under the registry's lock (a slow disk then held
/// up every read): while a registration is being written, a read answers, and does not yet
/// believe the registration; once written it is believed. The disk is a writer that waits for
/// the test, so nothing here waits for a time.
#[test]
fn a_slow_write_does_not_hold_up_a_read() {
    let scratch = Scratch::new("reg-slow-write");
    let (entered, writing) = mpsc::channel::<()>();
    let (go_on, finish) = mpsc::channel::<()>();
    let finish = Mutex::new(finish);
    let writer = move |dir: &Path, bytes: &[u8]| -> std::io::Result<()> {
        entered.send(()).unwrap();
        finish.lock().unwrap().recv().unwrap();
        fs::write(dir.join(REGISTRATIONS_FILE), bytes)
    };
    let registry = Registry::load(scratch.path(), FakeClock::new(), &declared(me()))
        .unwrap()
        .with_writer(Box::new(writer));
    let registry = Arc::new(registry);
    let registering = {
        let registry = registry.clone();
        let health = Some(("127.0.0.1:9301", "/ready"));
        std::thread::spawn(move || registry.register(me(), &reg("2.0.0", health), &grant(me())))
    };
    writing.recv().unwrap();
    assert_eq!(registry.report("svc", Some("2.0.0")), Reported::Absent);
    assert_eq!(registry.health_addr("svc"), None);
    go_on.send(()).unwrap();
    registering.join().unwrap().unwrap();
    assert_eq!(registry.report("svc", Some("2.0.0")), Reported::Healthy);
    assert!(registry.health_addr("svc").is_some());
    // The close is written the same way.
    go_on.send(()).unwrap();
    registry.closed("svc", 1).unwrap();
    writing.recv().unwrap();
    assert_eq!(
        registry.report("svc", Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
}

/// Catches a registration loaded from disk being believed under the grant it was made with: a
/// service whose declaration went, that is now declared for another user, or whose health port
/// is no longer granted is dropped at load; one whose time-to-live changed is held to the new
/// one.
#[test]
fn a_loaded_registration_is_held_to_the_declarations_of_now() {
    let rig = Rig::new("reg-revalidate", me());
    let health = Some(("127.0.0.1:9301", "/ready"));
    rig.registry
        .register(me(), &reg("2.0.0", health), &grant(me()))
        .unwrap();
    let state = rig.scratch.path().join("state");
    let load = |table: Table| Registry::load(&state, rig.clock.clone(), &table).unwrap();
    let only = |grant: Grant| Table(Mutex::new(BTreeMap::from([("svc".to_owned(), grant)])));

    let mut narrower = grant(me());
    narrower.health_ports = vec![9302];
    for (why, table) in [
        ("declaration gone", nothing()),
        ("another user", declared(me().wrapping_add(1))),
        ("health port no longer granted", only(narrower)),
    ] {
        let after = load(table);
        assert_eq!(
            after.report("svc", Some("2.0.0")),
            Reported::Absent,
            "{why}"
        );
        assert_eq!(after.health_addr("svc"), None, "{why}");
    }

    let mut shorter = grant(me());
    shorter.registration_ttl_secs = 10;
    let after = load(only(shorter));
    assert_eq!(after.report("svc", Some("2.0.0")), Reported::Healthy);
    assert!(after.health_addr("svc").is_some());
    rig.clock.advance(10);
    assert_eq!(
        after.report("svc", Some("2.0.0")),
        Reported::Unhealthy(Why::Stopped)
    );
    // The same grant keeps it, as it was.
    let after = load(declared(me()));
    assert_eq!(after.report("svc", Some("2.0.0")), Reported::Healthy);
}
