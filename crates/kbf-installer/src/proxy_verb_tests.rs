use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::{IntoRawFd as _, OwnedFd};
use std::path::PathBuf;

use rustix::io::{FdFlags, fcntl_getfd};

use super::*;

const PROXY: &str = r#"
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
    let config = load(&file(
        "ok",
        format!("min_generation = 4\n{PROXY}").as_bytes(),
    ))
    .expect("valid");
    assert_eq!(config.upstream.base_port, 18_000);
    assert_eq!(config.hold_max_s, 15);
    assert_eq!(
        config.operator_token_file,
        PathBuf::from("/etc/kbf/operator.token")
    );
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
        "unknown field `surprise`",
    );
    refused(
        &file("port", PROXY.replace("18000", "80").as_bytes()),
        "proxy: upstream.base_port must be 1024 to 65522",
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
    let taken = take(inherited.into_raw_fd()).expect("a listening TCP socket");
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

/// Catches a proxy that cannot be bound being reported as served: a descriptor that is not a
/// socket makes `Proxy::bind` fail, and the verb says so as a failure (exit 1).
#[test]
fn a_descriptor_the_runtime_cannot_use_is_a_failure_to_start() {
    let config = load(&file("serve", PROXY.as_bytes())).expect("valid");
    let regular = OwnedFd::from(fs::File::open(file("fd", b"x")).expect("open"));
    match serve(config, regular) {
        Err(VerbError::Failed(text)) => {
            assert!(text.starts_with("cannot start the proxy: "), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

/// Catches `run` reading a table and then serving without a socket: `--check` stops after the
/// table, and a run with no activation markers is a usage error that binds nothing.
#[test]
fn check_stops_at_the_table_and_a_run_needs_its_socket() {
    let path = file("run", PROXY.as_bytes());
    let args = |check| ProxyArgs {
        config: path.clone(),
        check,
    };
    assert_eq!(run(&args(true)), Ok(()));
    let text = usage_of(run(&args(false)));
    assert!(text.contains("LISTEN_PID"), "{text}");
    let missing = ProxyArgs {
        config: path.with_file_name("none.toml"),
        check: true,
    };
    assert!(usage_of(run(&missing)).contains("No such file"));
}
