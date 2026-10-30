//! The listeners for jobs: what the file may say of them, and what a start does with them.

use std::fs::Permissions;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::Request;
use kbf_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::oneshot;

use super::{Bound, Duplex, JobListeners, Listeners, UNIX_PEER, group_id};
use crate::server::auth::sign;
use crate::server::cache_cli::serve_cache_bound;
use crate::server::cli::{Error, exit_status};
use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_cache::CacheSettings;
use crate::server::local_cache::testing::pattern;
use crate::server::local_cache::{Disk, SystemDisk};
use crate::server::{Drained, Incoming, ServerConfig};
use crate::{Backend, FaultyBackend, MemoryBackend, Op, SystemClock};

const BUCKET: &str = "tier-bucket";
const KEY_ID: &str = "AKIDCACHE";
const SECRET: &str = "cache-secret";

/// The knobs of a run whose drain does not wait.
const QUICK: PerRun = PerRun {
    drain_lb_wait: Duration::from_millis(50),
    drain_deadline: Duration::from_secs(60),
};

/// A scratch directory with a key file in it.
struct Node {
    dir: TestDir,
    key: PathBuf,
}

impl Node {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let key = dir.path().join("key");
        std::fs::write(&key, format!("{KEY_ID}\n{SECRET}\n")).unwrap();
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
        Self { dir, key }
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("jobs.sock")
    }

    /// The file of a build node with this extra text in `[server]`.
    fn text(&self, server: &str) -> String {
        format!(
            r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "c1"
bucket          = "{BUCKET}"
access_key_file = "{key}"
{server}

[local_cache]
path   = "{cache}"
budget = "1MiB"

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#,
            key = self.key.display(),
            cache = self.dir.path().join("cache").display(),
        )
    }

    fn settings(&self, server: &str) -> CacheSettings {
        let text = self.text(server);
        settings_from_text(&text, QUICK)
            .unwrap()
            .into_cache()
            .unwrap()
    }

    /// A build node that listens on the bridge (here a loopback port) and on `path`.
    fn at(&self, path: &Path) -> CacheSettings {
        let server = format!(
            "bridge_listen = \"127.0.0.1:0\"\nlisten_unix = \"{}\"",
            path.display()
        );
        self.settings(&server)
    }

    /// A build node that listens on the bridge and on the socket of the node.
    fn listening(&self) -> CacheSettings {
        self.at(&self.socket())
    }
}

/// The answer to a request: its status and body, with the id of the request cut out of the body
/// of an error, which differs from one request to the next.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    status: u16,
    body: Vec<u8>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn without_request_id(mut body: Vec<u8>) -> Vec<u8> {
    let end = b"</RequestId>";
    if let (Some(from), Some(to)) = (find(&body, b"<RequestId>"), find(&body, end)) {
        body.drain(from..to + end.len());
    }
    body
}

/// A connection to a listener, over which requests are sent as they would be on the wire.
struct Client {
    stream: Box<dyn Duplex>,
}

async fn tcp(address: SocketAddr) -> Client {
    let stream = TcpStream::connect(address).await.unwrap();
    Client {
        stream: Box::new(stream),
    }
}

async fn unix(path: &Path) -> Client {
    let stream = UnixStream::connect(path).await.unwrap();
    Client {
        stream: Box::new(stream),
    }
}

impl Client {
    /// Sends the request, with the headers it was signed with, and reads the answer by its
    /// `Content-Length`.
    async fn call(&mut self, request: Request<Full<Bytes>>) -> Reply {
        let (parts, _) = request.into_parts();
        let mut head = format!("{} {} HTTP/1.1\r\n", parts.method, parts.uri);
        for (name, value) in &parts.headers {
            head.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
        }
        head.push_str("content-length: 0\r\n\r\n");
        self.stream.write_all(head.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let end = loop {
            if let Some(at) = find(&buf, b"\r\n\r\n") {
                break at + 4;
            }
            self.more(&mut buf).await;
        };
        let head = String::from_utf8(buf[..end].to_vec()).unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        let length = head
            .lines()
            .find_map(|line| {
                let line = line.to_ascii_lowercase();
                let value = line.strip_prefix("content-length:")?;
                Some(value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while buf.len() < end + length {
            self.more(&mut buf).await;
        }
        let body = without_request_id(buf[end..end + length].to_vec());
        Reply { status, body }
    }

    async fn more(&mut self, buf: &mut Vec<u8>) {
        let mut chunk = [0u8; 4096];
        let taken = self.stream.read(&mut chunk).await.unwrap();
        assert!(
            taken > 0,
            "the connection closed before the answer was whole"
        );
        buf.extend_from_slice(&chunk[..taken]);
    }

    /// Whether the other end has closed the connection.
    async fn closed(&mut self) -> bool {
        self.stream.read(&mut [0u8; 1]).await.is_ok_and(|n| n == 0)
    }
}

/// The settings of a client that signs as the full key of the node.
fn signing() -> ServerConfig {
    ServerConfig::new(BUCKET, KEY_ID, SECRET, Arc::new(SystemClock))
}

fn get(uri: &str, extra: &[(&str, &str)]) -> Request<Full<Bytes>> {
    sign(&signing(), "GET", uri, extra, b"")
}

/// A request that carries no signature.
fn unsigned(uri: &str) -> Request<Full<Bytes>> {
    let request = Request::builder().uri(uri).header("host", "tier.test");
    request.body(Full::new(Bytes::new())).unwrap()
}

/// The same request on each of three connections: the main listener's, the bridge's and the
/// socket's. Asserts that the answers are equal, and gives that answer.
async fn alike(clients: &mut [Client; 3], make: impl Fn() -> Request<Full<Bytes>>) -> Reply {
    let mut replies = Vec::new();
    for client in clients.iter_mut() {
        replies.push(client.call(make()).await);
    }
    assert_eq!(
        replies[0], replies[1],
        "the bridge answers as the main does"
    );
    assert_eq!(
        replies[0], replies[2],
        "the socket answers as the main does"
    );
    replies.swap_remove(0)
}

/// Runs `serve_cache_bound`, calls `work` with where it listens, then stops it. A start that
/// fails never calls `work`.
async fn serving<F, Fut, T>(
    settings: &CacheSettings,
    upstream: Arc<dyn Backend>,
    work: F,
) -> (Result<Drained, Error>, Option<T>)
where
    F: FnOnce(Bound) -> Fut,
    Fut: Future<Output = T>,
{
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Bound>();
    let running = serve_cache_bound(
        settings,
        upstream,
        disk,
        async move {
            let _ = stopped.await;
        },
        move |bound: &Bound| {
            let _ = told.send(bound.clone());
        },
    );
    let driving = async {
        let out = match bound.await {
            Ok(bound) => Some(work(bound).await),
            Err(_) => None,
        };
        let _ = stop.send(());
        out
    };
    tokio::join!(running, driving)
}

fn empty_upstream() -> Arc<dyn Backend> {
    Arc::new(MemoryBackend::new())
}

/// Asserts that a request over the bridge and over the socket is answered as one to the main
/// listener is: the same status and body for a whole read of an object, a range of it, a write
/// (refused, as a read cache refuses it), a request with no signature, and `/readyz`; and that
/// the three are one cache: the object is read from the upstream once, by the first, and the
/// others hit. The client of the socket signs with a `Host` that is nothing of the socket, and it
/// is accepted. A listener that served another cache, other keys or another front would answer
/// differently here.
#[tokio::test]
async fn the_bridge_and_the_socket_serve_what_the_main_listener_serves() {
    let node = Node::new("tsl-alike");
    let settings = node.listening();
    let memory = Arc::new(MemoryBackend::new());
    let body = pattern(1000);
    memory
        .put("cas/obj", Bytes::from(body.clone()))
        .await
        .unwrap();
    let upstream = Arc::new(FaultyBackend::new(memory));
    let seen = Arc::clone(&upstream);
    let work = |bound: Bound| async move {
        let mut clients = [
            tcp(bound.main).await,
            tcp(bound.bridge.unwrap()).await,
            unix(&bound.unix.unwrap()).await,
        ];
        let uri = format!("/{BUCKET}/cas/obj");

        let [main, bridge, socket] = &mut clients;
        let filled = main.call(get(&uri, &[])).await;
        assert_eq!((filled.status, &filled.body), (200, &body));
        let reads = seen.calls(Op::Get);
        assert!(reads >= 1);
        for client in [bridge, socket] {
            assert_eq!(client.call(get(&uri, &[])).await, filled);
        }
        assert_eq!(seen.calls(Op::Get), reads, "the others hit the cache");
        let metrics = unix_metrics(&mut clients[2]).await;
        assert!(
            metrics.contains("tierstore_cache_hits_total 2\n"),
            "{metrics}"
        );
        assert!(
            metrics.contains("tierstore_cache_fills_total 1\n"),
            "{metrics}"
        );

        let range = [("range", "bytes=10-19")];
        let part = alike(&mut clients, || get(&uri, &range)).await;
        assert_eq!(part.body, body[10..20]);

        let put = || sign(&signing(), "PUT", &uri, &[], b"x");
        let refused = alike(&mut clients, put).await;
        assert_eq!(refused.status, 403);
        let text = String::from_utf8(refused.body).unwrap();
        assert!(text.contains("AccessDenied"), "{text}");

        let anonymous = alike(&mut clients, || unsigned(&uri)).await;
        assert_eq!(anonymous.status, 403);

        let ready = alike(&mut clients, || unsigned("/readyz")).await;
        assert_eq!((ready.status, ready.body), (200, b"ready".to_vec()));
    };
    let (drained, done) = serving(&settings, upstream, work).await;
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    assert!(done.is_some());
}

/// The text of `/metricz` over a connection.
async fn unix_metrics(client: &mut Client) -> String {
    let reply = client.call(unsigned("/metricz")).await;
    assert_eq!(reply.status, 200);
    String::from_utf8(reply.body).unwrap()
}

/// Asserts the socket file while the server runs and after it: a socket, with mode 0660 whatever
/// the umask of the process (the mode is set before the socket listens), and gone once the
/// server has drained. The bridge and the main address are in what the start reports, and the
/// socket's path.
#[tokio::test]
async fn the_socket_is_mode_0660_and_removed_when_the_server_stops() {
    let node = Node::new("tsl-mode");
    let path = node.socket();
    let settings = node.at(&path);
    let work = |bound: Bound| async move {
        let meta = std::fs::symlink_metadata(bound.unix.as_ref().unwrap()).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.mode() & 0o777, 0o660);
        assert!(bound.bridge.unwrap().ip().is_loopback());
        assert_ne!(bound.bridge.unwrap(), bound.main);
        bound
    };
    let (drained, bound) = serving(&settings, empty_upstream(), work).await;
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    assert_eq!(bound.unwrap().unix, Some(path.clone()));
    assert!(
        std::fs::symlink_metadata(&path).is_err(),
        "the socket is removed"
    );
}

/// Asserts that a stale socket (the file a process left when it did not clean up) is replaced and
/// the new one is served, and that the node starts with only the extra listeners it is told:
/// with none, `/healthz` is served on the main address and nothing else is made.
#[tokio::test]
async fn a_socket_left_by_another_process_is_replaced() {
    let node = Node::new("tsl-stale");
    let path = node.socket();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    let settings = node.at(&path);
    let work = |_bound: Bound| async move {
        let mut client = unix(&path).await;
        let reply = client.call(unsigned("/healthz")).await;
        assert_eq!((reply.status, reply.body), (200, b"ok".to_vec()));
    };
    let (drained, done) = serving(&settings, empty_upstream(), work).await;
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    assert!(done.is_some());

    let plain = node.settings("");
    let work = |bound: Bound| async move { (bound.bridge, bound.unix) };
    let (drained, bound) = serving(&plain, empty_upstream(), work).await;
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    assert_eq!(bound, Some((None, None)));
}

/// Asserts that a path which is not a socket is never replaced, whatever it is (a regular file, a
/// directory, a link to a socket), and that the start fails with exit status 1 and says why, with
/// the thing left as it was; and the other ways the socket cannot be made: a directory that is not
/// there, a parent that is a file, a path too long for a socket.
#[tokio::test]
async fn a_path_that_is_not_a_socket_is_never_replaced() {
    let node = Node::new("tsl-refuse");
    let dir = node.dir.path();

    let file = dir.join("a-file");
    std::fs::write(&file, b"precious").unwrap();
    let (result, started) = serving(&node.at(&file), empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err();
    assert!(started.is_none());
    assert!(why.to_string().contains("is not a socket"), "{why}");
    assert!(why.to_string().contains("a-file"), "{why}");
    assert_eq!(exit_status(&why), 1);
    assert_eq!(std::fs::read(&file).unwrap(), b"precious");

    let folder = dir.join("a-dir");
    std::fs::create_dir(&folder).unwrap();
    let (result, _) = serving(&node.at(&folder), empty_upstream(), |_| async {}).await;
    assert!(result.unwrap_err().to_string().contains("is not a socket"));
    assert!(folder.is_dir());

    let real = dir.join("real.sock");
    drop(std::os::unix::net::UnixListener::bind(&real).unwrap());
    let link = dir.join("link.sock");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (result, _) = serving(&node.at(&link), empty_upstream(), |_| async {}).await;
    assert!(result.unwrap_err().to_string().contains("is not a socket"));
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(&real)
            .unwrap()
            .file_type()
            .is_socket()
    );

    let missing = dir.join("missing").join("x.sock");
    let (result, _) = serving(&node.at(&missing), empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err().to_string();
    assert!(why.contains("listen_unix") && why.contains("bind"), "{why}");

    let beneath = file.join("x.sock");
    let (result, _) = serving(&node.at(&beneath), empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err().to_string();
    assert!(why.contains(&beneath.display().to_string()), "{why}");

    let long = dir.join("x".repeat(120));
    let (result, _) = serving(&node.at(&long), empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err().to_string();
    assert!(why.contains("address"), "{why}");
}

/// Asserts that a bridge address that is taken fails the start, naming `bridge_listen`.
#[tokio::test]
async fn a_bridge_address_that_is_taken_fails_the_start() {
    let node = Node::new("tsl-taken");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap();
    let server = format!("bridge_listen = \"{address}\"");
    let settings = node.settings(&server);
    let (result, started) = serving(&settings, empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err();
    assert!(started.is_none());
    let want = format!("binding bridge_listen {address}");
    assert!(why.to_string().starts_with(&want), "{why}");
    assert_eq!(exit_status(&why), 1);
}

/// The group file of a test, with a group of this name and id.
fn group_file(node: &Node, name: &str, gid: u32) -> PathBuf {
    let file = node.dir.path().join("group");
    std::fs::write(&file, format!("{name}:x:{gid}:\n")).unwrap();
    file
}

/// A node that listens on its socket with this group, looked up in `file`.
fn grouped(node: &Node, group: &str, file: Option<&Path>) -> CacheSettings {
    let server = format!(
        "listen_unix = \"{}\"\nlisten_unix_group = \"{group}\"",
        node.socket().display()
    );
    let mut settings = node.settings(&server);
    if let Some(file) = file {
        settings.front.jobs.unix.as_mut().unwrap().group_file = file.to_owned();
    }
    settings
}

/// Asserts the group of the socket: a group the system does not have fails the start, with exit
/// status 1, and no socket is left; a group of the process (its own, found in a group file of the
/// test) is the group of the file; and a group the process is not in is refused when it is not
/// root, since only root may give a file to any group, and that socket is not left either.
#[tokio::test]
async fn the_group_of_the_socket_is_the_one_named() {
    let node = Node::new("tsl-group");
    let path = node.socket();

    let own = rustix::process::getegid().as_raw();
    let file = group_file(&node, "builders", own);
    let unknown = grouped(&node, "no-such-group", Some(&file));
    let (result, started) = serving(&unknown, empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err();
    assert!(started.is_none());
    assert!(why.to_string().contains("is not a group in"), "{why}");
    assert!(why.to_string().contains("no-such-group"), "{why}");
    assert_eq!(exit_status(&why), 1);
    assert!(std::fs::symlink_metadata(&path).is_err());

    let settings = grouped(&node, "builders", Some(&file));
    let work = |bound: Bound| async move {
        let meta = std::fs::symlink_metadata(bound.unix.unwrap()).unwrap();
        assert_eq!(meta.gid(), own);
    };
    let (drained, done) = serving(&settings, empty_upstream(), work).await;
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    assert!(done.is_some());

    if rustix::process::geteuid().is_root() {
        return;
    }
    let mine: Vec<u32> = rustix::process::getgroups()
        .unwrap()
        .iter()
        .map(|gid| gid.as_raw())
        .collect();
    let foreign = (60_000..)
        .find(|gid| !mine.contains(gid) && *gid != own)
        .unwrap();
    let file = group_file(&node, "outsiders", foreign);
    let settings = grouped(&node, "outsiders", Some(&file));
    let (result, started) = serving(&settings, empty_upstream(), |_| async {}).await;
    let why = result.unwrap_err();
    assert!(started.is_none());
    assert!(why.to_string().contains("changing the group"), "{why}");
    assert!(
        std::fs::symlink_metadata(&path).is_err(),
        "the socket is removed"
    );
}

/// Asserts how a group is looked up: by name, in the format of `/etc/group`, skipping lines that
/// are not entries, and refusing a name that is absent, an id that is not a number, the id that
/// means "no change" to `chown`, and a file that cannot be read.
#[test]
fn a_group_is_found_by_name_in_the_group_file() {
    let dir = TestDir::under(std::env::temp_dir(), "tsl-group-file");
    let file = dir.path().join("group");
    let text = "# a comment\nbroken\nbuilders:x:1234:alice,bob\nbad:x:many:\nall:x:4294967295:\n";
    std::fs::write(&file, text).unwrap();
    assert_eq!(group_id("builders", &file), Ok(1234));
    for name in ["broken", "bad", "all", "absent", "x"] {
        let why = group_id(name, &file).unwrap_err();
        assert!(why.contains("is not a group in"), "{name}: {why}");
    }
    let why = group_id("builders", &dir.path().join("nothing")).unwrap_err();
    assert!(why.starts_with("reading "), "{why}");
}

/// Asserts that the drain closes the connections of every listener: three connections that have
/// each been served a request and wait for the next are closed by the server when it stops, and
/// nothing is cut. A listener whose connections were not counted by the server would be left open.
#[tokio::test]
async fn the_drain_closes_the_connections_of_every_listener() {
    let node = Node::new("tsl-drain");
    let settings = node.listening();
    let work = |bound: Bound| async move {
        let mut clients = [
            tcp(bound.main).await,
            tcp(bound.bridge.unwrap()).await,
            unix(&bound.unix.unwrap()).await,
        ];
        let ready = alike(&mut clients, || unsigned("/readyz")).await;
        assert_eq!(ready.status, 200);
        clients
    };
    let (drained, clients) = serving(&settings, empty_upstream(), work).await;
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    for mut client in clients.unwrap() {
        assert!(client.closed().await);
    }
}

/// Asserts that the listeners are taken in turn: with two connections waiting on the main
/// listener and one on the bridge, the bridge's is taken second, not last; and that a connection
/// over the socket is given the peer address that stands for one with none.
#[tokio::test]
async fn a_busy_listener_does_not_starve_the_others() {
    let node = Node::new("tsl-turn");
    let path = node.socket();
    let any = "127.0.0.1:0".parse().unwrap();
    let jobs = JobListeners::new(any, Some(any), Some(path.clone()), None).unwrap();
    let mut listeners = Listeners::bind(any, &jobs).await.unwrap();
    let bound = listeners.bound().clone();

    let first = TcpStream::connect(bound.main).await.unwrap();
    let second = TcpStream::connect(bound.main).await.unwrap();
    let bridge = TcpStream::connect(bound.bridge.unwrap()).await.unwrap();
    let socket = UnixStream::connect(&path).await.unwrap();
    let mut peers = Vec::new();
    for _ in 0..4 {
        let (_, peer) = std::future::poll_fn(|cx| listeners.poll_incoming(cx))
            .await
            .unwrap();
        peers.push(peer);
    }
    let local = |stream: &TcpStream| stream.local_addr().unwrap();
    let want = [local(&first), local(&bridge), UNIX_PEER, local(&second)];
    assert_eq!(peers, want);
    drop(socket);
}

/// Asserts that the socket file is removed only if it is the one that was made: a file that has
/// taken its place (a process that started since) is left, and a file that is gone is not an
/// error.
#[tokio::test]
async fn a_file_that_replaced_the_socket_is_left_alone() {
    let node = Node::new("tsl-replaced");
    let path = node.socket();
    let any = "127.0.0.1:0".parse().unwrap();
    let jobs = JobListeners::new(any, None, Some(path.clone()), None).unwrap();

    let listeners = Listeners::bind(any, &jobs).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"another").unwrap();
    drop(listeners);
    assert_eq!(std::fs::read(&path).unwrap(), b"another");
    std::fs::remove_file(&path).unwrap();

    let listeners = Listeners::bind(any, &jobs).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    drop(listeners);
    assert!(std::fs::symlink_metadata(&path).is_err());

    let listeners = Listeners::bind(any, &jobs).await.unwrap();
    drop(listeners);
    assert!(std::fs::symlink_metadata(&path).is_err());
}

/// Asserts what the start line adds for the listeners: nothing for none, the bridge, the socket,
/// both in that order, each after a space.
#[test]
fn the_start_line_names_the_listeners_that_are_set() {
    let main = "192.0.2.1:9400".parse().unwrap();
    let bridge = "192.0.2.7:9400".parse().unwrap();
    let path = PathBuf::from("/run/tierstore-cache/jobs.sock");
    let only = |bridge, path: Option<&PathBuf>| {
        JobListeners::new(main, bridge, path.cloned(), None)
            .unwrap()
            .announce()
    };
    assert_eq!(only(None, None), "");
    assert_eq!(only(Some(bridge), None), " bridge=192.0.2.7:9400");
    assert_eq!(
        only(None, Some(&path)),
        " unix=/run/tierstore-cache/jobs.sock"
    );
    assert_eq!(
        only(Some(bridge), Some(&path)),
        " bridge=192.0.2.7:9400 unix=/run/tierstore-cache/jobs.sock"
    );
}

/// The error of a file whose `[server]` has this extra text, or the settings it makes.
fn refused(node: &Node, server: &str) -> String {
    let text = node.text(server);
    settings_from_text(&text, QUICK).unwrap_err().to_string()
}

/// Asserts what the file refuses of the three keys: a wildcard bridge address (v4 and v6), the
/// address of `listen` (but not two ports left to be chosen), a socket path that is relative or is
/// not a file's, an empty group, and a group with no socket; each by the key's name. A bridge and a
/// socket that are well formed are read as they are written.
#[test]
fn the_file_refuses_a_listener_it_cannot_serve() {
    let node = Node::new("tsl-config");
    for wildcard in ["0.0.0.0:9400", "[::]:9400"] {
        let why = refused(&node, &format!("bridge_listen = \"{wildcard}\""));
        assert!(why.contains("server.bridge_listen"), "{why}");
        assert!(why.contains("0.0.0.0"), "{why}");
    }
    let text = node.text("bridge_listen = \"127.0.0.1:9400\"");
    let same = text.replace("127.0.0.1:0", "127.0.0.1:9400");
    let why = settings_from_text(&same, QUICK).unwrap_err().to_string();
    assert!(why.contains("server.bridge_listen"), "{why}");
    assert!(why.contains("address of listen"), "{why}");
    let ports = node.text("bridge_listen = \"127.0.0.1:0\"");
    assert!(settings_from_text(&ports, QUICK).is_ok());

    for path in ["jobs.sock", "/", ""] {
        let why = refused(&node, &format!("listen_unix = \"{path}\""));
        assert!(why.contains("server.listen_unix"), "{path}: {why}");
        assert!(
            why.contains("absolute path of a socket file"),
            "{path}: {why}"
        );
    }
    let why = refused(
        &node,
        "listen_unix = \"/run/x.sock\"\nlisten_unix_group = \"\"",
    );
    assert!(why.contains("server.listen_unix_group"), "{why}");
    let why = refused(&node, "listen_unix_group = \"builders\"");
    assert!(why.contains("server.listen_unix_group"), "{why}");
    assert!(why.contains("listen_unix"), "{why}");
    let why = refused(&node, "bridge_listen = \"not an address\"");
    assert!(why.contains("bridge_listen"), "{why}");

    let server = "bridge_listen = \"192.0.2.7:9400\"\nlisten_unix = \"/run/x.sock\"\nlisten_unix_group = \"builders\"";
    let jobs = node.settings(server).front.jobs;
    assert_eq!(jobs.bridge, Some("192.0.2.7:9400".parse().unwrap()));
    let socket = jobs.unix.unwrap();
    assert_eq!(socket.path, PathBuf::from("/run/x.sock"));
    assert_eq!(socket.group.as_deref(), Some("builders"));
    assert_eq!(socket.group_file, PathBuf::from("/etc/group"));
    assert_eq!(node.settings("").front.jobs, JobListeners::default());
}

/// Asserts that a storage node refuses each of the three keys, by name: a listener for jobs is
/// the build node's.
#[test]
fn a_storage_node_refuses_the_listeners_for_jobs() {
    let node = Node::new("tsl-storage");
    for line in [
        "bridge_listen = \"127.0.0.1:9401\"",
        "listen_unix = \"/run/x.sock\"",
        "listen_unix_group = \"builders\"",
    ] {
        let text = format!(
            r#"
[server]
listen          = "127.0.0.1:9400"
proxy_id        = "s1"
bucket          = "tier-bucket"
access_key_file = "{key}"
{line}

[hot_store]
kind   = "memory"
budget = "1MiB"

[cold_store]
kind = "memory"
"#,
            key = node.key.display()
        );
        let why = settings_from_text(&text, QUICK).unwrap_err().to_string();
        let key = line.split(' ').next().unwrap();
        assert!(why.contains(&format!("server.{key}")), "{line}: {why}");
        assert!(
            why.contains("only a build node has a listener for jobs"),
            "{why}"
        );
    }
}

/// Asserts that the README of the packaging documents the three keys, each in a row of a table,
/// and says what `Host` a client of the socket sends.
#[test]
fn the_readme_documents_the_listeners_for_jobs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("../../packaging/tierstore/README.md")).unwrap();
    for key in ["bridge_listen", "listen_unix", "listen_unix_group"] {
        let wanted = format!("`server.{key}`");
        let row = readme
            .lines()
            .find(|line| line.starts_with('|') && line.contains(&wanted));
        assert!(row.is_some(), "no row for {key}");
    }
    assert!(readme.contains("### Listeners for jobs"));
    assert!(readme.contains("`Host`"));
}
