//! The `tierstore` binary with its listening sockets passed by a service manager, as systemd's
//! socket activation does: the test binds the sockets, starts the binary with them as descriptors
//! 3, 4, ... and `LISTEN_PID`, `LISTEN_FDS` and `LISTEN_FDNAMES` in its environment, and holds
//! them while the binary stops and starts again.
//!
//! No test here sleeps or asserts how long anything takes. The order of the events is made by
//! what the test waits for: the start line of a process, its exit status, a request written to
//! a socket that nothing is accepting on, the answers that come back. A process that hangs is
//! ended by the hang guard of `.config/nextest.toml`.

use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_objstore::s3::fake::FakeS3;
use kbf_testkit::reaped::{Reaped, first_line, spawn};
use rustix::process::{Pid as ProcessId, Signal, kill_process};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::files::{BIN_KEY_ID, BIN_SECRET, Files};
use crate::signer::{Request, encode, sign};

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// The bucket the binaries of these tests serve.
const BUCKET: &str = "tier-bucket";

/// How a test says `LISTEN_PID` is to be set in the child.
#[derive(Clone, Copy)]
enum Pid {
    /// The child's own process id, which only the child knows: the service manager does this.
    Own,
    /// The process id of somebody else.
    Other(u32),
    /// Not set.
    Unset,
}

/// What the test plays the service manager with: the sockets to pass (descriptors 3, 4, ... in
/// this order) and what the three variables say.
struct Passing<'a> {
    sockets: &'a [RawFd],
    pid: Pid,
    /// The text of `LISTEN_FDS`; `None` leaves it out, and `Some("")` is replaced by the number
    /// of sockets.
    count: Option<&'a str>,
    /// The text of `LISTEN_FDNAMES`; `None` leaves it out.
    names: Option<&'a str>,
}

/// Nothing passed and no variable set: a start without a service manager.
fn plain() -> Passing<'static> {
    Passing {
        sockets: &[],
        pid: Pid::Unset,
        count: None,
        names: None,
    }
}

impl<'a> Passing<'a> {
    /// What systemd passes for `sockets` named `names`: everything right.
    fn named(sockets: &'a [RawFd], names: &'a str) -> Self {
        Self {
            sockets,
            pid: Pid::Own,
            count: Some(""),
            names: Some(names),
        }
    }
}

/// Arms `command` to run with the sockets of `passing` as its descriptors from 3 up.
fn arm(command: &mut Command, passing: &Passing<'_>) {
    assert!(passing.sockets.len() <= 4);
    let mut sockets = [-1; 4];
    sockets[..passing.sockets.len()].copy_from_slice(passing.sockets);
    let count = match passing.count {
        Some("") => Some(passing.sockets.len().to_string()),
        other => other.map(str::to_owned),
    };
    let count = count.map(|text| CString::new(text).unwrap());
    let names = passing.names.map(|text| CString::new(text).unwrap());
    let pid = passing.pid;
    let other = match pid {
        Pid::Other(pid) => Some(CString::new(pid.to_string()).unwrap()),
        Pid::Own | Pid::Unset => None,
    };
    // SAFETY: async-signal-safe calls (`fcntl`, `dup2`, `getpid`, `unsetenv`) and `setenv`,
    // which is not: if another thread of this test binary held the environment or allocator
    // lock at `fork`, the child would block before `exec` (the test then waits for ever and the
    // hang guard ends it; it never wrongly passes). The process id of the child is only known
    // after `fork`, so `LISTEN_PID` cannot be set earlier. `Command::env` is not used: std
    // hands its own copy of the environment to `exec` as soon as one variable is set on a
    // command, and that copy would not have what `setenv` sets here.
    unsafe {
        command.pre_exec(move || {
            // Out of the way of the descriptors 3, 4, ... that are about to be filled.
            let mut high = [-1; 4];
            for (at, socket) in sockets.iter().enumerate().filter(|(_, s)| **s >= 0) {
                high[at] = libc::fcntl(*socket, libc::F_DUPFD_CLOEXEC, 100);
                if high[at] < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            for (at, moved) in high.iter().enumerate().filter(|(_, h)| **h >= 0) {
                let target = 3 + libc::c_int::try_from(at).unwrap();
                if libc::dup2(*moved, target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            match (&other, pid) {
                (Some(text), _) => {
                    libc::setenv(c"LISTEN_PID".as_ptr(), text.as_ptr(), 1);
                }
                (None, Pid::Own) => {
                    let mut digits = [0_u8; 12];
                    let mut at = digits.len() - 1;
                    let mut own = libc::getpid();
                    while own > 0 {
                        at -= 1;
                        digits[at] = b'0' + (own % 10) as u8;
                        own /= 10;
                    }
                    libc::setenv(c"LISTEN_PID".as_ptr(), digits[at..].as_ptr().cast(), 1);
                }
                (None, _) => {
                    libc::unsetenv(c"LISTEN_PID".as_ptr());
                }
            }
            match &count {
                Some(text) => libc::setenv(c"LISTEN_FDS".as_ptr(), text.as_ptr(), 1),
                None => libc::unsetenv(c"LISTEN_FDS".as_ptr()),
            };
            match &names {
                Some(text) => libc::setenv(c"LISTEN_FDNAMES".as_ptr(), text.as_ptr(), 1),
                None => libc::unsetenv(c"LISTEN_FDNAMES".as_ptr()),
            };
            Ok(())
        });
    }
}

/// The end of a process that was refused to start: its status and what it wrote.
struct Refusal {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs the binary with `flags` and `passing` to its end.
fn run_passing(flags: &[String], passing: &Passing<'_>) -> Refusal {
    let mut command = Command::new(BIN);
    command.args(flags).stdin(Stdio::null());
    arm(&mut command, passing);
    let out = command.output().unwrap();
    Refusal {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Asserts that a start was refused with status 2 and nothing on stdout, with `want` in the
/// message.
fn assert_refused(refusal: &Refusal, want: &str) {
    assert_eq!(refusal.code, Some(2), "{}", refusal.stderr);
    assert_eq!(refusal.stdout, "");
    assert!(
        refusal.stderr.starts_with("tierstore: "),
        "{}",
        refusal.stderr
    );
    assert!(refusal.stderr.contains(want), "{want}: {}", refusal.stderr);
}

/// A running binary, and what its start line said.
struct Process {
    child: Option<Reaped>,
    pid: ProcessId,
    line: String,
    /// Keeps the pipe of the binary's stdout open.
    _stdout: std::io::BufReader<std::process::ChildStdout>,
}

/// The longest `first_line` is let to wait for the start line. It only stops a process that
/// never prints one, and then the test fails; the hang guard of `.config/nextest.toml` ends a
/// test that waits for longer.
const START_LINE_WAIT: Duration = Duration::from_secs(3600);

/// Starts the binary with `flags`, passing the sockets as `passing` says, and waits for its start
/// line.
fn start(flags: &[String], passing: &Passing<'_>) -> Process {
    let mut command = Command::new(BIN);
    command
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    arm(&mut command, passing);
    let keep_below = 3 + i32::try_from(passing.sockets.len()).unwrap();
    let mut child = spawn(&mut command, keep_below).unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line, reader) = first_line(stdout, START_LINE_WAIT).unwrap();
    assert!(line.starts_with("tierstore "), "{line}");
    let pid = ProcessId::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    Process {
        child: Some(child),
        pid,
        line,
        _stdout: reader,
    }
}

impl Process {
    /// The words of the start line that begin with `key=`, their values.
    fn said(&self, key: &str) -> Option<&str> {
        let prefix = format!("{key}=");
        self.line
            .split_whitespace()
            .find_map(|word| word.strip_prefix(prefix.as_str()))
    }

    /// Sends SIGTERM and waits for the process to exit: its exit code.
    async fn stop(&mut self) -> Option<i32> {
        self.term();
        self.wait().await
    }

    /// Sends SIGTERM.
    fn term(&self) {
        kill_process(self.pid, Signal::TERM).unwrap();
    }

    /// Waits for the process to exit: its exit code.
    async fn wait(&mut self) -> Option<i32> {
        let mut child = self.child.take().expect("not stopped yet");
        let waiting = tokio::task::spawn_blocking(move || {
            let status = child.wait();
            (child, status)
        });
        let (_child, status) = waiting.await.unwrap();
        status.unwrap().code()
    }
}

/// The date of an S3 request, now.
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (days, rest) = (i64::try_from(secs / 86400).unwrap(), secs % 86400);
    // The civil date of a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// A signed request for the key of the key file, as bytes to write to a socket: one request on
/// a connection that the server closes after the answer.
fn signed(addr: SocketAddr, method: &str, key: &str, create: bool, body: &[u8]) -> Vec<u8> {
    build(addr, method, key, create, body, true)
}

/// The same, on a connection that stays open after the answer unless `close`.
fn build(
    addr: SocketAddr,
    method: &str,
    key: &str,
    create: bool,
    body: &[u8],
    close: bool,
) -> Vec<u8> {
    let path = format!("/{BUCKET}/{key}");
    let host = addr.to_string();
    let headers: &[(&str, &str)] = if create {
        &[("if-none-match", "*")]
    } else {
        &[]
    };
    let request = Request {
        method,
        host: &host,
        path: &path,
        query: &[],
        headers,
        body,
        stamp: &stamp(),
    };
    let mut text = format!("{method} {} HTTP/1.1\r\n", encode(&path, b"/"));
    for (name, value) in sign(&request, BIN_KEY_ID, BIN_SECRET) {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    let connection = if close { "connection: close\r\n" } else { "" };
    text.push_str(&format!(
        "content-length: {}\r\n{connection}\r\n",
        body.len()
    ));
    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// An answer: the status, the id the server gave the request, and the body.
#[derive(Debug)]
struct Answer {
    status: u16,
    id: u64,
    /// Whether the answer says `Connection: close`.
    close: bool,
    body: Vec<u8>,
}

/// Parses everything a server wrote before it closed the connection.
fn parse(bytes: &[u8]) -> Result<Answer, String> {
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| format!("no head in {} bytes", bytes.len()))?;
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|word| word.parse().ok())
        .ok_or_else(|| format!("no status in {head:?}"))?;
    let id = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("x-amz-request-id: ")
                .map(str::to_owned)
        })
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .ok_or_else(|| format!("no request id in {head:?}"))?;
    let close = head
        .lines()
        .any(|line| line.eq_ignore_ascii_case("connection: close"));
    Ok(Answer {
        status,
        id,
        close,
        body: bytes[split + 4..].to_vec(),
    })
}

/// Connects to `addr` and writes `request`. The connection is made by the kernel with nothing
/// accepting on the socket if nothing is: it waits in the backlog, and the request in its buffer.
async fn send(addr: SocketAddr, request: &[u8]) -> std::io::Result<TcpStream> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(request).await?;
    Ok(stream)
}

/// Reads the answer on `stream` to the end of the connection.
async fn receive(mut stream: TcpStream) -> Result<Answer, String> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .map_err(|why| format!("reading the answer: {why}"))?;
    parse(&bytes)
}

/// One request and its answer; any failure of the connection is the `Err`.
async fn call(
    addr: SocketAddr,
    method: &str,
    key: &str,
    create: bool,
    body: &[u8],
) -> Result<Answer, String> {
    let request = signed(addr, method, key, create, body);
    let stream = send(addr, &request)
        .await
        .map_err(|why| format!("{method} {key}: {why} ({:?})", why.kind()))?;
    receive(stream)
        .await
        .map_err(|why| format!("{method} {key}: {why}"))
}

/// The body of the object `client` writes in round `round`.
fn body_of(label: &str, client: usize, round: usize) -> Vec<u8> {
    format!("object {label}/{client}/{round}").into_bytes()
}

/// The key `client` writes in round `round`, under the prefix both shapes serve and the cache
/// keeps.
fn key_of(label: &str, client: usize, round: usize) -> String {
    format!("cas/{label}-{client}-{round}")
}

/// What a request has to be to have succeeded.
fn check(answer: Result<Answer, String>, status: u16, body: Option<&[u8]>) -> Result<u64, String> {
    let answer = answer?;
    if answer.status != status {
        return Err(format!(
            "status {}: {}",
            answer.status,
            String::from_utf8_lossy(&answer.body)
        ));
    }
    match body {
        Some(want) if want != answer.body.as_slice() => {
            Err("the body is not the object".to_owned())
        }
        _ => Ok(answer.id),
    }
}

/// `clients` tasks each writing `rounds` objects create-only and reading each back, at once.
/// The ids of the answers, or the first failure of each task.
async fn hammer(
    addr: SocketAddr,
    label: &'static str,
    clients: usize,
    rounds: usize,
) -> Vec<Result<u64, String>> {
    let mut tasks = Vec::new();
    for client in 0..clients {
        tasks.push(tokio::spawn(async move {
            let mut results = Vec::new();
            for round in 0..rounds {
                let (key, body) = (key_of(label, client, round), body_of(label, client, round));
                results.push(check(call(addr, "PUT", &key, true, &body).await, 200, None));
                results.push(check(
                    call(addr, "GET", &key, false, b"").await,
                    200,
                    Some(&body),
                ));
            }
            results
        }));
    }
    let mut all = Vec::new();
    for task in tasks {
        all.extend(task.await.unwrap());
    }
    all
}

/// Asserts that every request in `results` succeeded and that the ids are those of a process that
/// began counting at 1 and answered `results.len()` requests (and `extra` more): so `results` is
/// the work of a process that was started for it and not of one that had been serving before.
fn assert_all_served(results: &[Result<u64, String>], extra: u64, who: &str) {
    let failed: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(failed.is_empty(), "{who}: {failed:?}");
    let ids: Vec<u64> = results.iter().map(|r| *r.as_ref().unwrap()).collect();
    let most = u64::try_from(ids.len()).unwrap() + extra;
    assert!(!ids.is_empty(), "{who} served nothing");
    assert!(
        ids.iter().all(|id| (1..=most).contains(id)),
        "{who}: {ids:?} is not from a process that began at 1"
    );
}

/// How the process under test is configured.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// A storage node over two directory tiers, by flags.
    Storage,
    /// A build node: a write-through cache over an S3 upstream, by a file.
    Cache,
}

/// The files and the flags of a shape, and the upstream of a cache.
struct Node {
    files: Files,
    /// Keeps the upstream of a cache node up.
    _upstream: Option<FakeS3>,
    /// The flags that start it, given the addresses it serves and whether the sockets come from
    /// the service manager.
    flags: Box<dyn Fn(SocketAddr, Option<SocketAddr>, bool) -> Vec<String>>,
}

impl Node {
    async fn new(shape: Shape, name: &str) -> Self {
        let files = Files::new(name);
        match shape {
            Shape::Storage => {
                let root = files.key.parent().unwrap().to_path_buf();
                let base = files.flags(&[
                    "--drain-lb-wait",
                    "0s",
                    "--reconcile-interval",
                    "100ms",
                    "--lease-ttl",
                    "1s",
                ]);
                let hot = format!("dir:{}", root.join("hot").display());
                let cold = format!("dir:{}", root.join("cold").display());
                let base = crate::files::with(&base, "--hot", &hot);
                let base = crate::files::with(&base, "--cold", &cold);
                let flags = move |listen: SocketAddr, oci: Option<SocketAddr>, systemd: bool| {
                    let mut flags = crate::files::with(&base, "--listen", &listen.to_string());
                    if let Some(oci) = oci {
                        flags.extend(["--oci-listen".to_owned(), oci.to_string()]);
                    }
                    if systemd {
                        flags.push("--listen-from-systemd".to_owned());
                    }
                    flags
                };
                Self {
                    files,
                    _upstream: None,
                    flags: Box::new(flags),
                }
            }
            Shape::Cache => {
                let fake = FakeS3::start().await;
                let upstream_key = files.secret("upstream.key", "UPKEY\nup-secret\n");
                let cache = files.key.with_file_name("cache");
                let template = format!(
                    r#"
[server]
listen          = "LISTEN"
proxy_id        = "bin-cache"
bucket          = "{BUCKET}"
access_key_file = "{key}"
SYSTEMD
[local_cache]
path          = "{cache}"
budget        = "1MiB"
write_through = true

[cold_store]
kind             = "s3"
bucket           = "kbf-fake"
endpoint         = "{endpoint}"
credentials_file = "{upstream_key}"

[[policy]]
prefix = "cas/"
cache  = true
OCI"#,
                    key = files.key.display(),
                    cache = cache.display(),
                    endpoint = fake.endpoint(),
                    upstream_key = upstream_key.display(),
                );
                let dir = files.key.with_file_name("node");
                std::fs::create_dir_all(&dir).unwrap();
                let flags = move |listen: SocketAddr, oci: Option<SocketAddr>, systemd: bool| {
                    let systemd = if systemd {
                        "listen_from_systemd = true\n"
                    } else {
                        ""
                    };
                    let oci = oci
                        .map(|oci| format!("\n[oci]\nlisten = \"{oci}\"\nprefix = \"cas/\"\n"))
                        .unwrap_or_default();
                    let text = template
                        .replace("LISTEN", &listen.to_string())
                        .replace("SYSTEMD\n", systemd)
                        .replace("OCI", &oci);
                    let file = dir.join("node.toml");
                    std::fs::write(&file, text).unwrap();
                    std::fs::set_permissions(
                        &file,
                        std::os::unix::fs::PermissionsExt::from_mode(0o600),
                    )
                    .unwrap();
                    ["--config", file.to_str().unwrap(), "--drain-lb-wait", "0s"]
                        .map(str::to_owned)
                        .to_vec()
                };
                Self {
                    files,
                    _upstream: Some(fake),
                    flags: Box::new(flags),
                }
            }
        }
    }

    /// The flags of a start serving `listen` (and `oci`), from the service manager or not.
    fn flags(&self, listen: SocketAddr, oci: Option<SocketAddr>, systemd: bool) -> Vec<String> {
        (self.flags)(listen, oci, systemd)
    }
}

/// A socket the test binds as the service manager would, and its address.
fn bound() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Asserts that a restart loses no request, in both shapes of the node. The test plays the
/// service manager: it binds the socket and keeps it for the whole test.
///
/// - Process 1 answers a burst of 4 clients, each writing 5 objects create-only and reading each
///   back, every answer a success.
/// - Process 1 gets SIGTERM and exits with status 0. While nothing is running, 4 clients connect
///   and write a create-only put: the kernel takes the connections and holds the requests. None
///   is refused, and the test goes on only when all four have written their request.
/// - Process 2 is started on the same socket. It answers the four waiting requests, every one a
///   success, and then a burst of its own, which reads the objects process 1 had written too.
///
/// Both processes served: the ids a process gives its requests begin at 1, so the ids of the
/// answers of each phase are those of a process that was started for it. No request is repeated
/// or retried by the client.
async fn restart_loses_no_request(shape: Shape, name: &str) {
    let node = Node::new(shape, name).await;
    let (socket, addr) = bound();
    let fds = [socket.as_raw_fd()];
    let flags = node.flags(addr, None, true);

    let mut first = start(&flags, &Passing::named(&fds, "s3"));
    assert_eq!(first.said("listen"), Some(addr.to_string().as_str()));
    let served = hammer(addr, "a", 4, 5).await;
    assert_eq!(served.len(), 40);
    assert_all_served(&served, 0, "process 1");
    assert_eq!(first.stop().await, Some(0), "{shape:?}");

    // Nothing accepts now. These connect all the same, and write.
    let mut waiting = Vec::new();
    for client in 0..4 {
        let (key, body) = (key_of("b", client, 0), body_of("b", client, 0));
        let request = signed(addr, "PUT", &key, true, &body);
        let stream = send(addr, &request).await;
        waiting
            .push(stream.unwrap_or_else(|why| panic!("{shape:?}: refused with no process: {why}")));
    }

    let mut second = start(&flags, &Passing::named(&fds, "s3"));
    assert_eq!(second.said("listen"), Some(addr.to_string().as_str()));
    let mut answers = Vec::new();
    for stream in waiting {
        answers.push(receive(stream).await.and_then(|a| check(Ok(a), 200, None)));
    }
    assert_all_served(&answers, 0, "process 2, the waiting requests");
    let served = hammer(addr, "c", 4, 5).await;
    assert_all_served(&served, 4, "process 2");
    // What process 1 wrote is there for process 2.
    let (key, body) = (key_of("a", 3, 4), body_of("a", 3, 4));
    let old = check(call(addr, "GET", &key, false, b"").await, 200, Some(&body));
    assert!(old.is_ok(), "{shape:?}: {old:?}");
    assert_eq!(second.stop().await, Some(0), "{shape:?}");
}

/// See [`restart_loses_no_request`]: a storage node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_node_restarted_on_its_socket_loses_no_request() {
    restart_loses_no_request(Shape::Storage, "act-restart-storage").await;
}

/// See [`restart_loses_no_request`]: a build node, which production runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_node_restarted_on_its_socket_loses_no_request() {
    restart_loses_no_request(Shape::Cache, "act-restart-cache").await;
}

/// Asserts what the restart test is not vacuous against: a node that binds its own address, as
/// without a service manager, stopped and started again on the same address, refuses the
/// connection made between the two. The refusal is the kernel's, as no socket listens then; the
/// test connects after the exit status of the first process and before the start of the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_binds_its_own_address_refuses_connections_between_two_processes() {
    let node = Node::new(Shape::Storage, "act-rebind").await;
    let free: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut first = start(&node.flags(free, None, false), &plain());
    let addr: SocketAddr = first.said("listen").unwrap().parse().unwrap();
    let put = call(addr, "PUT", "cas/x", true, b"x").await;
    assert!(check(put, 200, None).is_ok());
    assert_eq!(first.stop().await, Some(0));

    let request = signed(addr, "PUT", "cas/y", true, b"y");
    let refused = send(addr, &request).await.expect_err("nothing listens");
    assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);

    let mut second = start(&node.flags(addr, None, false), &plain());
    let get = call(addr, "GET", "cas/x", false, b"").await;
    assert!(check(get, 200, Some(b"x")).is_ok());
    assert_eq!(second.stop().await, Some(0));
}

/// Asserts that without `--listen-from-systemd` the variables are ignored: a process that is
/// told by them that a socket is its own, with a socket in descriptor 3, binds its own address
/// and not that socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_flag_the_variables_are_ignored() {
    let node = Node::new(Shape::Storage, "act-ignored").await;
    let (socket, held) = bound();
    let fds = [socket.as_raw_fd()];
    let free: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut running = start(&node.flags(free, None, false), &Passing::named(&fds, "s3"));
    let said: SocketAddr = running.said("listen").unwrap().parse().unwrap();
    assert_ne!(said, held);
    assert_eq!(running.stop().await, Some(0));
}

/// Asserts that both listeners of a node come from the service manager, each by its name: the S3
/// API from the socket named `s3` and the OCI front from the one named `oci`, whatever the order
/// the manager passes them in; each answers on the socket the test bound.
async fn both_listeners_come_by_name(shape: Shape, name: &str) {
    let node = Node::new(shape, name).await;
    let (s3, s3_addr) = bound();
    let (oci, oci_addr) = bound();
    let fds = [oci.as_raw_fd(), s3.as_raw_fd()];
    let flags = node.flags(s3_addr, Some(oci_addr), true);
    let mut running = start(&flags, &Passing::named(&fds, "oci:s3"));
    assert_eq!(running.said("listen"), Some(s3_addr.to_string().as_str()));
    assert_eq!(
        running.said("oci-listen"),
        Some(oci_addr.to_string().as_str())
    );

    let put = call(s3_addr, "PUT", "cas/both", true, b"both").await;
    assert!(check(put, 200, None).is_ok(), "{shape:?}");
    let registry = b"GET /v2/ HTTP/1.1\r\nhost: registry\r\nconnection: close\r\n\r\n";
    let mut stream = TcpStream::connect(oci_addr).await.unwrap();
    stream.write_all(registry).await.unwrap();
    let mut said = String::new();
    stream.read_to_string(&mut said).await.unwrap();
    assert!(said.starts_with("HTTP/1.1 200"), "{shape:?}: {said}");
    assert_eq!(running.stop().await, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_node_takes_its_s3_and_oci_sockets_by_name() {
    both_listeners_come_by_name(Shape::Storage, "act-both-storage").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cache_node_takes_its_s3_and_oci_sockets_by_name() {
    both_listeners_come_by_name(Shape::Cache, "act-both-cache").await;
}

/// Asserts that a start with `--listen-from-systemd` is refused with status 2, the reason in the
/// message and nothing on stdout, for each way the service manager's hand-over can be wrong, in
/// both shapes (the cache node says the keys of its file): no `LISTEN_PID`, none of the other
/// variables, a `LISTEN_PID` of another process, a count that is not the number of names, a name
/// that is not the one asked for, a missing second socket, an extra socket, a name twice, and
/// sockets that are not what they are named for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_from_the_service_manager_is_refused_when_the_handover_is_wrong() {
    for shape in [Shape::Storage, Shape::Cache] {
        let node = Node::new(shape, &format!("act-refused-{shape:?}")).await;
        let (socket, addr) = bound();
        let (other, other_addr) = bound();
        let fds = [socket.as_raw_fd()];
        let flags = node.flags(addr, None, true);
        let with = |passing: Passing<'_>| run_passing(&flags, &passing);

        let nothing = plain();
        assert_refused(&with(nothing), "LISTEN_PID is not set");
        let no_count = Passing {
            sockets: &fds,
            pid: Pid::Own,
            count: None,
            names: Some("s3"),
        };
        assert_refused(&with(no_count), "LISTEN_FDS is not set");
        let no_names = Passing {
            sockets: &fds,
            pid: Pid::Own,
            count: Some(""),
            names: None,
        };
        assert_refused(&with(no_names), "LISTEN_FDNAMES is not set");
        let not_ours = Passing {
            sockets: &fds,
            pid: Pid::Other(1),
            count: Some(""),
            names: Some("s3"),
        };
        assert_refused(&with(not_ours), "LISTEN_PID is \"1\"");
        let garbled = Passing {
            sockets: &fds,
            pid: Pid::Own,
            count: Some(""),
            names: Some("s3"),
        };
        let garbled = Passing {
            count: Some("one"),
            ..garbled
        };
        assert_refused(&with(garbled), "LISTEN_FDS is \"one\"");
        let two = [socket.as_raw_fd(), other.as_raw_fd()];
        assert_refused(&with(Passing::named(&two, "s3:extra")), "needs 1");
        let wrong_name = Passing::named(&fds, "web");
        assert_refused(&with(wrong_name), "\"s3\"");
        let twice = [socket.as_raw_fd(), other.as_raw_fd()];
        assert_refused(&with(Passing::named(&twice, "s3:s3")), "needs 1");
        assert_refused(&with(Passing::named(&[], "")), "LISTEN_FDS is");

        // The socket is another address than the one the node serves.
        let elsewhere = [other.as_raw_fd()];
        let refused = with(Passing::named(&elsewhere, "s3"));
        assert_refused(&refused, &other_addr.to_string());
        assert_refused(&refused, &addr.to_string());
    }
}

/// Asserts that a descriptor that is not a listening TCP socket is refused with status 2 and a
/// message that says what it is: a UDP socket, a TCP socket that is bound but does not listen, a
/// connected TCP socket, a listening unix socket, a file, and a descriptor that is not open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_descriptor_that_is_not_a_listening_tcp_socket_is_refused() {
    let node = Node::new(Shape::Storage, "act-kinds").await;
    let (listener, addr) = bound();
    let flags = node.flags(addr, None, true);
    let run = |fd: RawFd| run_passing(&flags, &Passing::named(&[fd], "s3"));

    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    assert_refused(&run(udp.as_raw_fd()), "not a stream socket");
    let connected = std::net::TcpStream::connect(addr).unwrap();
    assert_refused(&run(connected.as_raw_fd()), "not a listening socket");
    let path = node.files.key.with_file_name("unix.sock");
    let unix = UnixListener::bind(&path).unwrap();
    assert_refused(&run(unix.as_raw_fd()), "not a TCP socket");
    let file = std::fs::File::open(Path::new(&node.files.key)).unwrap();
    assert_refused(&run(file.as_raw_fd()), "not a socket");
    // A descriptor number nothing holds in the child: the highest one the test passes is 3.
    let refused = run_passing(
        &flags,
        &Passing {
            sockets: &[],
            pid: Pid::Own,
            count: Some("1"),
            names: Some("s3"),
        },
    );
    assert_refused(&refused, "descriptor 3");
    drop(listener);
}

/// Asserts that a socket the manager passed is made non-blocking by the process and that the
/// process stops accepting on it without closing the socket for the manager: after the drain a
/// connection made to the manager's socket is held (it is not refused and not reset), and the
/// socket is still listening for the next process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_outlives_the_drain_of_the_process_that_was_given_it() {
    let node = Node::new(Shape::Storage, "act-outlives").await;
    let (socket, addr) = bound();
    let flags = node.flags(addr, None, true);
    let mut running = start(&flags, &Passing::named(&[socket.as_raw_fd()], "s3"));
    assert_eq!(running.stop().await, Some(0));
    let held = std::net::TcpStream::connect(addr);
    assert!(held.is_ok(), "{held:?}");
    // The connection waits in the queue of the socket; it was not reset.
    let mut held = held.unwrap();
    held.set_nonblocking(true).unwrap();
    let mut byte = [0_u8; 1];
    let read = held.read(&mut byte);
    assert_eq!(read.unwrap_err().kind(), ErrorKind::WouldBlock);
    held.write_all(b"x").unwrap();
}

/// Asserts the flag and the key in step: `--print-config` writes `listen_from_systemd = true`
/// for the flag, a file with the key loads (`--check-config` passes) and a file with it printed
/// back is the file that was printed, the flag given with `--config` is refused as a data-path
/// flag is, and a start from a file with the key refuses a wrong hand-over in the words of the
/// key.
#[test]
fn the_flag_and_the_key_are_one_setting() {
    let files = Files::new("act-config");
    let flags = files.flags(&["--listen-from-systemd", "--print-config"]);
    let (code, printed, err) = crate::binary::run(&flags);
    assert_eq!(code, 0, "{err}");
    assert!(printed.contains("listen_from_systemd = true"), "{printed}");
    let file = files.secret("printed.toml", &printed).display().to_string();
    let check = ["--config", file.as_str(), "--check-config"].map(str::to_owned);
    assert_eq!(
        crate::binary::run(&check),
        (0, String::new(), String::new())
    );

    let without = files.flags(&["--print-config"]);
    let (_, printed, _) = crate::binary::run(&without);
    assert!(!printed.contains("listen_from_systemd"), "{printed}");

    let both = ["--config", file.as_str(), "--listen-from-systemd"].map(str::to_owned);
    let (code, out, err) = crate::binary::run(&both);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--listen-from-systemd"), "{err}");

    let (code, out, err) = crate::binary::run(&["--config".to_owned(), file]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("server.listen_from_systemd"), "{err}");
    assert!(!err.contains("--listen-from-systemd"), "{err}");
}

/// What a keep-alive connection reads next.
enum Next {
    /// The server closed the connection (or reset it) without another answer.
    Closed,
    Answer(Answer),
}

/// Reads one answer from a connection that stays open: the head, then as many bytes of body as
/// `content-length` says.
async fn read_one(stream: &mut TcpStream) -> Next {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let split = bytes.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(split) = split {
            let head = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: ").map(str::to_owned))
                .map_or(0, |text| text.trim().parse::<usize>().unwrap());
            if bytes.len() >= split + 4 + length {
                return Next::Answer(parse(&bytes).unwrap());
            }
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Next::Closed,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Asserts that a drain held open by a request in flight does not leave keep-alive connections
/// open or answer them 503, and that the next process serves what the first left.
///
/// - Process 1 has a connection `a` that has served one request and is idle (keep-alive), and a
///   put `b` whose body has not all arrived, which holds the drain open. That `b` is being handled
///   is known from the ids: the server numbers a request when it begins it, so a probe on a fresh
///   connection that gets an id two past the last one means `b` took the one between.
/// - SIGTERM. The test sends requests on `a` until the connection ends: an answer that says
///   `Connection: close`, or the connection closed. Any answer on it that is 503 fails the test (a
///   server that refuses the connection's requests for the drain answers 503 here, from the moment
///   it stops accepting until `b` is done, however long that is).
/// - `b` is completed and answered 200, process 1 exits 0, and process 2, started on the same
///   socket, serves a fresh connection as its first request and has what `b` wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_held_open_by_a_request_ends_keep_alive_connections_without_a_503() {
    let node = Node::new(Shape::Storage, "act-keepalive").await;
    let (socket, addr) = bound();
    let fds = [socket.as_raw_fd()];
    let mut flags = node.flags(addr, None, true);
    flags.extend(["--body-idle-timeout".to_owned(), "1h".to_owned()]);
    let mut first = start(&flags, &Passing::named(&fds, "s3"));
    let seed = call(addr, "PUT", "cas/seed", true, b"seed").await;
    assert!(check(seed, 200, None).is_ok());

    let mut a = TcpStream::connect(addr).await.unwrap();
    let get = build(addr, "GET", "cas/seed", false, b"", false);
    a.write_all(&get).await.unwrap();
    let Next::Answer(served) = read_one(&mut a).await else {
        panic!("the connection closed before its first answer");
    };
    assert_eq!((served.status, served.close), (200, false));

    let held = vec![b'h'; 1000];
    let put = build(addr, "PUT", "cas/held", true, &held, false);
    let head = put.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let mut b = TcpStream::connect(addr).await.unwrap();
    b.write_all(&put[..head + 10]).await.unwrap();
    let mut last = served.id;
    loop {
        let probe = call(addr, "GET", "cas/seed", false, b"").await.unwrap();
        if probe.id != last + 1 {
            break;
        }
        last = probe.id;
    }

    first.term();
    loop {
        if a.write_all(&get).await.is_err() {
            break;
        }
        match read_one(&mut a).await {
            Next::Closed => break,
            Next::Answer(answer) => {
                let said = String::from_utf8_lossy(&answer.body).into_owned();
                assert_eq!(
                    answer.status, 200,
                    "an answer to a keep-alive connection: {said}"
                );
                if answer.close {
                    break;
                }
            }
        }
    }

    b.write_all(&put[head + 10..]).await.unwrap();
    let Next::Answer(done) = read_one(&mut b).await else {
        panic!("the put in flight was cut");
    };
    assert_eq!(done.status, 200);
    assert_eq!(first.wait().await, Some(0));

    let mut second = start(&flags, &Passing::named(&fds, "s3"));
    let seed = call(addr, "GET", "cas/seed", false, b"").await.unwrap();
    assert_eq!((seed.status, seed.id), (200, 1));
    let kept = call(addr, "GET", "cas/held", false, b"").await.unwrap();
    assert_eq!((kept.status, kept.body), (200, held));
    assert_eq!(second.stop().await, Some(0));
}
