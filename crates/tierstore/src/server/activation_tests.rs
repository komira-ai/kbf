use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixListener;

use rustix::io::{FdFlags, fcntl_getfd};

use super::{OCI, Passed, S3, Sockets, plan, take};

fn passed(pid: Option<&str>, fds: Option<&str>, names: Option<&str>) -> Passed {
    Passed {
        pid: pid.map(str::to_owned),
        fds: fds.map(str::to_owned),
        names: names.map(str::to_owned),
    }
}

fn listening() -> (TcpListener, SocketAddr) {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
}

/// Asserts the descriptors a hand-over names: the sockets asked for come back in the order they
/// were asked for, each at the position of its name, from the first descriptor given.
#[test]
fn the_descriptors_are_those_of_the_names_in_the_order_asked_for() {
    let both = passed(Some("77"), Some("2"), Some("oci:s3"));
    assert_eq!(plan(&both, 77, 3, &[S3, OCI]), Ok(vec![4, 3]));
    assert_eq!(plan(&both, 77, 10, &[OCI, S3]), Ok(vec![10, 11]));
    let one = passed(Some("77"), Some("1"), Some("s3"));
    assert_eq!(plan(&one, 77, 3, &[S3]), Ok(vec![3]));
}

/// Asserts each way a hand-over is refused, with a message that says what is wrong: the variables
/// missing one at a time, a process id that is not this process's or not a number, a count that is
/// not a number or not the number of names, other sockets than the start needs, a name twice, and
/// a name that is not there.
#[test]
fn a_handover_that_is_not_exactly_the_sockets_asked_for_is_refused() {
    let cases = [
        (passed(None, Some("1"), Some("s3")), "LISTEN_PID is not set"),
        (
            passed(Some("1"), Some("1"), Some("s3")),
            "LISTEN_PID is \"1\" but this process is 77",
        ),
        (
            passed(Some("x"), Some("1"), Some("s3")),
            "LISTEN_PID is \"x\"",
        ),
        (
            passed(Some("77"), None, Some("s3")),
            "LISTEN_FDS is not set",
        ),
        (
            passed(Some("77"), Some("1"), None),
            "LISTEN_FDNAMES is not set",
        ),
        (
            passed(Some("77"), Some("-1"), Some("s3")),
            "LISTEN_FDS is \"-1\", not a number",
        ),
        (
            passed(Some("77"), Some("2"), Some("s3")),
            "LISTEN_FDS is 2 but LISTEN_FDNAMES is \"s3\", 1 names",
        ),
        (
            passed(Some("77"), Some("0"), Some("")),
            "LISTEN_FDS is 0 but",
        ),
        (
            passed(Some("77"), Some("2"), Some("s3:oci")),
            "passed 2 sockets (s3:oci) but this start needs 1 (s3)",
        ),
        (
            passed(Some("77"), Some("1"), Some("web")),
            "it must name one socket \"s3\"",
        ),
    ];
    for (given, want) in cases {
        let why = plan(&given, 77, 3, &[S3]).unwrap_err();
        assert!(why.contains(want), "{want}: {why}");
    }
    let twice = passed(Some("77"), Some("2"), Some("s3:s3"));
    let why = plan(&twice, 77, 3, &[S3, OCI]).unwrap_err();
    assert!(why.contains("it must name one socket \"s3\""), "{why}");
    let missing = passed(Some("77"), Some("2"), Some("s3:web"));
    let why = plan(&missing, 77, 3, &[S3, OCI]).unwrap_err();
    assert!(why.contains("it must name one socket \"oci\""), "{why}");
}

/// Asserts that a listening TCP socket is taken whole: the listener is the socket that was
/// passed (a connection to it is accepted), it is non-blocking, it is close-on-exec, and a socket
/// the manager made blocking and inheritable is made so.
#[test]
fn a_listening_tcp_socket_is_taken_as_it_is_and_made_nonblocking() {
    let (socket, addr) = listening();
    socket.set_nonblocking(false).unwrap();
    let fd = socket.into_raw_fd();
    let taken = take(fd, S3, addr).unwrap();
    assert_eq!(taken.as_raw_fd(), fd);
    assert_eq!(taken.local_addr().unwrap(), addr);
    let flags = fcntl_getfd(&taken).unwrap();
    assert!(flags.contains(FdFlags::CLOEXEC));
    // Non-blocking: nothing waits to be accepted, and the call says so at once.
    let nothing = taken.accept().unwrap_err();
    assert_eq!(nothing.kind(), std::io::ErrorKind::WouldBlock);
    // The socket is the one that was bound: a connection made to its address is its own, and the
    // connect has returned, so the connection is in the queue already.
    let _client = TcpStream::connect(addr).unwrap();
    let (_, peer) = taken.accept().unwrap();
    assert!(peer.ip().is_loopback());
}

/// Asserts that a descriptor that is not a listening TCP stream socket is refused, naming what
/// it is, and is left open: a datagram socket, a stream socket that is connected and one that
/// does not listen, a listening unix socket, a file, and a number nothing holds.
#[test]
fn a_descriptor_that_is_not_a_listening_tcp_socket_is_refused_and_left_open() {
    let (listener, addr) = listening();
    let datagram = UdpSocket::bind("127.0.0.1:0").unwrap();
    let connected = TcpStream::connect(addr).unwrap();
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "activation-unix");
    let unix = UnixListener::bind(dir.path().join("s")).unwrap();
    let file = std::fs::File::open(dir.path()).unwrap();
    let open = |fd: RawFd| take(fd, S3, addr).unwrap_err();

    assert!(open(datagram.as_raw_fd()).contains("is not a stream socket"));
    assert!(open(connected.as_raw_fd()).contains("is not a listening socket"));
    assert!(open(unix.as_raw_fd()).contains("is not a TCP socket"));
    assert!(open(file.as_raw_fd()).contains("is not a socket"));
    // A number nothing holds: the socket it was is dropped (closed) before it is asked about.
    let closed = UdpSocket::bind("127.0.0.1:0").unwrap().as_raw_fd();
    let why = open(closed);
    assert!(why.contains(&format!("descriptor {closed}")), "{why}");
    // None of those was closed by the refusal.
    assert!(datagram.local_addr().is_ok());
    assert!(connected.peer_addr().is_ok());
    assert!(unix.local_addr().is_ok());
    assert!(file.metadata().is_ok());
    assert!(listener.local_addr().is_ok());
}

/// Asserts that a socket bound to another address than the one the start serves is refused,
/// with both addresses in the message, and is not closed.
#[test]
fn a_socket_bound_to_another_address_than_the_flag_says_is_refused() {
    let (socket, addr) = listening();
    let (_, other) = listening();
    let why = take(socket.as_raw_fd(), OCI, other).unwrap_err();
    assert!(why.contains("\"oci\""), "{why}");
    assert!(why.contains(&addr.to_string()), "{why}");
    assert!(why.contains(&other.to_string()), "{why}");
    assert!(why.contains("ListenStream="), "{why}");
    assert!(socket.local_addr().is_ok());
}

/// Asserts the whole hand-over for one socket: the sockets of the start come out by their names,
/// the S3 one is the socket passed and the OCI one is none; and a hand-over that is refused is
/// reported as a refused flag (exit status 2), with `--listen-from-systemd` named.
#[test]
fn a_handover_makes_the_sockets_of_the_start_and_a_refusal_is_a_flag_refusal() {
    let (socket, addr) = listening();
    let fd = socket.into_raw_fd();
    let given = passed(Some("77"), Some("1"), Some("s3"));
    let mut sockets = Sockets::from_passed(&given, 77, fd, addr, None).unwrap();
    assert_eq!(sockets.s3.as_ref().unwrap().local_addr().unwrap(), addr);
    assert!(sockets.oci().is_none());

    let wrong = passed(Some("1"), Some("1"), Some("s3"));
    let why = Sockets::from_passed(&wrong, 77, fd, addr, None).unwrap_err();
    assert!(
        why.to_string()
            .starts_with("--listen-from-systemd: LISTEN_PID is"),
        "{why}"
    );
    let (other, other_addr) = listening();
    let why = Sockets::from_passed(&given, 77, other.as_raw_fd(), addr, None).unwrap_err();
    assert!(why.to_string().contains(&other_addr.to_string()), "{why}");
    let two = passed(Some("77"), Some("2"), Some("s3:oci"));
    let why = Sockets::from_passed(&two, 77, fd, addr, None).unwrap_err();
    assert!(why.to_string().contains("needs 1"), "{why}");
}

/// Asserts that a start that took no sockets binds its address: the S3 listener is a fresh bind,
/// and the OCI one is none.
#[tokio::test]
async fn without_sockets_the_address_is_bound() {
    let mut sockets = Sockets::default();
    let listener = sockets
        .s3_or_bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    assert!(listener.local_addr().unwrap().port() != 0);
    assert!(sockets.oci().is_none());
    let taken = listener.local_addr().unwrap();
    let again = Sockets::default().s3_or_bind(taken).await.unwrap_err();
    assert!(
        again.to_string().starts_with(&format!("binding {taken}: ")),
        "{again}"
    );
}

/// Asserts that a socket the manager passed is served from: the listener the start gets is the
/// one passed, and a connection made to its address is accepted through the runtime.
#[tokio::test]
async fn a_passed_socket_is_the_listener_of_the_start() {
    let (socket, addr) = listening();
    let fd = socket.into_raw_fd();
    let given = passed(Some("77"), Some("1"), Some("s3"));
    let mut sockets = Sockets::from_passed(&given, 77, fd, addr, None).unwrap();
    let listener = sockets
        .s3_or_bind("192.0.2.1:9".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(listener.local_addr().unwrap(), addr);
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (_, peer) = listener.accept().await.unwrap();
    assert_eq!(peer, client.local_addr().unwrap());
}

/// Asserts that the hand-over is taken once: the second call of `from_process` in a process is
/// refused, whatever the first one found (here no `LISTEN_PID`, which the first call refuses).
#[test]
fn the_handover_is_taken_once_per_process() {
    let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let first = Sockets::from_process(addr, None).unwrap_err();
    assert!(first.to_string().contains("LISTEN_PID"), "{first}");
    let second = Sockets::from_process(addr, None).unwrap_err();
    assert!(
        second.to_string().contains("already taken by this process"),
        "{second}"
    );
}

/// The `.rs` files under `dir`, with their text.
fn sources(dir: &std::path::Path, found: &mut Vec<(std::path::PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
        }
    }
}

/// Asserts that `unsafe` is allowed in exactly one place in the crate, the function of
/// `activation.rs` that adopts a descriptor, and used nowhere else; and that no file of the crate
/// names `Command::new` in `src` (a tripwire on that spelling, not a proof that no child is
/// started), which is why the variables of the hand-over are not removed from the environment.
#[test]
fn unsafe_is_allowed_once_and_no_child_process_is_started() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let allow = ["allow(unsafe", "_code)"].concat();
    let block = ["unsafe", " {"].concat();
    let function = ["unsafe", " fn"].concat();
    let child = ["Command", "::new"].concat();
    let mut allowed = Vec::new();
    for (path, text) in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // This file names the patterns; it is checked for the others.
        let own = name == "activation_tests.rs";
        if text.contains(&allow) && !own {
            allowed.push(name.clone());
        }
        if !own && name != "activation.rs" {
            assert!(
                !text.contains(&block) && !text.contains(&function),
                "{name}"
            );
        }
        assert!(
            own || !text.contains(&child),
            "{name} starts a child process"
        );
    }
    assert_eq!(allowed, ["activation.rs"]);
    let text = files
        .iter()
        .find(|(p, _)| p.ends_with("activation.rs"))
        .unwrap();
    assert_eq!(text.1.matches(&allow).count(), 1);
    // The two operations of `take`: the borrow that asks the system what the descriptor is, and
    // the owner made of it once it is a listening socket.
    assert_eq!(text.1.matches(&block).count(), 2);
    assert!(!text.1.contains(&function));
}
