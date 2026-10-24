//! `kbf-installer proxy` in pool mode as a service manager runs it: a process with one
//! activated socket, `[[proxy.frontend]]` tables and an admin API on a loopback socket of its
//! own. No frontend runs here, so every member is up and none is ready; what is proved is the
//! verb: it takes the table, serves the admin API beside the one activated socket (and listens
//! on nothing else), drains and undrains a member when asked, and stops on SIGTERM with exit 0.
//! The balancing itself is `kbf-proxy`'s own tests and `proxy_pool.rs` of the integration crate.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use kbf_testkit::reaped::{self, Reaped};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

/// The state database of the host, as `installer.toml` names it. Nothing connects to it here.
const DATABASE: &str = "database = \"postgres:///kbf?host=/nonexistent\"\n";

/// The operator token the proxy reads at start (0600, 32 or more characters).
const TOKEN: &str = "operator-token-0123456789-abcdefghijkl";

/// Two members, on ports nothing listens on.
const MEMBERS: &str = "[[proxy.frontend]]\nname = \"a\"\nbase_port = 18000\n\
                       [[proxy.frontend]]\nname = \"b\"\nbase_port = 18010\n";

/// A new CA and a server certificate for `localhost` it signed, written (0600) to
/// `server.pem`, `server.key` and `ca.pem` in `dir`. Returns the CA file.
fn issue(dir: &Path) -> PathBuf {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose, SanType,
    };
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
    private(dir, "server.pem", &cert.pem());
    private(dir, "server.key", &key.serialize_pem());
    private(dir, "ca.pem", &ca.pem())
}

/// Writes `text` to `file` in `dir`, readable by its owner only.
fn private(dir: &Path, file: &str, text: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(file);
    fs::write(&path, text).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
    path
}

/// An `installer.toml` for a pool of the members in `MEMBERS` (and `more`, keys of `[proxy]`
/// itself), over real files in a scratch directory: the proxy's admin API is at `admin`, and
/// `database` is the top-level `database` line (a pool needs none).
fn pool_config(name: &str, admin: SocketAddr, more: &str, database: &str) -> PathBuf {
    let dir = kbf_host::testkit::scratch(&format!("proxy-pool-{name}"));
    let ca = issue(&dir);
    let token = private(&dir, "operator.token", TOKEN);
    let (cert, key) = (dir.join("server.pem"), dir.join("server.key"));
    let state = dir.join("state");
    fs::create_dir_all(&state).expect("state dir");
    let text = format!(
        "{database}[proxy]\noperator_token_file = {token:?}\nserver_cert_file = {cert:?}\n\
         server_key_file = {key:?}\nclient_ca_file = {ca:?}\n\
         proxy_auth_file = {:?}\nstate_dir = {state:?}\nadmin_listen = \"{admin}\"\n\
         {more}{MEMBERS}",
        dir.join("proxy-auth"),
    );
    let path = dir.join("installer.toml");
    fs::write(&path, text).expect("write");
    path
}

/// A loopback address that was free a moment ago.
fn free_address() -> SocketAddr {
    let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    socket.local_addr().expect("addr")
}

fn check(path: &Path) -> Output {
    Command::new(BIN)
        .args(["proxy", "--check", "--config"])
        .arg(path)
        .env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS")
        .output()
        .expect("runs")
}

/// Asserts that `--check` takes a pool table, with a state database named or not (a pool
/// migrates nothing), and refuses (exit 2, saying that the two-slot supervision was removed) a
/// table that has the slot table as well.
#[test]
fn check_takes_a_pool_table_and_refuses_one_that_is_also_a_slot_table() {
    let path = pool_config("check", free_address(), "", DATABASE);
    let out = check(&path);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let bare = pool_config("check-bare", free_address(), "", "");
    let out = check(&bare);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let slot = "\n[proxy.upstream]\nbase_port = 19000\n";
    let both = fs::read_to_string(&path).expect("read") + slot;
    let other = path.with_file_name("both.toml");
    fs::write(&other, both).expect("write");
    let out = check(&other);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("two-slot supervision was removed"), "{err}");
}

/// Starts the verb with `listener` as its descriptor 3 and `LISTEN_PID` set to the child's own
/// pid in the child, as a service manager does. The guard stops and reaps the child when it
/// drops.
fn start(path: &Path, listener: &TcpListener) -> Reaped {
    let fd = listener.as_raw_fd();
    let mut command = Command::new(BIN);
    command
        .args(["proxy", "--config"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: async-signal-safe calls (`fcntl`, `dup2`, `getpid`) and `setenv`, which is not: if
    // another thread of this test binary held the environment or allocator lock at `fork`, the
    // child would block before `exec` (the start then fails on its wait, never wrongly passes).
    // The pid is only known after `fork`, so it cannot be set earlier.
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
    // Descriptor 3 is the activated socket; nothing above it is inherited.
    reaped::spawn(&mut command, 4).expect("spawn kbf-installer")
}

/// One request to the admin API, tried again for up to 30 s while nothing listens yet: the
/// status code and the body.
fn admin(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let started = Instant::now();
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(stream) => break stream,
            Err(error) => {
                let waited = started.elapsed();
                assert!(waited < Duration::from_secs(30), "no admin API: {error}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    let request = format!("{head}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).expect("a status line");
    (code.parse().expect("a status code"), body.to_owned())
}

/// Whether a list of members in `text` shows `name` in `state`.
fn shows(text: &str, name: &str, state: &str) -> bool {
    text.contains(&format!("\"name\":\"{name}\",\"state\":\"{state}\""))
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
    ports.sort_unstable();
    ports
}

/// Asserts that the verb serves a pool from a table with no state database: it listens on the
/// activated socket and the admin address and on nothing else, the admin API lists the two
/// members up and not ready, a drain and an undrain change the state and answer the list, a
/// SIGHUP does not end the process, and SIGTERM stops it with exit 0 after it has said what it
/// is.
#[test]
fn a_pool_is_served_with_its_admin_api_and_sigterm_stops_it() {
    let admin_addr = free_address();
    let path = pool_config("serve", admin_addr, "", "");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let public = listener.local_addr().expect("addr").port();
    let mut child = start(&path, &listener);
    drop(listener);

    let (code, list) = admin(admin_addr, "GET", "/v1/proxy/upstreams");
    assert_eq!(code, 200, "{list}");
    for name in ["a", "b"] {
        let member = format!("\"name\":\"{name}\",\"state\":\"up\",\"ready\":false");
        assert!(list.contains(&member), "{list}");
    }
    let mut want = vec![public, admin_addr.port()];
    want.sort_unstable();
    assert_eq!(
        listening_ports(child.id()),
        want,
        "the two sockets and nothing else"
    );

    let (code, drained) = admin(admin_addr, "POST", "/v1/proxy/upstreams/a:drain");
    assert_eq!(code, 200, "{drained}");
    assert!(shows(&drained, "a", "draining"), "{drained}");
    let (code, undrained) = admin(admin_addr, "POST", "/v1/proxy/upstreams/a:undrain");
    assert_eq!(code, 200, "{undrained}");
    assert!(shows(&undrained, "a", "up"), "{undrained}");

    // SAFETY: `kill` of our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGHUP) }, 0);
    let (code, _) = admin(admin_addr, "GET", "/v1/proxy/upstreams");
    assert_eq!(code, 200, "still serving after a reload");

    let stopped = Instant::now();
    // SAFETY: `kill` of our own child.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        let waited = stopped.elapsed();
        assert!(waited < Duration::from_secs(20), "no exit on SIGTERM");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0), "a clean stop");
    let mut err = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err)
        .expect("stderr text");
    assert!(err.contains("pool of 2 frontends"), "{err}");
    assert!(err.contains("proxy serving"), "{err}");
}
