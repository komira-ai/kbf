//! `kbf-installer proxy` as a service manager runs it: a process with one activated socket.
//! Nothing here needs root or a child frontend; the proxy has no active child, so every
//! backend path is `UNAVAILABLE` and `/readyz` is 503, which is what the supervisor-less verb
//! promises.

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
    use std::os::unix::fs::PermissionsExt as _;
    let dir = kbf_host::testkit::scratch(&format!("proxy-bin-{name}"));
    let ca_file = issue(&dir, "");
    let private = |file: &str, text: &str| {
        let path = dir.join(file);
        fs::write(&path, text).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        path
    };
    let token = private("operator.token", "operator-token-0123456789-abcdefghijkl");
    let (cert, key) = (dir.join("server.pem"), dir.join("server.key"));
    let text = format!(
        "[proxy]\noperator_token_file = {token:?}\nserver_cert_file = {cert:?}\n\
         server_key_file = {key:?}\nclient_ca_file = {ca_file:?}\n\
         proxy_auth_file = {:?}\n[proxy.upstream]\nbase_port = 18000\n",
        dir.join("proxy-auth"),
    );
    // Not `config`: it makes the directory again, and the files above are in it.
    let path = dir.join("installer.toml");
    fs::write(&path, text).expect("write");
    (path, ca_file)
}

/// A new CA and a server certificate for `localhost` it signed, written (0600) to
/// `{prefix}ca.pem`, `{prefix}server.pem` and `{prefix}server.key` in `dir`. Returns the CA file.
fn issue(dir: &Path, prefix: &str) -> PathBuf {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, SanType,
    };
    use std::os::unix::fs::PermissionsExt as _;
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
        let path = dir.join(format!("{prefix}{file}"));
        fs::write(&path, text).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        path
    };
    private("server.pem", &cert.pem());
    private("server.key", &key.serialize_pem());
    private("ca.pem", &ca.pem())
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

/// [`start_with`], and with `block_term` the verb inherits SIGTERM, SIGINT and SIGHUP blocked, as from
/// a launcher that blocked them.
fn start_masked(path: &Path, listener: &TcpListener, stderr: Stdio, block_term: bool) -> Reaped {
    let fd = listener.as_raw_fd();
    let mut command = Command::new(BIN);
    command
        .args(["proxy", "--config"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
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
                libc::sigaddset(&raw mut set, libc::SIGHUP);
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
    try_tls(addr, ca).await.expect("TLS handshake")
}

/// [`tls`], and the handshake's failure instead of a panic: whether the server's certificate
/// chains to `ca`.
async fn try_tls(
    addr: SocketAddr,
    ca: &Path,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
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
}

/// One `GET` over TLS and h2 to `addr`: the status and the body.
async fn get(addr: SocketAddr, ca: &Path, path: &str) -> (u16, String) {
    let tcp = tls(addr, ca).await;
    let (send, connection) = h2::client::handshake(tcp).await.expect("h2 handshake");
    tokio::spawn(connection);
    let request = http::Request::builder()
        .uri(format!("http://proxy.test{path}"))
        .body(())
        .expect("a request");
    let (response, _) = send
        .ready()
        .await
        .expect("capacity")
        .send_request(request, true)
        .expect("send");
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

    let state = kbf_host::testkit::scratch("proxy-bin-adopt-state");
    let children = state.join("children");
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

    let (path, ca) = served_config("adopt");
    let table = fs::read_to_string(&path).expect("config text").replacen(
        "[proxy]\n",
        &format!("[proxy]\nstate_dir = \"{}\"\n", state.display()),
        1,
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
        err.contains("adopted the frontend child in slot A"),
        "the recorded child was not adopted: {err}"
    );
    // SAFETY: signal 0 only checks that the process exists; the child is ours.
    let alive = unsafe { libc::kill(frontend.id().cast_signed(), 0) };
    assert_eq!(alive, 0, "the frontend went down with the proxy");
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

/// Sends `signal` to the child.
fn send(child: &Reaped, signal: i32) {
    // SAFETY: `kill` of our own child.
    assert_eq!(unsafe { libc::kill(child.id().cast_signed(), signal) }, 0);
}

/// Waits until a handshake that trusts `ca` succeeds, or fails the test after 20 s.
async fn until_served_by(addr: SocketAddr, ca: &Path) {
    let started = Instant::now();
    while try_tls(addr, ca).await.is_err() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the certificate of {} is never served",
            ca.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Catches a verb that does not reload on SIGHUP, one that dies of it (the default action),
/// one that cuts the streams open at the reload, and one that cannot be reloaded when its
/// launcher blocked SIGHUP. The files are replaced as a renewal does (a new CA, certificate
/// and key renamed over the old ones); SIGHUP arrives; a new handshake then sees the new
/// certificate and the old CA no longer verifies it; the h2 connection opened before the
/// signal still answers afterwards; the verb is still running, and then stops on SIGTERM with 0.
#[tokio::test(flavor = "current_thread")]
async fn sighup_serves_the_renewed_certificate_and_cuts_no_stream() {
    let (path, served_ca) = served_config("hup-reload");
    let dir = path.parent().expect("dir").to_owned();
    // The file the proxy reads is replaced below; the client keeps trusting a copy of the old one.
    let ca_a = dir.join("a-ca.pem");
    fs::copy(&served_ca, &ca_a).expect("copy the old CA");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start_masked(&path, &listener, Stdio::piped(), true);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca_a, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    // A connection opened under the old certificate, kept open across the reload.
    let (open, connection) = h2::client::handshake(tls(addr, &ca_a).await)
        .await
        .expect("h2 handshake");
    tokio::spawn(connection);

    let ca_b = issue(&dir, "b-");
    for file in ["server.pem", "server.key"] {
        fs::rename(dir.join(format!("b-{file}")), dir.join(file)).expect("replace");
    }
    fs::copy(&ca_b, &served_ca).expect("replace the client CA");
    // Before the signal nothing changes: a renewal alone reloads nothing.
    assert!(
        try_tls(addr, &ca_b).await.is_err(),
        "reloaded without a signal"
    );
    send(&child, libc::SIGHUP);
    until_served_by(addr, &ca_b).await;
    assert!(
        try_tls(addr, &ca_a).await.is_err(),
        "the old certificate is still served"
    );
    assert!(
        child.try_wait().expect("wait").is_none(),
        "SIGHUP stopped the verb"
    );

    let request = http::Request::builder()
        .uri("http://proxy.test/healthz")
        .body(())
        .expect("a request");
    let (response, _) = open
        .ready()
        .await
        .expect("the old connection is open")
        .send_request(request, true)
        .expect("send");
    assert_eq!(response.await.expect("a response").status().as_u16(), 200);

    send(&child, libc::SIGTERM);
    let (status, took) = exited(&mut child, Duration::from_secs(10));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "after {took:?}");
}

/// Catches a reload that swaps before every file is checked, and one that says nothing: with
/// the key file gone SIGHUP leaves the old certificate served (the new one is never
/// half-applied), the verb keeps running, and its standard error says once that the files are
/// unusable and names the key file. A later SIGHUP with the key back reloads.
#[tokio::test(flavor = "current_thread")]
async fn a_sighup_with_a_missing_key_keeps_the_old_certificate_and_names_the_file() {
    let (path, served_ca) = served_config("hup-missing-key");
    let dir = path.parent().expect("dir").to_owned();
    let ca_a = dir.join("a-ca.pem");
    fs::copy(&served_ca, &ca_a).expect("copy the old CA");
    let log = dir.join("verb.log");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start_with(
        &path,
        &listener,
        Stdio::from(fs::File::create(&log).expect("log file")),
    );
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca_a, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    // A renewed certificate and CA are in place, the key is not: the three do not make a
    // unit, so none of them may apply.
    let ca_b = issue(&dir, "b-");
    fs::rename(dir.join("b-server.pem"), dir.join("server.pem")).expect("replace");
    fs::copy(&ca_b, &served_ca).expect("replace the client CA");
    let key = dir.join("server.key");
    fs::rename(&key, dir.join("old.key")).expect("move the key away");
    send(&child, libc::SIGHUP);
    let started = Instant::now();
    let said = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if text.contains("previous ones are kept") {
            break text;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "no line: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(said.matches("previous ones are kept").count(), 1, "{said}");
    assert!(
        said.contains(&key.display().to_string()),
        "no file named: {said}"
    );
    assert!(
        try_tls(addr, &ca_b).await.is_err(),
        "the new certificate half-applied"
    );
    assert!(
        child.try_wait().expect("wait").is_none(),
        "a failed reload stopped the verb"
    );

    fs::rename(dir.join("b-server.key"), &key).expect("the renewed key");
    send(&child, libc::SIGHUP);
    until_served_by(addr, &ca_b).await;
    send(&child, libc::SIGTERM);
    let (status, took) = exited(&mut child, Duration::from_secs(10));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "after {took:?}");
}

/// Sends SIGHUP to `child` over and over until its standard error (`log`) says it reloaded,
/// which only a registered handler does. The flood has no fixed length, so it covers the
/// exposed window whatever the start-up speed; it fails the test if the verb stops first or
/// the line never comes within 30 s.
fn flood_hup_until_reloaded(child: &mut Reaped, log: &Path) {
    let started = Instant::now();
    while !fs::read_to_string(log)
        .unwrap_or_default()
        .contains("reloaded the TLS files")
    {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the SIGHUPs never reloaded"
        );
        assert!(
            child.try_wait().expect("wait").is_none(),
            "an early SIGHUP stopped the verb"
        );
        send(child, libc::SIGHUP);
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// Catches a verb that dies of a SIGHUP sent before its handler exists (`ExecReload` can run
/// as soon as the unit is active, and the default action of SIGHUP ends the process). The
/// launcher here blocks SIGHUP, so the signals sent from the spawn on stay pending in the
/// verb until it unblocks them; unblocking before the handler is registered delivers them to
/// the default action. SIGHUP is sent without pause until the verb has logged a reload, so
/// the exposed window is covered however slowly it starts. The verb must survive, serve, and
/// still stop on SIGTERM with 0.
#[tokio::test(flavor = "current_thread")]
async fn a_sighup_sent_before_the_handler_exists_does_not_kill_the_verb() {
    let (path, ca) = served_config("hup-early");
    let log = path.parent().expect("dir").join("verb.log");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start_masked(
        &path,
        &listener,
        Stdio::from(fs::File::create(&log).expect("log file")),
        true,
    );
    drop(listener);
    flood_hup_until_reloaded(&mut child, &log);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);
    send(&child, libc::SIGTERM);
    let (status, took) = exited(&mut child, Duration::from_secs(10));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "after {took:?}");
}

/// The start-up window under a service manager, which blocks nothing: the launcher leaves the
/// mask alone, so only the verb's own `hold_reload_signal` keeps SIGHUP from the default
/// action until the handler exists. To make that window as long as the test wants, `--config`
/// is a FIFO: the verb sits in its first read of the configuration, after the hold and long
/// before any handler, until this test writes it. The test sees the verb open the FIFO (a
/// non-blocking open for writing fails until a reader exists), sends SIGHUP into the wait,
/// then writes the configuration and floods SIGHUP until a reload is logged. A verb without
/// the hold dies of the first one. The limit: a SIGHUP in the instants between `exec` and the
/// hold cannot be survived by any implementation, so none is sent there.
#[tokio::test(flavor = "current_thread")]
async fn a_sighup_in_the_start_up_window_under_an_unmasked_launcher_does_not_kill_the_verb() {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let (config, ca) = served_config("hup-unmasked");
    let text = fs::read(&config).expect("the configuration");
    let dir = config.parent().expect("dir").to_owned();
    let fifo = dir.join("fifo.toml");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("a path");
    // SAFETY: `mkfifo` of a path this test owns.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let log = dir.join("verb.log");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start_masked(
        &fifo,
        &listener,
        Stdio::from(fs::File::create(&log).expect("log file")),
        false,
    );
    drop(listener);
    let started = Instant::now();
    let mut writer = loop {
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
        {
            Ok(file) => break file,
            Err(_) => {
                assert!(
                    started.elapsed() < Duration::from_secs(30),
                    "the verb never opened its configuration"
                );
                assert!(
                    child.try_wait().expect("wait").is_none(),
                    "the verb stopped before it read its configuration"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    };
    for _ in 0..5 {
        send(&child, libc::SIGHUP);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        child.try_wait().expect("wait").is_none(),
        "a SIGHUP while the verb read its configuration stopped it"
    );
    std::io::Write::write_all(&mut writer, &text).expect("write the configuration");
    drop(writer);
    flood_hup_until_reloaded(&mut child, &log);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);
    send(&child, libc::SIGTERM);
    let (status, took) = exited(&mut child, Duration::from_secs(10));
    assert_eq!(status.and_then(|s| s.code()), Some(0), "after {took:?}");
}

/// Catches a reload that cuts streams open at the swap: 50 connections each hold a stream
/// (a request sent but not yet answered to the client) across a SIGHUP that installs a renewed certificate,
/// and every one completes with 200 afterwards, on a connection that still takes a new request.
#[tokio::test(flavor = "current_thread")]
async fn fifty_open_streams_all_complete_across_a_reload() {
    let (path, served_ca) = served_config("hup-streams");
    let dir = path.parent().expect("dir").to_owned();
    let ca_a = dir.join("a-ca.pem");
    fs::copy(&served_ca, &ca_a).expect("copy the old CA");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let child = start_masked(&path, &listener, Stdio::piped(), true);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca_a, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    let mut open = Vec::new();
    for _ in 0..50 {
        let (send_request, connection) = h2::client::handshake(tls(addr, &ca_a).await)
            .await
            .expect("h2 handshake");
        tokio::spawn(connection);
        let request = http::Request::builder()
            .method("POST")
            .uri("http://proxy.test/healthz")
            .body(())
            .expect("a request");
        let (response, body) = send_request
            .clone()
            .ready()
            .await
            .expect("capacity")
            .send_request(request, false)
            .expect("send");
        open.push((send_request, response, body));
    }

    let ca_b = issue(&dir, "b-");
    for file in ["server.pem", "server.key"] {
        fs::rename(dir.join(format!("b-{file}")), dir.join(file)).expect("replace");
    }
    fs::copy(&ca_b, &served_ca).expect("replace the client CA");
    send(&child, libc::SIGHUP);
    until_served_by(addr, &ca_b).await;

    // Each stream sent before the reload is answered, and its connection takes a new request.
    for (send_request, response, _body) in open {
        let first = tokio::time::timeout(Duration::from_secs(20), response)
            .await
            .expect("an answer")
            .expect("the stream survived the reload");
        assert_eq!(first.status().as_u16(), 200);
        let request = http::Request::builder()
            .uri("http://proxy.test/healthz")
            .body(())
            .expect("a request");
        let (response, _) = send_request
            .ready()
            .await
            .expect("the connection survived the reload")
            .send_request(request, true)
            .expect("send");
        assert_eq!(response.await.expect("a response").status().as_u16(), 200);
    }
}

/// Catches a handshake that fails while the certificate is swapped: a client that trusts both
/// the old and the new CA handshakes in a tight loop while SIGHUP arrives, and every
/// handshake, before, during and after the swap, succeeds (it sees the old certificate or the
/// new one, never an error).
#[tokio::test(flavor = "current_thread")]
async fn handshakes_racing_a_reload_see_the_old_or_the_new_certificate() {
    let (path, served_ca) = served_config("hup-race");
    let dir = path.parent().expect("dir").to_owned();
    let ca_a = dir.join("a-ca.pem");
    fs::copy(&served_ca, &ca_a).expect("copy the old CA");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let child = start_masked(&path, &listener, Stdio::piped(), true);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, &ca_a, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    let ca_b = issue(&dir, "b-");
    for file in ["server.pem", "server.key"] {
        fs::rename(dir.join(format!("b-{file}")), dir.join(file)).expect("replace");
    }
    let both = dir.join("both-ca.pem");
    let mut text = fs::read_to_string(&ca_a).expect("old CA");
    text.push_str(&fs::read_to_string(&ca_b).expect("new CA"));
    fs::write(&both, text).expect("both CAs");
    fs::copy(&ca_b, &served_ca).expect("replace the client CA");

    let mut handshakes = 0_u32;
    let mut seen_new = false;
    let mut signalled = false;
    let started = Instant::now();
    while !seen_new || handshakes < 200 {
        assert!(started.elapsed() < Duration::from_secs(30), "no swap seen");
        if handshakes == 50 && !signalled {
            send(&child, libc::SIGHUP);
            signalled = true;
        }
        try_tls(addr, &both)
            .await
            .unwrap_or_else(|e| panic!("handshake {handshakes} failed: {e}"));
        handshakes += 1;
        seen_new = signalled && try_tls(addr, &ca_b).await.is_ok();
    }
}

/// Catches a SIGHUP that cancels or restarts a stop under way: SIGTERM with a client holding a
/// connection starts the drain, a SIGHUP follows, and the verb still ends within the stop
/// bound of the SIGTERM (it does not keep serving, and does not hang on the reload).
#[tokio::test(flavor = "current_thread")]
async fn a_sighup_during_the_stop_does_not_cancel_it() {
    let (path, ca) = served_config("hup-during-stop");
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

    send(&child, libc::SIGTERM);
    let (early, _) = exited(&mut child, Duration::from_millis(500));
    assert!(
        early.is_none(),
        "the SIGTERM drains while a client is connected"
    );
    send(&child, libc::SIGHUP);
    let (status, took) = exited(&mut child, Duration::from_secs(12));
    let status = status.unwrap_or_else(|| panic!("still running {took:?} after the SIGTERM"));
    assert!(status.code().is_some(), "ended by a signal: {status:?}");
    assert!(took < Duration::from_secs(10), "after {took:?}");
}
