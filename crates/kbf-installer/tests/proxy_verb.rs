//! `kbf-installer proxy` as a service manager runs it: a process with one activated socket.
//! Nothing here needs root or a child frontend; the proxy has no active child, so every
//! backend path is `UNAVAILABLE` and `/readyz` is 503, which is what the supervisor-less verb
//! promises. The update route (`/v1/update/frontend`) is the update API with no handover driver
//! behind it: it answers its status and refuses a `POST`, and the tests at the end say how.

#![cfg(target_os = "linux")]

use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use kbf_testkit::reaped::{self, Reaped};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

const TABLE: &str = r#"
[proxy]
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
[proxy.upstream]
base_port = 18000
"#;

/// A `[proxy]` table over real files in a scratch directory: a CA, a server certificate for
/// `localhost`, the operator token (0600, 32 or more characters) and a secret path. Returns
/// the `installer.toml` and the CA file a client trusts.
fn served_config(name: &str) -> (PathBuf, PathBuf) {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, SanType,
    };
    use std::os::unix::fs::PermissionsExt as _;
    let dir = kbf_host::testkit::scratch(&format!("proxy-bin-{name}"));
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    let mut server = CertificateParams::new(Vec::<String>::new()).expect("params");
    server.subject_alt_names = vec![SanType::DnsName("localhost".try_into().expect("ia5"))];
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().expect("key");
    let cert = server.signed_by(&key, &ca).expect("server certificate");
    let private = |file: &str, text: &str| {
        let path = dir.join(file);
        fs::write(&path, text).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        path
    };
    let (cert, key) = (
        private("server.pem", &cert.pem()),
        private("server.key", &key.serialize_pem()),
    );
    let ca_file = private("ca.pem", &ca.pem());
    let token = private("operator.token", TOKEN);
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("state dir");
    let text = format!(
        "[proxy]\noperator_token_file = {token:?}\nserver_cert_file = {cert:?}\n\
         server_key_file = {key:?}\nclient_ca_file = {ca_file:?}\n\
         proxy_auth_file = {:?}\nstate_dir = {state:?}\n[proxy.upstream]\nbase_port = 18000\n",
        dir.join("proxy-auth"),
    );
    // Not `config`: it makes the directory again, and the files above are in it.
    let path = dir.join("installer.toml");
    fs::write(&path, text).expect("write");
    (path, ca_file)
}

/// The operator token `served_config` writes: the bearer the update route is called with.
const TOKEN: &str = "operator-token-0123456789-abcdefghijkl";

/// The proxy's state directory (`orders.json`, `requests.json`, `children/`), next to the
/// config `served_config` wrote.
fn state_of(config: &Path) -> PathBuf {
    config.with_file_name("state")
}

/// The directory the verb is told holds the root half's `status.json`: next to the config, so
/// every test has its own.
fn update_dir_of(config: &Path) -> PathBuf {
    config.with_file_name("update")
}

/// The order spool the verb is told to use: next to the config.
fn orders_dir_of(config: &Path) -> PathBuf {
    config.with_file_name("orders")
}

fn config(name: &str, text: &str) -> PathBuf {
    let path = kbf_host::testkit::scratch(&format!("proxy-bin-{name}")).join("installer.toml");
    fs::write(&path, text).expect("write");
    path
}

fn run(args: &[&str], path: &Path) -> Output {
    Command::new(BIN)
        .arg("proxy")
        .args(args)
        .arg("--config")
        .arg(path)
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS")
        .output()
        .expect("runs")
}

/// Catches `--check` that binds, or that passes a bad table: exit 0 for a valid one with
/// nothing printed, exit 2 and the path and reason on standard error for one with no
/// `[proxy]` table and for one with an unknown key; and a missing file is exit 2 too.
#[test]
fn check_is_exit_0_for_a_valid_table_and_2_for_every_refusal() {
    let good = config("check-ok", TABLE);
    let out = run(&["--check"], &good);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");

    for (name, text, want) in [
        ("check-none", "min_generation = 1\n", "no [proxy] table"),
        // A valid `[proxy]` next to a bad key of the rest of the file: `--check` is the
        // whole file's check, as the verb's own load is, not the proxy table's alone.
        (
            "check-other-key",
            &format!("min_generation = \"x\"\n{TABLE}"),
            "line 1: invalid type: string (the text is not shown)",
        ),
        (
            "check-typo",
            &format!("{TABLE}surprise = 1\n"),
            "unknown field",
        ),
    ] {
        let path = config(name, text);
        let out = run(&["--check"], &path);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{name}: {err}");
        assert!(
            err.contains(&path.display().to_string()) && err.contains(want),
            "{name}: {err}"
        );
    }
    let out = run(&["--check"], &good.with_file_name("missing.toml"));
    assert_eq!(out.status.code(), Some(2));
}

/// Catches a verb that serves without being handed its socket (it would bind a port of its
/// own): no markers, markers for another process, and a count that is not 1 each exit 2 with
/// the reason, before anything is bound.
#[test]
fn without_exactly_one_activated_socket_the_verb_exits_2() {
    let path = config("no-fd", TABLE);
    let out = run(&[], &path);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("socket activation") && err.contains("LISTEN_PID"),
        "{err}"
    );

    let out = Command::new(BIN)
        .args(["proxy", "--config"])
        .arg(&path)
        .env("LISTEN_PID", "1")
        .env("LISTEN_FDS", "1")
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "another process's markers");
}

/// Starts the verb with `listener` as its descriptor 3 and `LISTEN_PID` set to the child's
/// own pid in the child, as a service manager does. The guard stops and reaps the child when
/// it drops, so a failed assertion or a panic leaves no process behind.
fn start(path: &Path, listener: &TcpListener) -> Reaped {
    start_with(path, listener, Stdio::piped())
}

/// [`start`] with `stderr` for the verb's standard error.
fn start_with(path: &Path, listener: &TcpListener, stderr: Stdio) -> Reaped {
    start_masked(path, listener, stderr, false)
}

/// [`start_with`], and with `block_term` the verb inherits SIGTERM and SIGINT blocked, as from
/// a launcher that blocked them.
fn start_masked(path: &Path, listener: &TcpListener, stderr: Stdio, block_term: bool) -> Reaped {
    start_args(path, listener, stderr, block_term, &[])
}

/// [`start_masked`] with more flags for the verb.
fn start_args(
    path: &Path,
    listener: &TcpListener,
    stderr: Stdio,
    block_term: bool,
    flags: &[&std::ffi::OsStr],
) -> Reaped {
    let fd = listener.as_raw_fd();
    let mut command = Command::new(BIN);
    command
        .args(["proxy", "--config"])
        .arg(path)
        .arg("--orders-dir")
        .arg(orders_dir_of(path))
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    // A test that passes its own `--update-dir` (a `status.json` it wrote) is not given a second.
    if !flags
        .iter()
        .any(|flag| flag.to_str() == Some("--update-dir"))
    {
        command.arg("--update-dir").arg(update_dir_of(path));
    }
    // SAFETY: only async-signal-safe calls (`sigprocmask` family included) and `setenv`, which is safe here: the child is a
    // fresh fork of a test thread that holds no environment lock, and nothing else in this
    // file forks with a hook.
    unsafe {
        command.pre_exec(move || {
            if block_term {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut set);
                libc::sigaddset(&raw mut set, libc::SIGTERM);
                libc::sigaddset(&raw mut set, libc::SIGINT);
                libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
            }
            if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0);
            } else if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut digits = [0_u8; 12];
            let mut at = digits.len() - 1;
            let mut pid = libc::getpid();
            while pid > 0 {
                at -= 1;
                digits[at] = b'0' + (pid % 10) as u8;
                pid /= 10;
            }
            // Through `environ`, not `Command::env`: std passes its own copy of the
            // environment to `exec` as soon as one variable is set on the command.
            libc::setenv(c"LISTEN_PID".as_ptr(), digits[at..].as_ptr().cast(), 1);
            libc::setenv(c"LISTEN_FDS".as_ptr(), c"1".as_ptr(), 1);
            Ok(())
        });
    }
    // Descriptor 3 is the activated socket; nothing above it is inherited (a build tool's
    // lock on descriptor 9 would otherwise live as long as this child).
    reaped::spawn(&mut command, 4).expect("spawn kbf-installer")
}

/// A TLS stream to the proxy at `addr` that trusts the CA in `ca`, offering `h2`.
async fn tls(
    addr: SocketAddr,
    ca: &Path,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    for der in rustls::pki_types::CertificateDer::pem_file_iter(ca).expect("CA file") {
        roots.add(der.expect("CA PEM")).expect("root");
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").expect("name"),
            tcp,
        )
        .await
        .expect("TLS handshake")
}

/// One `GET` over TLS and h2 to `addr`: the status and the body.
async fn get(addr: SocketAddr, ca: &Path, path: &str) -> (u16, String) {
    call(addr, ca, "GET", path, None, b"").await
}

/// One request over TLS and h2 to `addr`: the status and the body. `bearer` is the token sent
/// as `Authorization: Bearer`, if any; `body` is the request body (none when empty).
async fn call(
    addr: SocketAddr,
    ca: &Path,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: &[u8],
) -> (u16, String) {
    let tcp = tls(addr, ca).await;
    let (send, connection) = h2::client::handshake(tcp).await.expect("h2 handshake");
    tokio::spawn(connection);
    let mut request = http::Request::builder()
        .method(method)
        .uri(format!("http://proxy.test{path}"));
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = request.body(()).expect("a request");
    let (response, mut sending) = send
        .ready()
        .await
        .expect("capacity")
        .send_request(request, body.is_empty())
        .expect("send");
    if !body.is_empty() {
        sending
            .send_data(bytes::Bytes::copy_from_slice(body), true)
            .expect("send the body");
    }
    let response = response.await.expect("a response");
    let status = response.status().as_u16();
    let mut body = response.into_body();
    let mut text = Vec::new();
    while let Some(chunk) = body.data().await {
        text.extend_from_slice(&chunk.expect("data"));
    }
    (status, String::from_utf8_lossy(&text).into_owned())
}

/// The ports `pid` is listening on: its socket inodes against the kernel's listening TCP
/// sockets, v4 and v6.
fn listening_ports(pid: u32) -> Vec<u16> {
    let inodes: Vec<String> = fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("fds")
        .filter_map(|e| fs::read_link(e.ok()?.path()).ok())
        .filter_map(|l| {
            let l = l.to_string_lossy().into_owned();
            Some(l.strip_prefix("socket:[")?.strip_suffix(']')?.to_owned())
        })
        .collect();
    let mut ports = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in fs::read_to_string(table).expect("table").lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f[3] == "0A" && inodes.iter().any(|i| i == f[9]) {
                let port = f[1].rsplit(':').next().expect("a port");
                ports.push(u16::from_str_radix(port, 16).expect("hex"));
            }
        }
    }
    ports
}

/// Catches a verb that does not serve its activated socket, that listens on anything else
/// (an admin or metrics port of its own), or that
/// does not stop on SIGTERM with exit 0: it answers `/healthz` 200, `/readyz` 503 with no
/// active child and an unknown path 404, listens on exactly the one port it was given, and
/// exits 0 on SIGTERM.
#[tokio::test(flavor = "current_thread")]
async fn the_activated_socket_is_served_alone_and_sigterm_stops_it() {
    serve_alone_and_stop_on(libc::SIGTERM).await;
}

/// Catches a verb whose SIGINT handler is missing: the default action ends the process by
/// the signal (no exit code) instead of the clean stop a terminal's Ctrl-C is owed. Streams
/// open at the stop are not part of this test.
#[tokio::test(flavor = "current_thread")]
async fn sigint_stops_it_with_exit_0_too() {
    serve_alone_and_stop_on(libc::SIGINT).await;
}

async fn serve_alone_and_stop_on(signal: i32) {
    let (path, ca) = served_config(&format!("serve-{signal}"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start(&path, &listener);
    drop(listener);

    let (status, body) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!((status, body.as_str()), (200, "ok"));
    assert_eq!(get(addr, &ca, "/readyz").await.0, 503, "no active child");
    assert_eq!(get(addr, &ca, "/nothing").await.0, 404);
    assert_eq!(
        listening_ports(child.id()),
        [addr.port()],
        "the activated socket and nothing else"
    );

    let stopped = Instant::now();
    // SAFETY: `kill` of our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        assert!(
            stopped.elapsed() < Duration::from_secs(20),
            "no exit on the signal"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(status.code(), Some(0), "a clean stop");
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("stderr"), &mut err)
        .expect("stderr text");
    assert!(err.contains("proxy serving"), "{err}");
}

/// Catches a guard that leaves the verb running when the test is over, and a child that
/// inherits what the test holds: after the drop the pid is gone (`kill(pid, 0)` is ESRCH,
/// so it was reaped, not left a zombie), and while it ran the child held descriptors 0 to 3
/// and nothing else, though the test left one open for it to inherit.
#[tokio::test(flavor = "current_thread")]
async fn no_child_outlives_its_guard_and_it_holds_only_its_socket() {
    let leaked = fs::File::open("/dev/null").expect("open");
    // SAFETY: clearing close-on-exec on a descriptor this test holds, as a tool that keeps
    // a lock open for its children does.
    unsafe { libc::fcntl(leaked.as_raw_fd(), libc::F_SETFD, 0) };

    let (path, ca) = served_config("orphan");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let child = start(&path, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    let pid = child.id().cast_signed();
    let mut held: Vec<i32> = fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("fds")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    held.sort_unstable();
    // 0 to 2 are the standard streams, 3 the socket; the runtime's own epoll, event and
    // signal descriptors come after and are the verb's, so the leaked number is what is
    // checked: it must not be there.
    assert!(held.contains(&3), "{held:?}");
    let leaked_at = leaked.as_raw_fd();
    assert!(
        fs::read_link(format!("/proc/{pid}/fd/{leaked_at}"))
            .map_or(true, |t| t.to_string_lossy() != "/dev/null"),
        "the child holds the test's descriptor {leaked_at}: {held:?}"
    );

    drop(child);
    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid, 0) };
    assert_eq!(alive, -1, "the child outlived its guard");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

/// Waits (at most `within`) for `child` to exit: its status and how long it took.
fn exited(child: &mut Reaped, within: Duration) -> (Option<std::process::ExitStatus>, Duration) {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return (Some(status), started.elapsed());
        }
        if started.elapsed() > within {
            return (None, started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Catches a verb whose second stop signal is ignored: with a client holding a connection
/// open the first SIGTERM drains for 5 s, and a second one ends it at once with status 1
/// (an operator who is done waiting), well inside the drain. Before the second signal it
/// must still be draining, so the test is not racing a process that already left.
#[tokio::test(flavor = "current_thread")]
async fn a_second_sigterm_ends_a_draining_verb_at_once() {
    let (path, ca) = served_config("second-term");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start(&path, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);
    let _idle = std::net::TcpStream::connect(addr).expect("an idle client");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let pid = child.id().cast_signed();
    // SAFETY: `kill` of our own child.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let (early, _) = exited(&mut child, Duration::from_millis(500));
    assert!(
        early.is_none(),
        "the first SIGTERM drains while a client is connected"
    );
    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let (status, took) = exited(&mut child, Duration::from_secs(3));
    assert_eq!(status.and_then(|s| s.code()), Some(1), "after {took:?}");
}

/// Catches a verb that dies because its standard error is closed (`eprintln!` panics on a
/// broken pipe, exit 101, before it serves): with the read end of its error pipe gone, it
/// serves and still stops on SIGTERM with status 0.
#[tokio::test(flavor = "current_thread")]
async fn a_closed_standard_error_does_not_stop_the_verb_serving() {
    let (path, ca) = served_config("closed-stderr");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    // A pipe with no reader: every write to it fails with a broken pipe.
    let mut ends = [0; 2];
    // SAFETY: `pipe` fills two descriptors, each owned from here on.
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    // SAFETY: both are fresh, open and owned by nothing else.
    let (reader, writer) =
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
    drop(reader);
    let mut child = start_with(&path, &listener, Stdio::from(writer));
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);
    // SAFETY: `kill` of our own child.
    assert_eq!(
        unsafe { libc::kill(child.id().cast_signed(), libc::SIGTERM) },
        0
    );
    let (status, took) = exited(&mut child, Duration::from_secs(10));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "after {took:?}");
}

/// Catches a verb that does not adopt the frontend a previous proxy left, or that takes its
/// children down when it stops: a process recorded under the proxy's state directory, still
/// the recorded process, is adopted at start (the startup hold ends into its slot, and the
/// verb says so), and it is still running after the proxy has drained and exited on SIGTERM.
#[tokio::test(flavor = "current_thread")]
async fn a_recorded_frontend_is_adopted_and_outlives_the_proxy_that_stops() {
    use kbf_installer::proxy::child_real::{Recorded, ident_of};

    let (path, ca) = served_config("adopt");
    let children = state_of(&path).join("children");
    fs::create_dir_all(&children).expect("children dir");
    let mut frontend = Command::new("sleep");
    frontend.arg("600").stdin(Stdio::null());
    let frontend = reaped::spawn(&mut frontend, 3).expect("a stand-in frontend");
    let ident = ident_of(frontend.id()).expect("identity");
    let record = Recorded {
        ident,
        commit: kbf_installer::CommitId::new(&"a".repeat(40)).expect("a commit"),
    };
    fs::write(
        children.join("a.json"),
        serde_json::to_vec(&record).expect("json"),
    )
    .expect("record");

    // The adopted child is asked its role before the startup hold ends into it: a stand-in
    // admin port answers as an active holder. Slot A's admin port is the base port plus 3.
    let admin = TcpListener::bind("127.0.0.1:0").expect("admin port");
    let admin_port = admin.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        const ROLE: &str = "{\"role\":\"active\",\"admin_protocol\":1,\"generation\":1,\
                            \"held_generation\":1,\"superseded\":false}";
        for stream in admin.incoming().flatten() {
            let mut stream = stream;
            let mut head = [0_u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut head);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{ROLE}",
                ROLE.len()
            );
            let _ = std::io::Write::write_all(&mut stream, reply.as_bytes());
        }
    });
    let table = fs::read_to_string(&path).expect("config text").replace(
        "base_port = 18000",
        &format!("base_port = {}", admin_port - 3),
    );
    fs::write(&path, table).expect("write");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut proxy = start(&path, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    // SAFETY: `kill` of our own child.
    assert_eq!(
        unsafe { libc::kill(proxy.id().cast_signed(), libc::SIGTERM) },
        0
    );
    let (status, _) = exited(&mut proxy, Duration::from_secs(20));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "a clean stop");
    let mut err = String::new();
    std::io::Read::read_to_string(&mut proxy.stderr.take().expect("stderr"), &mut err)
        .expect("stderr text");
    assert!(
        err.contains("the frontend child in slot A serves"),
        "the recorded child was not adopted: {err}"
    );
    // SAFETY: signal 0 only checks that the process exists; the child is ours.
    let alive = unsafe { libc::kill(frontend.id().cast_signed(), 0) };
    assert_eq!(alive, 0, "the frontend went down with the proxy");
}

/// Catches a cold start that cannot say why it failed or where: with no child to adopt and a
/// `status.json` that cannot be read, the verb reports each try to `boot.json` in its state
/// directory (the file replaces itself, state `retrying`, the reason's word) and to its standard
/// error with the detail, and a stop during the wait between tries ends it cleanly.
#[tokio::test(flavor = "current_thread")]
async fn a_cold_start_that_cannot_read_status_reports_each_try_and_stops_cleanly() {
    let update = kbf_host::testkit::scratch("proxy-bin-cold-update");
    fs::write(update.join("status.json"), b"{not json").expect("status.json");
    let (path, _ca) = served_config("cold");
    let state = state_of(&path);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let flags = [std::ffi::OsStr::new("--update-dir"), update.as_os_str()];
    let mut proxy = start_args(&path, &listener, Stdio::piped(), false, &flags);
    drop(listener);
    let boot = state.join("boot.json");
    let began = Instant::now();
    let text = loop {
        if let Ok(text) = fs::read_to_string(&boot)
            && text.contains("retrying")
        {
            break text;
        }
        assert!(began.elapsed() < Duration::from_secs(60), "no boot.json");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(json["reason"], "status-unreadable");
    // SAFETY: `kill` of our own child.
    assert_eq!(
        unsafe { libc::kill(proxy.id().cast_signed(), libc::SIGTERM) },
        0
    );
    let (status, _) = exited(&mut proxy, Duration::from_secs(20));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "a clean stop");
    let mut err = String::new();
    std::io::Read::read_to_string(&mut proxy.stderr.take().expect("stderr"), &mut err)
        .expect("stderr text");
    assert!(
        err.contains("cold start retrying (attempt 1 of 10): status-unreadable"),
        "{err}"
    );
    assert!(err.contains(", next try in 2 s"), "{err}");
    assert!(
        err.contains("; to fix: start the installer service"),
        "{err}"
    );
    assert!(err.contains(" ["), "the detail goes to the log: {err}");
    assert!(!err.contains("was not written"), "{err}");
}

/// Catches a verb that serves with the stop signals still blocked, as a launcher that blocked
/// them leaves it (the mask is inherited through `exec`, and a blocked signal is never
/// delivered, handler or not): started with SIGTERM and SIGINT blocked, the verb still ends
/// on each with status 0.
#[tokio::test(flavor = "current_thread")]
async fn a_launcher_that_blocked_the_stop_signals_cannot_make_the_verb_unstoppable() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let (path, ca) = served_config(&format!("masked-{signal}"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let mut child = start_masked(&path, &listener, Stdio::piped(), true);
        drop(listener);
        let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
            .await
            .expect("the proxy answers");
        assert_eq!(status, 200);
        // SAFETY: `kill` of our own child.
        assert_eq!(unsafe { libc::kill(child.id().cast_signed(), signal) }, 0);
        let (status, took) = exited(&mut child, Duration::from_secs(10));
        assert_eq!(
            status.and_then(|s| s.code()),
            Some(0),
            "signal {signal}, after {took:?}"
        );
    }
}

const UPDATE: &str = "/v1/update/frontend";
const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// The body of a `POST` that asks for an update to `COMMIT`.
fn post_body(request_id: &str) -> String {
    format!(r#"{{"commit":"{COMMIT}","request_id":"{request_id}","nodes":false}}"#)
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// The names in `dir`.
fn names_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// A verb serving on a port of its own that answers `/healthz`: its guard, its address, the CA
/// a client trusts and the config it was started with (the directories it was told to use are
/// next to it).
async fn serving(name: &str) -> (Reaped, SocketAddr, PathBuf, PathBuf) {
    let (config, ca) = served_config(name);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let proxy = start(&config, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);
    (proxy, addr, ca, config)
}

/// Catches an update route that is still the proxy's 503 `updates are not wired`, or that
/// reaches something other than the update API: with the operator token a `GET` is the API's
/// status of "no update" (200, no update id, no phase) although the root half has written no
/// `status.json` yet, and the verb opened the order spool and its counter where it was told.
#[tokio::test(flavor = "current_thread")]
async fn the_update_route_is_the_update_api() {
    let (_proxy, addr, ca, config) = serving("update-status").await;
    let (status, text) = call(addr, &ca, "GET", UPDATE, Some(TOKEN), b"").await;
    assert_eq!(status, 200, "{text}");
    let body = json(&text);
    assert_eq!(body["update_id"], serde_json::Value::Null, "{text}");
    assert_eq!(body["phase"], serde_json::Value::Null, "{text}");
    assert!(body["restart_pending"].is_boolean(), "{text}");
    assert!(orders_dir_of(&config).is_dir());
    assert!(state_of(&config).join("orders.json").is_file());
}

/// Catches a `POST` that goes as far as an order while nothing can take the update over
/// (the swap is not wired into the verb): the API refuses with 503 and its reason, no order
/// is in the spool, and no request id was saved, so the same request is a fresh one when the
/// driver is there.
#[tokio::test(flavor = "current_thread")]
async fn a_post_is_refused_while_no_handover_driver_is_wired_and_writes_nothing() {
    let (_proxy, addr, ca, config) = serving("update-post").await;
    let body = post_body("r1");
    let (status, text) = call(addr, &ca, "POST", UPDATE, Some(TOKEN), body.as_bytes()).await;
    assert_eq!(status, 503, "{text}");
    let answer = json(&text);
    assert_eq!(answer["error"], "handover_unavailable", "{text}");
    assert_eq!(answer["message"], "handover driver not wired", "{text}");
    assert_eq!(names_in(&orders_dir_of(&config)), Vec::<String>::new());
    assert!(!state_of(&config).join("requests.json").exists());
}

/// Catches an update route open to a caller the proxy should have refused. The API behind the
/// route authenticates nothing (the proxy checked the caller before it called), so this is
/// the one test that holds the property in the shipped binary: no token and a wrong token are
/// 401 on `GET` and on `POST`, a body over the proxy's 4096 bytes is 413, and none of them
/// reaches the API (no order, no saved request), while the same `POST` with the token does.
#[tokio::test(flavor = "current_thread")]
async fn the_proxy_refuses_before_the_update_api_is_called() {
    let (_proxy, addr, ca, config) = serving("update-auth").await;
    let body = post_body("r1");
    let wrong = Some("not-the-operator-token-0123456789-abcdef");
    for (method, bearer, payload) in [
        ("GET", None, ""),
        ("GET", wrong, ""),
        ("POST", None, body.as_str()),
        ("POST", wrong, body.as_str()),
    ] {
        let (status, text) = call(addr, &ca, method, UPDATE, bearer, payload.as_bytes()).await;
        assert_eq!(status, 401, "{method} with {bearer:?}: {text}");
    }
    let huge = vec![b' '; 4097];
    let (status, text) = call(addr, &ca, "POST", UPDATE, Some(TOKEN), &huge).await;
    assert_eq!(status, 413, "{text}");
    assert_eq!(names_in(&orders_dir_of(&config)), Vec::<String>::new());
    assert!(!state_of(&config).join("requests.json").exists());

    let (status, text) = call(addr, &ca, "POST", UPDATE, Some(TOKEN), body.as_bytes()).await;
    assert_eq!(status, 503, "with the token the API is reached: {text}");
}
