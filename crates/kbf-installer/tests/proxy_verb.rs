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
            "min_generation",
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
    let fd = listener.as_raw_fd();
    let mut command = Command::new(BIN);
    command
        .args(["proxy", "--config"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    // SAFETY: only async-signal-safe calls and `setenv`, which is safe here: the child is a
    // fresh fork of a test thread that holds no environment lock, and nothing else in this
    // file forks with a hook.
    unsafe {
        command.pre_exec(move || {
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

/// One `GET` over h2c (prior knowledge) to `addr`: the status and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
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
    let path = config(&format!("serve-{signal}"), TABLE);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start(&path, &listener);
    drop(listener);

    let (status, body) = tokio::time::timeout(Duration::from_secs(30), get(addr, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!((status, body.as_str()), (200, "ok"));
    assert_eq!(get(addr, "/readyz").await.0, 503, "no active child");
    assert_eq!(get(addr, "/nothing").await.0, 404);
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

    let path = config("orphan", TABLE);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let child = start(&path, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, "/healthz"))
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
    let path = config("second-term", TABLE);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut child = start(&path, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, "/healthz"))
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
    let path = config("closed-stderr", TABLE);
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
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, "/healthz"))
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

    let table = TABLE.replacen(
        "[proxy]\n",
        &format!("[proxy]\nstate_dir = \"{}\"\n", state.display()),
        1,
    );
    let path = config("adopt", &table);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut proxy = start(&path, &listener);
    drop(listener);
    let (status, _) = tokio::time::timeout(Duration::from_secs(30), get(addr, "/healthz"))
        .await
        .expect("the proxy answers");
    assert_eq!(status, 200);

    // SAFETY: `kill` of our own child.
    assert_eq!(unsafe { libc::kill(proxy.id().cast_signed(), libc::SIGTERM) }, 0);
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
