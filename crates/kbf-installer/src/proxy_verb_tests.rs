use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::{IntoRawFd as _, OwnedFd};
use std::path::PathBuf;
use std::sync::mpsc;

use hyper::header::{ALLOW, HeaderValue};
use rustix::io::{FdFlags, fcntl_getfd};

use super::*;
use crate::proxy::api::traits::RestartPending;
use crate::proxy::orders::SpoolError;
use crate::testkit::A;

const PROXY: &str = r#"
database = "postgres:///kbf?host=/nonexistent"
[proxy]
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
hold_max_s = 15
[proxy.upstream]
base_port = 18000
"#;

/// An `installer.toml` with this text, in a scratch directory of its own.
fn file(name: &str, text: &[u8]) -> PathBuf {
    let path = kbf_host::testkit::scratch(&format!("proxy-verb-{name}")).join("installer.toml");
    fs::write(&path, text).expect("write");
    path
}

fn usage_of(result: Result<impl std::fmt::Debug, VerbError>) -> String {
    match result {
        Err(VerbError::Usage(text)) => text,
        other => panic!("not a usage error: {other:?}"),
    }
}

/// Catches a loader that reads another table than `[proxy]`, drops the keys around it, or
/// does not apply the table's own rules.
#[test]
fn the_proxy_table_is_read_from_installer_toml() {
    let loaded = load(&file(
        "ok",
        format!("min_generation = 4\n{PROXY}").as_bytes(),
    ))
    .expect("valid");
    let config = loaded.proxy;
    assert_eq!(config.upstream.base_port, 18_000);
    assert_eq!(config.hold_max_s, 15);
    assert_eq!(
        config.operator_token_file,
        PathBuf::from("/etc/kbf/operator.token")
    );
    assert_eq!(
        (loaded.confirm_window_s, loaded.fetch_timeout_s),
        (900, 900)
    );
}

/// Catches the update API's two numbers read and dropped (they used to be: `load` returned the
/// `[proxy]` table alone): a file that sets them hands them on, and the places the API reads
/// and writes are the flags and the proxy's state directory.
#[test]
fn the_update_apis_numbers_and_places_come_from_the_file_and_the_flags() {
    let table = PROXY.replacen(
        "[proxy]\n",
        "[proxy]\nstate_dir = \"/srv/proxy-state\"\n",
        1,
    );
    let path = file(
        "places",
        format!("confirm_window_s = 300\nfetch_timeout_s = 1200\n{table}").as_bytes(),
    );
    let loaded = load(&path).expect("valid");
    assert_eq!(
        (loaded.confirm_window_s, loaded.fetch_timeout_s),
        (300, 1200)
    );
    let args = ProxyArgs {
        config: Some(path),
        update_dir: Some("/srv/root-half".into()),
        orders_dir: Some("/srv/orders".into()),
        check: false,
    };
    let (layout, _) = crate::testkit::host_root_on("places", Platform::Linux);
    let cfg = handover_config(&args, &loaded, &layout);
    // The children's ports are the upstream table's: in a slot the operator listener is at
    // offset 2 and the admin calls at 3, slot B ten ports above slot A, all on loopback; and
    // the operator token is the one the proxy checks callers against.
    let at = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    assert_eq!(
        cfg.slots,
        SlotAddrs {
            admin: [at(18003), at(18013)],
            operator: [at(18002), at(18012)],
        }
    );
    assert_eq!(
        cfg.operator_token_file,
        PathBuf::from("/etc/kbf/operator.token")
    );
    assert!(!cfg.gate.is_filled());
    assert_eq!(
        (
            cfg.state_dir,
            cfg.orders_dir,
            cfg.status_file,
            cfg.confirm_window_s,
            cfg.fetch_timeout_s
        ),
        (
            PathBuf::from("/srv/proxy-state"),
            PathBuf::from("/srv/orders"),
            PathBuf::from("/srv/root-half/status.json"),
            300,
            1200
        )
    );
}

/// Catches `proxy` watching the Linux state directory on a Mac: without `--update-dir` and
/// `--orders-dir` the update API's places are the layout's (the ones `setup` and the Mac
/// provisioner write) on each platform, and the flags still win.
#[test]
fn the_update_apis_places_default_to_the_layouts_on_every_platform() {
    let path = file("default-places", PROXY.as_bytes());
    let loaded = load(&path).expect("valid");
    let none = ProxyArgs {
        config: Some(path.clone()),
        check: false,
        update_dir: None,
        orders_dir: None,
    };
    for (platform, state) in [
        (Platform::Linux, "/var/lib/kbf-installer"),
        (Platform::MacOs, "/var/db/kbf-installer"),
    ] {
        let (layout, _) = crate::testkit::host_root_on("default-places-dirs", platform);
        let cfg = handover_config(&none, &loaded, &layout);
        assert_eq!(
            cfg.status_file,
            PathBuf::from(format!("{state}/status.json")),
            "{platform:?}"
        );
        assert_eq!(
            cfg.orders_dir,
            PathBuf::from(format!("{state}/orders")),
            "{platform:?}"
        );
        assert_eq!(
            none.update_dir_in(&layout).to_str(),
            Some(layout.update_dir().as_str())
        );
        assert_eq!(
            none.orders_dir_in(&layout).to_str(),
            Some(layout.orders_dir().as_str())
        );
        let given = ProxyArgs {
            config: Some(path.clone()),
            check: false,
            update_dir: Some("/srv/u".into()),
            orders_dir: Some("/srv/o".into()),
        };
        let cfg = handover_config(&given, &loaded, &layout);
        assert_eq!(cfg.status_file, PathBuf::from("/srv/u/status.json"));
        assert_eq!(cfg.orders_dir, PathBuf::from("/srv/o"));
    }
}

/// Catches each way a file is refused being accepted, or refused without its path and its
/// reason: unreadable, a directory, not text, no `[proxy]` table, an unknown key in the table,
/// an unknown key outside it, a table whose own rules fail and a file over the size limit.
#[test]
fn a_file_that_is_not_a_proxy_config_is_refused_with_its_path_and_reason() {
    let path = file("none", b"min_generation = 4\n");
    let refused = |path: &Path, want: &str| {
        let text = usage_of(load(path));
        assert!(
            text.starts_with(&path.display().to_string()) && text.contains(want),
            "{text}"
        );
    };
    refused(&path, "has no [proxy] table");
    refused(&path.with_file_name("missing.toml"), "No such file");
    refused(path.parent().expect("dir"), "directory");
    refused(&file("binary", &[0xff, 0xfe, 0x00]), "UTF-8");
    refused(
        &file("typo", format!("{PROXY}\nsurprise = 1\n").as_bytes()),
        "unknown field",
    );
    refused(
        &file("outside", format!("surprise = 1\n{PROXY}").as_bytes()),
        "line 1: unknown field (the text is not shown)",
    );
    // The parser quotes the key it met; the message must not carry it.
    let text = usage_of(load(&file(
        "outside-redacted",
        format!("surprise = 1\n{PROXY}").as_bytes(),
    )));
    assert!(!text.contains("surprise"), "{text}");
    refused(
        &file("port", PROXY.replace("18000", "80").as_bytes()),
        "proxy: upstream.base_port must be 1024 to 65521 so both slots fit",
    );
    refused(
        &file("big", &vec![b'#'; MAX_FILE_BYTES + 1]),
        "bytes; at most",
    );
}

/// Catches an activation rule loosened: the markers must name this process and exactly one
/// descriptor, whatever else is open. (`refuses_without_exactly_one_listen_fd`)
#[test]
fn the_verb_refuses_without_exactly_one_listen_fd_for_this_process() {
    let pid = std::process::id();
    let mine = pid.to_string();
    let other = (pid + 1).to_string();
    // Not an open descriptor: a loosened rule would go on to say so, not to take something.
    let fd = 1_000_000;
    for (listen_pid, listen_fds, want) in [
        (None, Some("1"), "LISTEN_PID"),
        (Some(other.as_str()), Some("1"), "LISTEN_PID"),
        (Some("not a pid"), Some("1"), "LISTEN_PID"),
        (Some(mine.as_str()), None, "LISTEN_FDS"),
        (Some(mine.as_str()), Some("0"), "LISTEN_FDS"),
        (Some(mine.as_str()), Some("2"), "LISTEN_FDS"),
        (Some(mine.as_str()), Some("one"), "LISTEN_FDS"),
    ] {
        let text = usage_of(activated(listen_pid, listen_fds, pid, fd));
        assert!(text.contains(want), "{listen_pid:?} {listen_fds:?}: {text}");
    }
}

/// What the OS reports for `fd`, for a failure message: on Linux `SO_ACCEPTCONN`, elsewhere
/// the result of `getsockopt(TCP_CONNECTION_INFO)` (return code, errno, `tcpi_state`).
fn seen_by_the_os(fd: &impl std::os::fd::AsRawFd) -> String {
    #[cfg(target_os = "linux")]
    {
        let _ = fd;
        "SO_ACCEPTCONN is read through rustix".to_owned()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: all-zero is a valid `tcp_connection_info` (integers only).
        let mut info: libc::tcp_connection_info = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
        // SAFETY: `fd` is open; `info` and `len` are live and `len` is the size of `info`.
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_CONNECTION_INFO,
                (&raw mut info).cast(),
                &raw mut len,
            )
        };
        let errno = std::io::Error::last_os_error();
        format!(
            "TCP_CONNECTION_INFO rc={rc} errno={errno} len={len} tcpi_state={}",
            info.tcpi_state
        )
    }
}

/// Catches a descriptor taken without being looked at: a closed number, a pipe, a socket that
/// does not listen and a listening socket that is not TCP are each refused, and a TCP
/// listener comes back as the same socket, close-on-exec.
#[test]
fn only_a_listening_tcp_socket_is_taken_and_it_is_made_close_on_exec() {
    let pid = std::process::id();
    let mine = pid.to_string();
    let take = |fd| activated(Some(mine.as_str()), Some("1"), pid, fd);

    assert!(usage_of(take(1_000_000)).contains("not open"));

    let (reader, _writer) = std::io::pipe().expect("pipe");
    let text = usage_of(take(reader.into_raw_fd()));
    assert!(text.contains("not a listening socket"), "{text}");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let stream =
        std::net::TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
    let text = usage_of(take(stream.into_raw_fd()));
    assert!(text.contains("not a listening socket"), "{text}");

    // A TCP socket that is bound but never listens: the state check must say no.
    let bound = rustix::net::socket(
        rustix::net::AddressFamily::INET,
        rustix::net::SocketType::STREAM,
        None,
    )
    .expect("socket");
    rustix::net::bind(&bound, &std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let text = usage_of(take(bound.into_raw_fd()));
    assert!(text.contains("not a listening socket"), "{text}");

    // A listening socket that is not TCP. (An abstract name: a path in the scratch directory
    // can be longer than a socket path may be.)
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::net::UnixListener;
        let name = format!("kbf-proxy-verb-test-{pid}");
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name).expect("a name");
        let unix = UnixListener::bind_addr(&address).expect("bind");
        let text = usage_of(take(unix.into_raw_fd()));
        assert!(text.contains("not a TCP socket"), "{text}");
    }

    // A socket as a service manager hands it over: listening and inheritable (std sets
    // close-on-exec on its own sockets, which would hide a verb that never sets it).
    let inherited = rustix::net::socket(
        rustix::net::AddressFamily::INET,
        rustix::net::SocketType::STREAM,
        None,
    )
    .expect("socket");
    assert!(
        !fcntl_getfd(&inherited)
            .expect("flags")
            .contains(FdFlags::CLOEXEC)
    );
    rustix::net::bind(
        &inherited,
        &std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
    )
    .expect("bind");
    rustix::net::listen(&inherited, 8).expect("listen");
    let addr = TcpListener::from(inherited.try_clone().expect("dup"))
        .local_addr()
        .expect("addr");
    let seen = seen_by_the_os(&inherited);
    let taken = take(inherited.into_raw_fd()).unwrap_or_else(|e| {
        panic!("a listening TCP socket was refused: {e:?}; the OS says {seen}")
    });
    assert!(
        fcntl_getfd(&taken)
            .expect("flags")
            .contains(FdFlags::CLOEXEC)
    );
    let again = TcpListener::from(taken);
    assert_eq!(again.local_addr().expect("addr"), addr);
}

/// Catches an error that maps to the wrong exit code or the wrong `--json` word, and the two
/// conversions the `?` of the serving path uses.
#[test]
fn errors_map_to_exit_codes_and_words() {
    let usage = VerbError::Usage("u".to_owned());
    let failed = VerbError::Failed("f".to_owned());
    assert_eq!((usage.exit(), usage.code()), (Exit::Usage, "usage"));
    assert_eq!((failed.exit(), failed.code()), (Exit::Failed, "failed"));
    assert_eq!(
        VerbError::from(std::io::Error::other("io broke")),
        VerbError::Failed("io broke".to_owned())
    );
    assert_eq!(
        VerbError::from(Errno::BADF),
        VerbError::Failed(Errno::BADF.to_string())
    );
}

/// Catches the update API's start-up errors mapped to the wrong exit: a spool or counter the
/// service user cannot use is the operator's (exit 2, which the unit does not restart); a
/// program that cannot be identified is not (exit 1).
#[test]
fn the_update_apis_start_up_errors_map_to_exit_codes() {
    let spool = HandoverError::Orders {
        dir: PathBuf::from("/o"),
        source: SpoolError::Counter(std::io::Error::other("c")),
    };
    assert_eq!(
        VerbError::from(spool),
        VerbError::Usage(
            "update API: the order spool at /o: the order counter cannot be read or made durable: c"
                .to_owned()
        )
    );
    let binary = HandoverError::Binary(std::io::Error::other("proc"));
    assert_eq!(
        VerbError::from(binary),
        VerbError::Failed("update API: cannot tell which program is running: proc".to_owned())
    );
}

/// The update API's seams over a scratch directory: the state directory exists, the spool and
/// the root half's `status.json` do not.
fn handover(name: &str) -> HandoverConfig {
    let root = kbf_host::testkit::scratch(&format!("proxy-verb-api-{name}"));
    let state_dir = root.join("state");
    fs::create_dir(&state_dir).expect("state dir");
    HandoverConfig {
        state_dir,
        orders_dir: root.join("orders"),
        status_file: root.join("status.json"),
        confirm_window_s: 900,
        fetch_timeout_s: 900,
        slots: SlotAddrs {
            admin: [loopback(18003), loopback(18013)],
            operator: [loopback(18002), loopback(18012)],
        },
        operator_token_file: root.join("operator.token"),
        installer_toml: root.join("installer.toml"),
        gate: GateSlot::new(),
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Catches a proxy that cannot be bound being reported as served: a descriptor that is not a
/// socket makes `Proxy::bind` fail, and the verb says so as a failure (exit 1).
#[test]
fn a_descriptor_the_runtime_cannot_use_is_a_failure_to_start() {
    let loaded = load(&file("serve", PROXY.as_bytes())).expect("valid");
    let regular = OwnedFd::from(fs::File::open(file("fd", b"x")).expect("open"));
    let database = state_database(&file("serve-db", PROXY.as_bytes())).expect("a database");
    match serve(
        loaded.proxy,
        database,
        &handover("serve"),
        regular,
        &kbf_host::Root::host(),
    ) {
        Err(VerbError::Failed(text)) => {
            assert!(text.starts_with("cannot start the proxy: "), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

/// Catches the update API's seams built after the proxy is bound (a spool the service user
/// cannot open would then be a failed start, exit 1, that systemd restarts every second):
/// with a file where `orders/` should be and a descriptor that would fail the bind, the
/// refusal is the spool's, a usage error naming the directory.
#[test]
fn a_spool_that_cannot_be_opened_is_a_usage_error_before_anything_is_bound() {
    let loaded = load(&file("spool", PROXY.as_bytes())).expect("valid");
    let handover = handover("spool");
    fs::write(&handover.orders_dir, b"a file").expect("a file in the way");
    let regular = OwnedFd::from(fs::File::open(file("fd-spool", b"x")).expect("open"));
    let database = state_database(&file("spool-db", PROXY.as_bytes())).expect("a database");
    let text = usage_of(serve(
        loaded.proxy,
        database,
        &handover,
        regular,
        &kbf_host::Root::host(),
    ));
    assert!(
        text.starts_with("update API: the order spool at "),
        "{text}"
    );
    assert!(
        text.contains(&handover.orders_dir.display().to_string()),
        "{text}"
    );
}

/// Catches `run` reading a table and then serving without a socket: `--check` stops after the
/// table, and a run with no activation markers is a usage error that binds nothing.
#[test]
fn check_stops_at_the_table_and_a_run_needs_its_socket() {
    let path = file("run", PROXY.as_bytes());
    let args = |check| ProxyArgs {
        config: Some(path.clone()),
        update_dir: Some(path.with_file_name("update")),
        orders_dir: Some(path.with_file_name("orders")),
        check,
    };
    assert_eq!(run(&args(true), Path::new("/")), Ok(()));
    let text = usage_of(run(&args(false), Path::new("/")));
    assert!(text.contains("LISTEN_PID"), "{text}");
    let missing = ProxyArgs {
        config: Some(path.with_file_name("none.toml")),
        ..args(true)
    };
    assert!(usage_of(run(&missing, Path::new("/"))).contains("No such file"));
}

/// Catches a `--root` that is not absolute being taken: with the socket in place the verb
/// refuses it as a usage error before it binds anything.
#[test]
fn a_root_that_is_not_absolute_is_a_usage_error() {
    let path = file("rel-root", PROXY.as_bytes());
    let args = ProxyArgs {
        config: Some(path.clone()),
        update_dir: Some(path.with_file_name("update")),
        orders_dir: Some(path.with_file_name("orders")),
        check: false,
    };
    let result = run(&args, Path::new("relative"));
    assert!(usage_of(result).starts_with("--root: "));
}

static BOUND_EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static SECOND_EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn bound_exit(code: i32) -> ! {
    BOUND_EXIT.store(code, Ordering::SeqCst);
    panic!("exit {code}");
}

fn second_exit(code: i32) -> ! {
    SECOND_EXIT.store(code, Ordering::SeqCst);
    panic!("exit {code}");
}

/// Catches a stop that waits on the proxy alone: with a drain that never returns, the first
/// signal still ends the process (status 1) once the bound is over, from a thread the
/// runtime does not run, and the drain was asked for exactly once.
#[test]
fn the_first_stop_signal_starts_a_clock_that_ends_the_process() {
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: Duration::from_millis(30),
        exit: bound_exit,
    });
    let mut drains = 0;
    let clock = stop.signalled(|| drains += 1);
    assert_eq!(
        (drains, BOUND_EXIT.load(Ordering::SeqCst)),
        (1, -1),
        "before the bound"
    );
    assert!(clock.join().is_err(), "the thread ends in the exit");
    assert_eq!(BOUND_EXIT.load(Ordering::SeqCst), 1);
}

/// Catches a second signal that is ignored (an operator's second SIGTERM would wait out the
/// drain again): it ends the process at once with status 1 and asks for no second drain.
#[test]
fn a_second_stop_signal_ends_the_process_at_once() {
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: Duration::from_secs(60),
        exit: second_exit,
    });
    drop(stop.signalled(|| {}));
    assert_eq!(
        SECOND_EXIT.load(Ordering::SeqCst),
        -1,
        "the first only drains"
    );
    let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(stop.signalled(|| panic!("a second drain")));
    }));
    assert!(second.is_err());
    assert_eq!(SECOND_EXIT.load(Ordering::SeqCst), 1);
}

/// Pins the production bound: longer than the proxy's 5 s drain (so a clean drain is never
/// cut) and short enough that a stop signal ends the process within seconds.
#[test]
fn the_production_stop_bound_is_ten_seconds() {
    assert_eq!(STOP_BOUND, Duration::from_secs(10));
}

const UPDATE: &str = "/v1/update/frontend";
const LONG: Duration = Duration::from_secs(10);

fn asked(method: &str, path: &str, body: &str) -> UpdateRequest {
    UpdateRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        body: body.as_bytes().to_vec(),
    }
}

/// The body of a `POST` that asks for an update to `A`.
fn post_of() -> String {
    format!(r#"{{"commit":"{A}","request_id":"r1","nodes":false}}"#)
}

fn json(reply: &UpdateReply) -> serde_json::Value {
    serde_json::from_slice(&reply.body).unwrap_or(serde_json::Value::Null)
}

/// The update API started the way `serve` starts it, over seams in a scratch directory (`edit`
/// changes them first). What stops the proxy is a channel: one message for each drain. The
/// bound of the stop is an hour, so the test process is never ended by it.
struct Api {
    bridge: UpdateBridge,
    stop: Arc<Stop>,
    died: Arc<AtomicBool>,
    drains: mpsc::Receiver<()>,
}

fn api(name: &str, edit: impl FnOnce(&mut Parts)) -> Api {
    let mut parts = handover::parts(&handover(name)).expect("the seams");
    edit(&mut parts);
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: Duration::from_secs(3600),
        exit: std::process::exit,
    });
    let died = Arc::new(AtomicBool::new(false));
    let (tx, drains) = mpsc::channel();
    let drain: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = tx.send(());
    });
    let bridge = start_updates(parts, &stop, &died, &drain).expect("the actor");
    Api {
        bridge,
        stop,
        died,
        drains,
    }
}

/// Catches a bridge that loses a part of the request or of the answer: the method and the path
/// (a `GET` of the route is the API's status, `abort` is its own path, the proxy's other
/// update paths are not this API's), the body (a `POST` with a valid body reaches the API's
/// own refusal, where a body lost on the way is the router's 400 before the API), and the
/// reply's status, body and headers (a 405 keeps its `Allow`). A healthy API never stops the
/// proxy.
#[tokio::test]
async fn the_bridge_carries_the_request_to_the_api_and_the_answer_back() {
    let api = api("bridge", |_| {});
    let reply = api.bridge.handle(asked("GET", UPDATE, "")).await;
    assert_eq!(reply.status, 200);
    assert_eq!(json(&reply)["update_id"], serde_json::Value::Null);

    let reply = api.bridge.handle(asked("POST", UPDATE, &post_of())).await;
    assert_eq!(reply.status, 503);
    assert_eq!(json(&reply)["error"], "handover_unavailable");

    let reply = api.bridge.handle(asked("PUT", UPDATE, "")).await;
    assert_eq!(reply.status, 405);
    assert_eq!(
        reply.headers,
        [(ALLOW, HeaderValue::from_static("GET, POST"))]
    );

    let abort = format!("{UPDATE}/abort");
    let reply = api.bridge.handle(asked("POST", &abort, "")).await;
    assert_eq!(reply.status, 409);
    assert_eq!(json(&reply)["error"], "no_update_open");

    let reply = api
        .bridge
        .handle(asked("POST", "/v1/update/desired", ""))
        .await;
    assert_eq!(reply.status, 404);
    assert!(api.drains.try_recv().is_err(), "the proxy was told to stop");
    assert!(!api.died.load(Ordering::SeqCst));
}

/// A restart answer that panics. The status read asks it, so a `GET` kills the actor's thread.
struct Dies;

impl RestartPending for Dies {
    fn restart_pending(&self) -> bool {
        panic!("the actor's thread dies")
    }
}

/// Catches an actor that dies silently, or that stops the proxy more than once: after its
/// thread died the request that was in hand is a 503, the proxy was told to stop (once), the
/// death is remembered, the bound of the stop is running, every later request is a 503 and
/// asks no second stop, and the verb that returns after that is a failure (exit 1).
#[tokio::test]
async fn an_update_api_that_dies_stops_the_proxy_once_and_fails_the_verb() {
    let api = api("death", |parts| parts.restart = Box::new(Dies));
    let reply = api.bridge.handle(asked("GET", UPDATE, "")).await;
    assert_eq!(reply.status, 503);
    assert_eq!(
        api.drains.recv_timeout(LONG),
        Ok(()),
        "the proxy was not told to stop"
    );
    assert!(api.died.load(Ordering::SeqCst));
    assert!(api.stop.seen.load(Ordering::SeqCst), "no bound is running");
    for _ in 0..3 {
        let reply = api.bridge.handle(asked("GET", UPDATE, "")).await;
        assert_eq!(reply.status, 503);
    }
    assert!(
        api.drains.recv_timeout(Duration::from_millis(100)).is_err(),
        "the proxy was told to stop twice"
    );
    assert_eq!(
        outcome(Ok(()), &api.died),
        Err(VerbError::Failed(
            "the update API stopped: the proxy was stopped to be started again".to_owned()
        ))
    );
}

/// Catches a verb that exits 0 after the update API died (`Restart=on-failure` would not start
/// it again), and one that lets the death hide the runtime's own error.
#[test]
fn a_verb_whose_update_api_died_is_a_failure_though_the_runtime_returned_cleanly() {
    let (alive, dead) = (AtomicBool::new(false), AtomicBool::new(true));
    assert_eq!(outcome(Ok(()), &alive), Ok(()));
    assert_eq!(
        outcome(Ok(()), &dead),
        Err(VerbError::Failed(
            "the update API stopped: the proxy was stopped to be started again".to_owned()
        ))
    );
    for died in [&alive, &dead] {
        assert_eq!(
            outcome(Err(std::io::Error::other("run broke")), died),
            Err(VerbError::Failed("run broke".to_owned()))
        );
    }
}

static DEATH_EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn death_exit(code: i32) -> ! {
    DEATH_EXIT.store(code, Ordering::SeqCst);
    panic!("exit {code}");
}

/// Catches an actor death that stops the proxy without the bound a stop signal has (a drain
/// that never finishes would then never end the process): the first stop of this kind drains
/// once and starts the clock that ends the process with status 1, and a second is no stop.
#[test]
fn an_actors_death_starts_the_clock_a_first_signal_starts() {
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: Duration::from_millis(30),
        exit: death_exit,
    });
    let mut drains = 0;
    let clock = stop.failed(Box::new(|| drains += 1)).expect("a first stop");
    assert_eq!(drains, 1);
    assert!(clock.join().is_err(), "the thread ends in the exit");
    assert_eq!(DEATH_EXIT.load(Ordering::SeqCst), 1);
    assert!(stop.failed(Box::new(|| drains += 1)).is_none());
    assert_eq!(drains, 1, "a second stop drained again");
}

/// Catches a death that is taken for a second stop signal: with a drain under way, the actor
/// dying is remembered and neither drains again nor ends the process at once (which would cut
/// the drain a signal asked for). A cut would be `exit` called on this thread, and it panics.
#[test]
fn a_death_during_a_drain_is_noted_and_does_not_end_the_drain() {
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: Duration::from_secs(3600),
        exit: death_exit,
    });
    drop(stop.signalled(|| {}));
    let died = AtomicBool::new(false);
    actor_died(&stop, &died, || panic!("a second drain"));
    assert!(died.load(Ordering::SeqCst));
}

/// Catches a swap thread that dies without stopping the proxy, or that stops it as a second
/// signal (which ends the process at once): through the hook the driver is given, the death
/// is remembered so that `outcome` fails the verb, the proxy drains once, and a second death
/// with the drain under way drains nothing more.
#[cfg(target_os = "linux")]
#[test]
fn a_dead_handover_driver_stops_the_proxy_once_and_fails_the_verb() {
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: Duration::from_secs(3600),
        exit: death_exit,
    });
    let died = Arc::new(AtomicBool::new(false));
    let drains = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drain: Arc<dyn Fn() + Send + Sync> = {
        let drains = Arc::clone(&drains);
        Arc::new(move || {
            drains.fetch_add(1, Ordering::SeqCst);
        })
    };
    let hook = death_hook(&stop, &died, &drain);
    hook("the swap stopped: x");
    assert_eq!(drains.load(Ordering::SeqCst), 1);
    assert!(died.load(Ordering::SeqCst));
    assert!(outcome(Ok(()), &died).is_err());
    hook("again");
    assert_eq!(
        drains.load(Ordering::SeqCst),
        1,
        "a second death drained again"
    );
}

/// Catches a lookup of the recorded process that asks the wrong slot or invents one: the slot
/// the child control recorded gives its process, the other gives none.
#[cfg(target_os = "linux")]
#[test]
fn the_driver_asks_the_child_control_for_the_recorded_process_of_a_slot() {
    use crate::proxy::child_real::{ProcChild, Recorded};
    use crate::proxy::model::{ProcIdent, Slot as Own};
    let dir = kbf_host::testkit::scratch("proxy-verb-recorded");
    let ident = ProcIdent {
        pid: 7,
        start_time: 9,
        exe: "/x/kbf".into(),
    };
    let record = Recorded {
        ident: ident.clone(),
        commit: crate::testkit::c(A),
    };
    fs::write(
        dir.join("a.json"),
        serde_json::to_vec(&record).expect("json"),
    )
    .expect("record");
    let layout = crate::layout::Layout::linux(kbf_host::Root::host());
    let child = Arc::new(ProcChild::new(layout, dir, Box::new(|_, _| Vec::new())));
    let recorded = recorded_by(&child);
    assert_eq!(recorded(Own::A), Some(ident));
    assert_eq!(recorded(Own::B), None);
}

/// Catches a start-up line that does not say what the API does: it names the files, the
/// staging deadline (the fetch limit of 900 s and five minutes) and that a `POST` is refused,
/// and why.
#[test]
fn the_start_up_line_says_what_the_update_api_will_do() {
    let cfg = handover("line");
    let parts = handover::parts(&cfg).expect("the seams");
    assert_eq!(
        update_line(&parts, &cfg),
        format!(
            "update API on (status {}, orders {}, staging deadline 1200 s); \
             a POST is refused: proxy not bound yet",
            cfg.status_file.display(),
            cfg.orders_dir.display()
        )
    );
}

/// Catches `Stop::production` carrying another bound or another exit than the verb's: its
/// fields are pinned here. It does not see which `Stop` `serve` builds (a literal `Stop` in
/// `serve` would pass); `serve` takes its stop from `Stop::production` and reads as such.
#[test]
fn the_production_stop_ends_the_process_after_the_bound() {
    let stop = Stop::production();
    assert_eq!(stop.bound, STOP_BOUND);
    assert_eq!(
        stop.exit as usize,
        std::process::exit as fn(i32) -> ! as usize
    );
    assert!(!stop.seen.load(Ordering::SeqCst));
}

/// Catches a start that reads as served when it did not: the log line says which slot serves an
/// adopted child, and that new streams stay held, with the reason, for a held start and for a
/// supervisor that could not even build its admin client.
#[cfg(target_os = "linux")]
#[test]
fn the_supervisors_line_says_what_the_start_did() {
    use crate::proxy::model::Slot;
    use crate::proxy::supervisor::Startup;
    assert_eq!(
        startup_line(Ok(Startup::Adopted { active: Slot::B })),
        "the frontend child in slot B serves"
    );
    assert_eq!(
        startup_line(Ok(Startup::Held("both slots have a live child".into()))),
        "no frontend child adopted, new streams stay held: both slots have a live child"
    );
    assert_eq!(
        startup_line(Err("the proxy secret is not a header value".into())),
        "no frontend child adopted, new streams stay held: the proxy secret is not a header value"
    );
}

/// Catches a `proxy` that reads `/etc/kbf/installer.toml` on a Mac, where `setup` writes
/// `/usr/local/etc/kbf/installer.toml`: with no `--config` the file is the platform's, and a
/// flag wins.
#[test]
fn proxy_reads_the_installer_toml_setup_wrote_on_the_platform() {
    let args = |config| ProxyArgs {
        config,
        check: true,
        update_dir: Some("/nonexistent".into()),
        orders_dir: Some("/nonexistent/orders".into()),
    };
    assert_eq!(
        config_path(&args(None), Platform::Linux),
        PathBuf::from("/etc/kbf/installer.toml")
    );
    assert_eq!(
        config_path(&args(None), Platform::MacOs),
        PathBuf::from("/usr/local/etc/kbf/installer.toml")
    );
    assert_eq!(
        config_path(&args(Some(PathBuf::from("/x.toml"))), Platform::MacOs),
        PathBuf::from("/x.toml")
    );
}

/// Catches a database the verb accepts that it could not use, and a refusal that quotes it.
/// The key is required (a frontend host runs on Postgres); a password is refused because the
/// file is world-readable; an unusable URL is refused; none of the messages carries the URL or
/// its parts; and a good one is accepted without being connected to.
#[test]
fn the_state_database_is_required_and_its_refusals_never_quote_the_url() {
    let secret = "postgres://kbf_marker_user:hunter2_marker@127.0.0.1:5432/kbf_marker_db";
    let at = |name: &str, text: &str| state_database(&file(name, text.as_bytes()));
    let cases = [
        (
            "db-none",
            "min_generation = 1\n".to_owned(),
            "state database",
        ),
        (
            "db-password",
            format!("database = {secret:?}\n"),
            "carries a password",
        ),
        (
            "db-bad",
            "database = \"mysql://kbf_marker_user@127.0.0.1/kbf_marker_db\"\n".to_owned(),
            "not usable",
        ),
    ];
    for (name, text, want) in cases {
        let why = usage_of(at(name, &text));
        assert!(why.contains(want), "{name}: {why}");
        for part in [
            "hunter2_marker",
            "kbf_marker_user",
            "kbf_marker_db",
            "127.0.0.1",
        ] {
            assert!(!why.contains(part), "{name}: {part} in {why}");
        }
    }
    assert!(
        at(
            "db-ok",
            "database = \"postgres:///kbf?host=/nonexistent\"\n"
        )
        .is_ok()
    );
}

/// Catches `--check` that passes a file the run would refuse for want of a database.
#[test]
fn check_refuses_a_file_without_a_database() {
    let path = file(
        "check-db",
        PROXY
            .replace("database = \"postgres:///kbf?host=/nonexistent\"\n", "")
            .as_bytes(),
    );
    let args = ProxyArgs {
        config: Some(path),
        check: true,
        update_dir: Some("/nonexistent".into()),
        orders_dir: Some("/nonexistent/orders".into()),
    };
    assert!(usage_of(run(&args, Path::new("/"))).contains("state database"));
}

/// The verb's start, on a scratch host: the installed release (a stand-in that would fail at
/// once if it were ever run) is verified, the database is unreachable, and the start is
/// bounded to one attempt.
///
/// Catches a verb that starts the installed release without migrating it first (the
/// `NoMigration` it used to pass): the attempt must fail as `migration-failed`, in the line the
/// log gets and in `boot.json`, with nothing recorded as started and no part of the URL
/// anywhere. A start that skipped the migration would run the stand-in and fail as
/// `exited-before-ready`.
///
/// Mutant seen red: `host_migrator` replaced by a hook that migrates nothing.
#[cfg(target_os = "linux")]
#[test]
fn the_start_migrates_the_installed_release_before_it_runs_it() {
    use kbf_proxy::Proxy;
    use kbf_proxy::config::ProxyConfig;
    use kbf_state::postgres::PostgresConfig;
    use kbf_state::postgres::schema;

    use crate::proxy::boot::Retry;
    use crate::proxy::child_real::ProcChild;
    use crate::proxy::gate_real::ProxyGate;
    use crate::proxy::gate_real_tests::proxy_table;
    use crate::proxy::runtime::SystemClock;
    use crate::proxy::supervisor::{children_dir, frontend_argv};
    use crate::testkit::{A, c, host_root};

    let (layout, root) = host_root("proxy-verb-start");
    let release = layout.join(&layout.release_dir(&c(A)));
    fs::create_dir_all(&release).expect("release dir");
    let program = release.join("kbf");
    // A link to this test binary: an object the coverage tools can read, and a program that
    // would fail at once if the start ever ran it.
    std::os::unix::fs::symlink(std::env::current_exe().expect("test binary"), &program)
        .expect("link");
    fs::write(release.join(crate::stage::VERIFIED), format!("{A}\n")).expect("VERIFIED");
    let state_dir = root.join("proxy-state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let status = root.join("status.json");
    // The root half has staged and installed this release and says what schema it needs, so
    // the migration goes straight to the database.
    fs::write(
        &status,
        format!(
            "{{\"installed_commit\":\"{A}\",\"staged_commit\":\"{A}\",\"staged\":{{\"protocol\":1,\"min_protocol\":1,\"state_schema\":1}}}}\n"
        ),
    )
    .expect("status.json");

    let config = ProxyConfig::from_toml_str(&proxy_table("verb-start", "")).expect("config");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let (handle, _runner) = Proxy::bind(config.clone(), OwnedFd::from(listener)).expect("proxy");
    let gate = ProxyGate::new(handle.clone());
    let url = "postgres://kbf_marker_user:hunter2_marker@127.0.0.1:1/kbf_marker_db";
    let with = Supervise {
        database: Database::new(
            PostgresConfig::parse(url).expect("parses"),
            schema::embedded(),
        ),
        state_dir: state_dir.clone(),
        proxy_auth_file: config.proxy_auth_file.clone(),
        upstream: config.upstream,
        operator_token_file: config.operator_token_file.clone(),
        status,
    };
    let retry = Retry {
        attempts: 1,
        first_wait: Duration::from_millis(1),
        max_wait: Duration::from_millis(1),
    };
    let child = ProcChild::new(
        layout,
        children_dir(&with.state_dir),
        frontend_argv(&with.proxy_auth_file, with.upstream),
    );
    let started = start_with(&handle, &gate, &SystemClock::new(), &child, with, retry);
    let line = startup_line(started);
    assert!(
        line.starts_with("no frontend child adopted, new streams stay held: ")
            && line.contains("migration-failed"),
        "{line}"
    );
    let boot = fs::read_to_string(state_dir.join("boot.json")).expect("boot.json");
    assert!(boot.contains("\"reason\":\"migration-failed\""), "{boot}");
    assert!(!state_dir.join("children").exists(), "a child was started");
    for part in [
        "hunter2_marker",
        "kbf_marker_user",
        "kbf_marker_db",
        "127.0.0.1",
    ] {
        assert!(!line.contains(part) && !boot.contains(part), "{part}");
    }
}
