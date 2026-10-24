use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::IntoRawFd as _;
use std::path::PathBuf;

use rustix::io::{FdFlags, fcntl_getfd};

use super::*;

/// A pool table: two frontends that run on their own.
const POOL: &str = r#"
[proxy]
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
[[proxy.frontend]]
name = "a"
base_port = 18000
[[proxy.frontend]]
name = "b"
base_port = 18010
"#;

/// The table of the removed two-slot supervision: the `upstream` table in place of the
/// frontends.
const SLOT: &str = r#"
database = "postgres:///kbf?host=/nonexistent"
[proxy]
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
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

/// The arguments of the verb over the file `config` (none: the platform's), with no retired
/// flag.
fn args(config: Option<PathBuf>, check: bool) -> ProxyArgs {
    ProxyArgs {
        config,
        check,
        update_dir: None,
        orders_dir: None,
        kbf_binary: None,
    }
}

/// Asserts that the loader reads the `[proxy]` table, and not another table or the keys around
/// it, and applies the table's own rules.
#[test]
fn the_proxy_table_is_read_from_installer_toml() {
    let config = load(&file(
        "ok",
        format!("min_generation = 4\n{POOL}").as_bytes(),
    ))
    .expect("valid");
    assert_eq!(config.frontend.len(), 2);
    assert_eq!(
        config.operator_token_file,
        PathBuf::from("/etc/kbf/operator.token")
    );
}

/// Asserts that each way a file is refused is a usage error that names the file and the
/// reason: unreadable, a directory, not text, no `[proxy]` table, an unknown key in the table,
/// a table whose own rules fail (both the frontends and the `upstream` table) and a file over
/// the size limit. (A key outside the table that this build does not know is not a refusal: the
/// next test.)
#[test]
fn a_file_that_is_not_a_proxy_config_is_refused_with_its_path_and_reason() {
    let path = file("none", b"min_generation = 4\n");
    let refused = |path: &Path, want: &str| {
        let text = usage_of(load(path));
        assert!(text.starts_with(&path.display().to_string()), "{text}");
        assert!(text.contains(want), "{text}");
    };
    refused(&path, "has no [proxy] table");
    refused(&path.with_file_name("missing.toml"), "No such file");
    refused(path.parent().expect("dir"), "directory");
    refused(&file("binary", &[0xff, 0xfe, 0x00]), "UTF-8");
    let typo = POOL.replacen("[proxy]\n", "[proxy]\nsurprise = 1\n", 1);
    refused(&file("typo", typo.as_bytes()), "unknown field");
    let both = format!("{POOL}[proxy.upstream]\nbase_port = 18000\n");
    refused(
        &file("both", both.as_bytes()),
        "two-slot supervision was removed",
    );
    refused(
        &file("big", &vec![b'#'; MAX_FILE_BYTES + 1]),
        "bytes; at most",
    );
}

/// Asserts that the proxy does not refuse to start because the file it shares with the installer
/// holds a key or a value this build does not know. The file is also read by `setup` and the
/// fetcher, which are closed (they must understand the whole file); the proxy checks the
/// `[proxy]` table and the two numbers the shared parser bounds, and a package downgrade must not
/// take the public port down over a source key a later build wrote. The keys it does read are
/// still checked, and the table is still closed; a value typed into a key it reads is not
/// repeated.
#[test]
fn the_proxy_reads_the_keys_it_needs_and_ignores_the_rest_of_the_file() {
    let future = "release_source = \"mirror-pull\"\nrelease_ca = 7\nfuture_key = true\n";
    let text = format!("{future}confirm_window_s = 300\nfetch_timeout_s = 1200\n{POOL}");
    let config = load(&file("future-keys", text.as_bytes())).expect("valid");
    assert_eq!(config.frontend.len(), 2);
    let low = format!("confirm_window_s = 10\n{POOL}");
    let refused = usage_of(load(&file("low-window", low.as_bytes())));
    assert!(refused.contains("confirm_window_s"), "{refused}");
    let typed = format!("confirm_window_s = \"surprise\"\n{POOL}");
    let refused = usage_of(load(&file("typed-window", typed.as_bytes())));
    assert!(!refused.contains("surprise"), "{refused}");
    let in_table = POOL.replace("name = \"a\"", "name = \"a\"\nsurprise = 1");
    let refused = usage_of(load(&file("typo-in-table", in_table.as_bytes())));
    assert!(refused.contains("unknown field"), "{refused}");
}

/// Asserts that a table with the `upstream` table (the removed two-slot supervision) is refused
/// as a usage error by the loader, by `--check` and by a run, which does so before it looks at
/// its socket, in words that name the file, say that the supervision was removed and name the
/// tables to give instead.
#[test]
fn a_slot_table_is_refused_and_the_words_say_the_supervision_was_removed() {
    let path = file("slot", SLOT.as_bytes());
    let said = usage_of(load(&path));
    assert!(said.starts_with(&path.display().to_string()), "{said}");
    assert!(said.contains("two-slot supervision was removed"), "{said}");
    assert!(said.contains("[[proxy.frontend]]"), "{said}");
    for check in [true, false] {
        let said = usage_of(run(&args(Some(path.clone()), check)));
        assert!(said.contains("two-slot supervision was removed"), "{said}");
        assert!(!said.contains("LISTEN_PID"), "{said}");
    }
}

/// Asserts that each retired flag given is named by exactly one warning, that none given is no
/// warning, and that the verb still runs with all of them given.
#[test]
fn each_retired_flag_given_is_named_by_one_warning_and_the_verb_still_runs() {
    assert!(args(None, false).retired_warnings().is_empty());
    let given = ProxyArgs {
        update_dir: Some("/srv/u".into()),
        orders_dir: Some("/srv/o".into()),
        kbf_binary: Some("/usr/bin/kbf".into()),
        ..args(Some(file("retired", POOL.as_bytes())), true)
    };
    let all = given.retired_warnings();
    assert_eq!(all.len(), 3, "{all:?}");
    for flag in ["--kbf-binary", "--update-dir", "--orders-dir"] {
        let line = format!("{flag} is no longer read: two-slot supervision was removed");
        assert_eq!(all.iter().filter(|l| **l == line).count(), 1, "{all:?}");
    }
    assert_eq!(run(&given), Ok(()));
}

/// Asserts that an activation rule is not loosened: the markers must name this process and
/// exactly one descriptor, whatever else is open.
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

/// Catches `run` reading a table and then serving without a socket: `--check` stops after the
/// table, and a run with no activation markers is a usage error that binds nothing.
#[test]
fn check_stops_at_the_table_and_a_run_needs_its_socket() {
    let path = file("run", POOL.as_bytes());
    assert_eq!(run(&args(Some(path.clone()), true)), Ok(()));
    let text = usage_of(run(&args(Some(path.clone()), false)));
    assert!(text.contains("LISTEN_PID"), "{text}");
    let missing = args(Some(path.with_file_name("none.toml")), true);
    assert!(usage_of(run(&missing)).contains("No such file"));
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

/// Catches `Stop::production` carrying another bound or another exit than the verb's: its
/// fields are pinned here. It does not see which `Stop` the pool builds (a literal `Stop` there
/// would pass); `proxy_pool::run` takes its stop from `Stop::production` and reads as such.
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

/// Catches a `proxy` that reads `/etc/kbf/installer.toml` on a Mac, where `setup` writes
/// `/usr/local/etc/kbf/installer.toml`: with no `--config` the file is the platform's, and a
/// flag wins.
#[test]
fn proxy_reads_the_installer_toml_setup_wrote_on_the_platform() {
    assert_eq!(
        config_path(&args(None, true), Platform::Linux),
        PathBuf::from("/etc/kbf/installer.toml")
    );
    assert_eq!(
        config_path(&args(None, true), Platform::MacOs),
        PathBuf::from("/usr/local/etc/kbf/installer.toml")
    );
    let given = args(Some(PathBuf::from("/x.toml")), true);
    assert_eq!(
        config_path(&given, Platform::MacOs),
        PathBuf::from("/x.toml")
    );
}
