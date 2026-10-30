//! The start of a build node: what it serves, and the refusals of its start.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{ByteRange, Capabilities, ObjectKey, ObjectStore};
use kbf_testkit::dir::TestDir;
use tokio::sync::oneshot;

use super::Drained;
use super::cache_cli::{
    run_cache, serve_cache, serve_cache_over, size_of_file_system, unjudged_warning,
};
use super::cli::Error;
use super::cli::exit_status;
use super::config::{PerRun, settings_from_text};
use super::config_cache::CacheSettings;
use super::endpoints::testing::Fleet;
use super::local_cache::testing::{FakeDisk, Quirk, digest, pattern};
use super::local_cache::{Disk, SystemDisk};
use crate::{Backend, FaultyBackend, MemoryBackend};

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

    fn cache_dir(&self) -> PathBuf {
        self.dir.path().join("cache")
    }

    /// The file of a build node with this extra text in `[server]`, and `local` in `[local_cache]`.
    fn text(&self, server: &str, local: &str) -> String {
        format!(
            r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "{key}"
{server}

[local_cache]
path   = "{cache}"
budget = "1MiB"
{local}

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#,
            key = self.key.display(),
            cache = self.cache_dir().display(),
        )
    }

    fn settings(&self, server: &str, local: &str) -> CacheSettings {
        let text = self.text(server, local);
        settings_from_text(&text, QUICK)
            .unwrap()
            .into_cache()
            .unwrap()
    }
}

/// A client of kbf for the server at `addr`.
fn client(addr: SocketAddr) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: "tier-bucket".to_owned(),
        credentials: Credentials::new(KEY_ID, SECRET),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).unwrap()
}

/// The text an unsigned GET of `path` is answered with, and its status.
async fn probe(addr: SocketAddr, path: &str) -> (u16, String) {
    let reply = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    (reply.status().as_u16(), reply.text().await.unwrap())
}

/// Runs `serve_cache` over this upstream and disk, calls `work` with the address, then stops it.
async fn serving<F, Fut>(
    settings: &CacheSettings,
    upstream: Arc<dyn Backend>,
    disk: Arc<dyn Disk>,
    work: F,
) -> Result<Drained, Error>
where
    F: FnOnce(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = serve_cache(
        settings,
        upstream,
        disk,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        // A start that fails never tells its address.
        if let Ok(bound) = addr.await {
            work(bound).await;
        }
        let _ = stop.send(());
    };
    let (drained, ()) = tokio::join!(running, driving);
    drained
}

/// Asserts that a build node serves what its upstream holds and reads it from the upstream once;
/// refuses a write with the upstream untouched; shows its counters on `/metricz`, and `/readyz`;
/// keeps the object in its directory; and drains with nothing cut.
#[tokio::test]
async fn a_build_node_serves_the_upstream_through_its_cache() {
    let node = Node::new("tscache-cli-serve");
    let settings = node.settings("", "scrub_rate = \"1MiB/s\"");
    let memory = Arc::new(MemoryBackend::new());
    let body = pattern(1000);
    memory
        .put("cas/obj", Bytes::from(body.clone()))
        .await
        .unwrap();
    let upstream = Arc::new(FaultyBackend::new(memory.clone()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let seen = Arc::clone(&upstream);
    let work = |addr: SocketAddr| async move {
        let store = client(addr);
        let key = ObjectKey::new("cas/obj").unwrap();
        let all = ByteRange::new(0, 1000).unwrap();
        assert_eq!(store.get_range(&key, all).await.unwrap(), body);
        assert_eq!(store.get_range(&key, all).await.unwrap(), body);
        assert_eq!(seen.calls(crate::Op::Get), 1);
        let new = ObjectKey::new("cas/new").unwrap();
        assert!(
            store
                .put_new(&new, Bytes::from_static(b"x"), None)
                .await
                .is_err()
        );
        assert_eq!(seen.calls(crate::Op::Put), 0);
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready"));
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        assert!(text.contains("tierstore_cache_hits_total 1\n"), "{text}");
        assert!(text.contains("tierstore_cache_fills_total 1\n"), "{text}");
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    let name = digest(b"cas/obj");
    let file = node
        .cache_dir()
        .join("obj")
        .join(&name[..2])
        .join(&name[2..4])
        .join(&name);
    assert_eq!(std::fs::read(file).unwrap(), pattern(1000));
}

/// Asserts that a build node over a list of endpoints serves what they hold, answers `/readyz`
/// 200 while one is ready and 503 once the poll says none is, shows their counters on `/metricz`,
/// and stops with the poll beside it (the drain waits for the poll to end, so a poll that
/// ignored the stop would hang the test).
#[tokio::test]
async fn a_build_node_over_endpoints_follows_their_readiness() {
    let node = Node::new("tscache-cli-endpoints");
    let settings = node.settings("", "");
    let fleet = Fleet::new(2);
    fleet.put("cas/obj", &pattern(1000)).await;
    let upstream: Arc<dyn Backend> = fleet.endpoints.clone();
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = serve_cache_over(
        &settings,
        upstream,
        Some(fleet.endpoints.clone()),
        disk,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let addr = addr.await.unwrap();
        let store = client(addr);
        let key = ObjectKey::new("cas/obj").unwrap();
        let all = ByteRange::new(0, 1000).unwrap();
        assert_eq!(store.get_range(&key, all).await.unwrap(), pattern(1000));
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready"));
        for name in &fleet.names {
            fleet.probe.set(name, false);
        }
        fleet.endpoints.poll().await;
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!(
            (status, text.as_str()),
            (503, "no upstream endpoint is ready")
        );
        let (_, text) = probe(addr, "/metricz").await;
        let gauge = format!("endpoint_ready{{endpoint=\"{}\"}} 0\n", fleet.names[0]);
        assert!(text.contains(&gauge), "{text}");
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
}

/// Asserts that an upstream that is a directory tier (no endpoints) and cannot be opened (its
/// path is under a file) is refused by `run_cache`, naming the tier, with exit status 1.
#[tokio::test]
async fn a_cold_directory_that_cannot_be_opened_is_refused() {
    let node = Node::new("tscache-cli-colddir");
    let file = node.dir.path().join("cold-in-the-way");
    std::fs::write(&file, b"a file").unwrap();
    let path = file.join("below");
    let dir = format!("kind = \"dir\"\npath = \"{}\"", path.display());
    let text = node.text("", "").replace("kind = \"memory\"", &dir);
    let settings = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let why = run_cache(&settings, std::future::ready(()), |_| {})
        .await
        .unwrap_err();
    assert!(why.to_string().contains("--cold"), "{why}");
    assert_eq!(exit_status(&why), 1);
}

/// Asserts that a list of endpoints one of which cannot be opened (the key file of the upstream
/// is not there) is refused by `run_cache`, naming the key file, with exit status 1, and that
/// nothing is served: the address is never told.
#[tokio::test]
async fn a_list_of_endpoints_that_cannot_be_opened_is_refused() {
    let node = Node::new("tscache-cli-listrefused");
    let missing = node.dir.path().join("no-such-credentials");
    let s3 = format!(
        "kind = \"s3\"\nbucket = \"b\"\nendpoints = [\"http://127.0.0.1:1\", \"http://127.0.0.1:2\"]\ncredentials_file = \"{}\"",
        missing.display()
    );
    let text = node.text("", "").replace("kind = \"memory\"", &s3);
    let settings = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    assert_eq!(settings.endpoints.len(), 2);
    let why = run_cache(&settings, std::future::ready(()), |_| {
        panic!("a start that is refused tells no address")
    })
    .await
    .unwrap_err();
    assert!(why.to_string().contains("credentials"), "{why}");
    assert_eq!(exit_status(&why), 1);
}

/// Asserts that `run_cache` makes the upstream of the file (a memory tier here, empty) and
/// serves it: a key that is not there is not found, and the start line is the cache's.
#[tokio::test]
async fn run_cache_opens_the_upstream_the_file_names() {
    let node = Node::new("tscache-cli-run");
    let settings = node.settings("", "");
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = run_cache(
        &settings,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let store = client(addr.await.unwrap());
        let key = ObjectKey::new("cas/none").unwrap();
        let first = ByteRange::new(0, 1).unwrap();
        assert!(store.get_range(&key, first).await.is_err());
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
}

/// Asserts the refusals of a start, each with the exit status of its reason: a request timeout of
/// zero, a bucket that is a probe path, an address that is taken and a key file for the upstream
/// that is not there (1 each), an upstream that does not answer (1, a restart may mend it), a
/// cache directory that cannot be opened (1), and a file system that fails the check of the
/// layout (3).
#[tokio::test]
async fn a_build_node_that_cannot_start_exits_with_the_status_of_the_reason() {
    let node = Node::new("tscache-cli-refused");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let nothing = |_addr: SocketAddr| async {};

    let zero = node.settings("s3_request_timeout = \"0s\"", "");
    let why = run_cache(&zero, std::future::ready(()), |_| {})
        .await
        .unwrap_err();
    assert!(why.to_string().contains("--s3-request-timeout"), "{why}");
    assert_eq!(exit_status(&why), 1);

    for name in ["healthz", "readyz", "usagez", "metricz"] {
        let text = node.text("", "").replace("tier-bucket", name);
        let settings = settings_from_text(&text, QUICK)
            .unwrap()
            .into_cache()
            .unwrap();
        let why = serving(&settings, upstream.clone(), disk.clone(), nothing)
            .await
            .unwrap_err();
        assert!(why.to_string().contains("probe path"), "{name}: {why}");
        assert_eq!(exit_status(&why), 1);
    }

    let down = Arc::new(FaultyBackend::new(upstream.clone()));
    down.set_down(true);
    let settings = node.settings("", "");
    let why = serving(&settings, down, disk.clone(), nothing)
        .await
        .unwrap_err();
    assert!(why.to_string().contains("did not answer a HEAD"), "{why}");
    assert_eq!(exit_status(&why), 1);

    let in_the_way = node.dir.path().join("in-the-way");
    std::fs::write(&in_the_way, b"a file").unwrap();
    let text = node.text("", "").replace(
        &node.cache_dir().display().to_string(),
        &in_the_way.display().to_string(),
    );
    let blocked = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let why = serving(&blocked, upstream.clone(), disk.clone(), nothing)
        .await
        .unwrap_err();
    assert!(why.to_string().contains("opening the local cache"), "{why}");
    assert_eq!(exit_status(&why), 1);

    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let text = node.text("", "").replace("127.0.0.1:0", &address);
    let busy = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let why = serving(&busy, upstream.clone(), disk.clone(), nothing)
        .await
        .unwrap_err();
    assert!(
        why.to_string().starts_with(&format!("binding {address}")),
        "{why}"
    );
    assert_eq!(exit_status(&why), 1);

    let missing = node.dir.path().join("no-such-credentials");
    let s3 = format!(
        "kind = \"s3\"\nbucket = \"b\"\nendpoint = \"http://127.0.0.1:1\"\ncredentials_file = \"{}\"",
        missing.display()
    );
    let text = node.text("", "").replace("kind = \"memory\"", &s3);
    let remote = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let why = run_cache(&remote, std::future::ready(()), |_| {})
        .await
        .unwrap_err();
    assert!(why.to_string().contains("credentials"), "{why}");
    assert_eq!(exit_status(&why), 1);

    let flawed = Arc::new(FakeDisk::new());
    flawed.set(Quirk::OneLink);
    let disk: Arc<dyn Disk> = flawed;
    let why = serving(&settings, upstream, disk, nothing)
        .await
        .unwrap_err();
    assert!(
        why.to_string().contains("failed the st_nlink check"),
        "{why}"
    );
    assert_eq!(exit_status(&why), 3);
}

/// Asserts that a budget above the size of the file system less its free-space floor is refused
/// at start with exit status 2 and a message that names the key, and that a budget exactly that
/// size starts. The sizes come from an injected file system (a budget of 1 MiB, a floor of 10
/// percent: 1165084 bytes leave exactly the budget, 1165083 one byte less).
#[tokio::test]
async fn a_budget_larger_than_the_file_system_less_the_floor_is_refused_with_status_2() {
    let node = Node::new("tscache-cli-room");
    let settings = node.settings("", "");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let nothing = |_addr: SocketAddr| async {};

    let small = Arc::new(FakeDisk::new());
    small.set_space(1_165_083, 1_165_083);
    let disk: Arc<dyn Disk> = small;
    let why = serving(&settings, upstream.clone(), disk, nothing)
        .await
        .unwrap_err();
    assert!(why.to_string().contains("local_cache.budget"), "{why}");
    assert!(
        why.to_string().contains("free_floor of 10 percent"),
        "{why}"
    );
    assert_eq!(exit_status(&why), 2);

    let enough = Arc::new(FakeDisk::new());
    enough.set_space(1_165_084, 1_165_084);
    let disk: Arc<dyn Disk> = enough;
    let drained = serving(&settings, upstream, disk, nothing).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts what the start does with a file system whose size it cannot use: when the system
/// refuses the question the answer is the reason (naming that the size could not be read), when it
/// answers 0 bytes the answer is that reason, and in neither case is the budget refused; the node
/// starts (and says so on stderr). A size that is read is returned as it is. A defect that read
/// "unknown" as 0 would refuse every budget with exit status 2, and one that read it as a
/// size would start a node on a budget it was never compared with.
#[tokio::test]
async fn a_file_system_whose_size_is_unknown_is_not_judged_and_the_node_starts() {
    let path = std::env::temp_dir();
    let failing = FakeDisk::new();
    failing.set(Quirk::SpaceFails);
    let why = size_of_file_system(&failing, &path).unwrap_err();
    assert!(why.contains("could not be read"), "{why}");

    let empty = FakeDisk::new();
    empty.set_space(0, 0);
    let why = size_of_file_system(&empty, &path).unwrap_err();
    assert!(why.contains("reports a size of 0 bytes"), "{why}");
    let line = unjudged_warning(&why, &path);
    assert!(line.starts_with("tierstore: warning: "), "{line}");
    assert!(line.contains(&why), "{line}");
    assert!(line.contains(&path.display().to_string()), "{line}");
    assert!(
        line.ends_with("local_cache.budget is not checked against it"),
        "{line}"
    );

    let sized = FakeDisk::new();
    sized.set_space(5000, 100);
    assert_eq!(size_of_file_system(&sized, &path), Ok(5000));

    let node = Node::new("tscache-cli-room-unknown");
    let settings = node.settings("", "");
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let nothing = |_addr: SocketAddr| async {};
    let refusing = Arc::new(FakeDisk::new());
    refusing.set(Quirk::SpaceFails);
    let disk: Arc<dyn Disk> = refusing;
    let drained = serving(&settings, upstream.clone(), disk, nothing)
        .await
        .unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });

    let zero = Arc::new(FakeDisk::new());
    zero.set_space(0, 0);
    let disk: Arc<dyn Disk> = zero;
    let drained = serving(&settings, upstream, disk, nothing).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}
