//! The start of a build node: what it serves, and the refusals of its start.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{ByteRange, Capabilities, ObjectKey, ObjectStore};
use kbf_testkit::dir::TestDir;
use tokio::sync::oneshot;

use super::Drained;
use super::cache_cli::{run_cache, serve_cache, size_of_file_system, unjudged_warning};
use super::cli::Error;
use super::cli::{checked, exit_status};
use super::config::{PerRun, Settings, settings_from_text};
use super::config_cache::CacheSettings;
use super::local_cache::testing::{FakeDisk, Quirk, digest, pattern};
use super::local_cache::{Disk, SystemDisk};
use crate::selftest_tests::{Flaw, Flawed};
use crate::{
    Backend, ByteRange as TierRange, Error as BackendError, FaultyBackend, Listed, MemoryBackend,
    Meta,
};

const KEY_ID: &str = "AKIDCACHE";
const SECRET: &str = "cache-secret";

/// The knobs of a run whose drain does not wait.
const QUICK: PerRun = PerRun {
    drain_lb_wait: Duration::from_millis(50),
    drain_deadline: Duration::from_secs(60),
};

/// A scratch directory with a key file in it.
pub(super) struct Node {
    dir: TestDir,
    key: PathBuf,
}

impl Node {
    pub(super) fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let key = dir.path().join("key");
        std::fs::write(&key, format!("{KEY_ID}\n{SECRET}\n")).unwrap();
        std::fs::set_permissions(&key, Permissions::from_mode(0o600)).unwrap();
        Self { dir, key }
    }

    pub(super) fn cache_dir(&self) -> PathBuf {
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

    pub(super) fn settings(&self, server: &str, local: &str) -> CacheSettings {
        let text = self.text(server, local);
        settings_from_text(&text, QUICK)
            .unwrap()
            .into_cache()
            .unwrap()
    }
}

/// A client of kbf for the server at `addr`.
pub(super) fn client(addr: SocketAddr) -> S3Store {
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
pub(super) async fn probe(addr: SocketAddr, path: &str) -> (u16, String) {
    let reply = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    (reply.status().as_u16(), reply.text().await.unwrap())
}

/// Runs `serve_cache` over this upstream and disk, calls `work` with the address, then stops it.
pub(super) async fn serving<F, Fut>(
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
            let _ = told.send(bound.s3);
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
        assert_eq!((status, text.as_str()), (200, "ready read-write"));
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        let gauge = "tierstore_cache_mode{mode=\"read_write\"} 1\n";
        assert!(text.contains(gauge), "{text}");
        let other = "tierstore_cache_mode{mode=\"read_only\"} 0\n";
        assert!(text.contains(other), "{text}");
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
            let _ = told.send(bound.s3);
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

/// What a read-only key sees of a store: the reads are answered and every write is refused as a
/// 403 is, with an error that is no precondition's.
struct ReadOnly(MemoryBackend);

fn forbidden() -> BackendError {
    BackendError::Other("403 AccessDenied".to_owned())
}

#[async_trait]
impl Backend for ReadOnly {
    async fn put(&self, _key: &str, _body: Bytes) -> Result<(), BackendError> {
        Err(forbidden())
    }

    async fn put_if_match(
        &self,
        _key: &str,
        _body: Bytes,
        _etag: &str,
    ) -> Result<String, BackendError> {
        Err(forbidden())
    }

    async fn get(&self, key: &str, range: Option<TierRange>) -> Result<Bytes, BackendError> {
        self.0.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, BackendError> {
        self.0.head(key).await
    }

    async fn delete(&self, _key: &str) -> Result<(), BackendError> {
        Err(forbidden())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, BackendError> {
        self.0.list(prefix).await
    }

    async fn create_upload(&self, _key: &str) -> Result<String, BackendError> {
        Err(forbidden())
    }
}

/// Asserts that a start with `write_through` asks the upstream the write questions of the
/// self-test and refuses an upstream that answers one wrongly with exit status 3, before the
/// address is bound: a key that may not write (the create-only check), a store that overwrites on
/// a create-only put (the same check), and one that overwrites on the completion of an upload (the
/// multipart-conditional check, so the start asks the multipart question too). The same read-only
/// upstream starts a node without `write_through`, which never asks.
#[tokio::test]
async fn a_write_through_start_tests_the_upstream_for_writes_and_refuses_a_wrong_answer_with_3() {
    let node = Node::new("tscache-cli-wthru-refused");
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let nothing = |_addr: SocketAddr| async {};
    let writing = node.settings("", "write_through = true");
    let reading = node.settings("", "");

    let read_only: Arc<dyn Backend> = Arc::new(ReadOnly(MemoryBackend::new()));
    let overwriting: Arc<dyn Backend> = Arc::new(Flawed::new(Flaw::OverwritesOnCreate));
    let completing: Arc<dyn Backend> = Arc::new(Flawed::new(Flaw::CompletionOverwrites));
    let flaws = [
        (read_only.clone(), "create-only"),
        (overwriting, "create-only"),
        (completing, "multipart-conditional"),
    ];
    for (upstream, check) in flaws {
        let why = serving(&writing, upstream, disk.clone(), nothing)
            .await
            .unwrap_err();
        let said = format!("failed the {check} check");
        assert!(why.to_string().contains(&said), "{check}: {why}");
        assert_eq!(exit_status(&why), 3, "{check}");
    }

    let drained = serving(&reading, read_only, disk, nothing).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that a node with `write_through` over an upstream that answers the self-test takes a
/// create-only put through its front, which is made at the upstream, and a repeat of it is
/// refused with a precondition error; that the probe keys of the self-test are gone from the
/// upstream when the node has started; and that the same node without `write_through` refuses the
/// put and the upstream gets none.
#[tokio::test]
async fn a_write_through_node_takes_a_put_and_a_read_cache_refuses_it() {
    let node = Node::new("tscache-cli-wthru-serve");
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    for writes in [true, false] {
        let flat = Arc::new(Flawed::new(Flaw::None));
        let upstream: Arc<dyn Backend> = flat.clone();
        let local = if writes { "write_through = true" } else { "" };
        let settings = node.settings("", local);
        let work = |addr: SocketAddr| async move {
            let store = client(addr);
            let key = ObjectKey::new("cas/made").unwrap();
            let made = store.put_new(&key, Bytes::from_static(b"made"), None).await;
            assert_eq!(made.is_ok(), writes, "{made:?}");
            let again = store
                .put_new(&key, Bytes::from_static(b"twice"), None)
                .await;
            assert!(again.is_err());
        };
        let drained = serving(&settings, upstream, disk.clone(), work)
            .await
            .unwrap();
        assert_eq!(drained, Drained { abandoned: 0 });
        let stored = flat.inner.get("cas/made", None).await;
        assert_eq!(stored.ok(), writes.then(|| Bytes::from_static(b"made")));
        assert!(flat.inner.list("_tiering/").await.unwrap().is_empty());
    }
}

/// Asserts what `--check-config` finds in the settings of a build node: a file that is whole
/// passes, a request timeout of zero is refused with the message of a start, and a key file that
/// is not there is refused (the branch for a cache), and that a budget far over the file system
/// is not looked at (the start's check of the disk is not made).
#[test]
fn check_config_of_a_build_node_refuses_a_zero_timeout_and_a_missing_key_file() {
    let node = Node::new("tscache-cli-check-cache");
    let big = node.text("", "").replace("\"1MiB\"", "\"8TiB\"");
    let whole = settings_from_text(&big, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    assert!(checked(&Settings::Cache(Box::new(whole))).is_ok());

    let zero = node.settings("s3_request_timeout = \"0s\"", "");
    let why = checked(&Settings::Cache(Box::new(zero))).unwrap_err();
    assert!(why.to_string().contains("--s3-request-timeout"), "{why}");

    let text = node
        .text("", "")
        .replace(&node.key.display().to_string(), "/no/such/access.key");
    let gone = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let why = checked(&Settings::Cache(Box::new(gone))).unwrap_err();
    assert!(why.to_string().contains("/no/such/access.key"), "{why}");
}

/// Asserts what `--check-config` finds in the settings of a storage node: a file whose key file
/// is there passes, one whose bucket is the name of a probe path is refused, and one whose key
/// file is not there is refused (the branch for a storage node).
#[test]
fn check_config_of_a_storage_node_reads_its_key_files() {
    let node = Node::new("tscache-cli-check-storage");
    let text = format!(
        r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "s1"
bucket          = "tier-bucket"
access_key_file = "{key}"

[hot_store]
kind   = "memory"
budget = 1048576

[cold_store]
kind = "memory"
"#,
        key = node.key.display()
    );
    let storage = |text: &str| settings_from_text(text, QUICK).unwrap();
    assert!(checked(&storage(&text)).is_ok());
    let probe = text.replace("tier-bucket", "healthz");
    let why = checked(&storage(&probe)).unwrap_err();
    assert!(why.to_string().contains("probe path"), "{why}");
    let gone = text.replace(&node.key.display().to_string(), "/no/such/access.key");
    let why = checked(&storage(&gone)).unwrap_err();
    assert!(why.to_string().contains("/no/such/access.key"), "{why}");
}

/// Asserts what `--check-config` finds in the `[oci]` table of a storage node: a plain listener
/// off loopback passes (the warning of a start is not printed, which the binary test checks on
/// stderr); a prefix that overlaps `_journal/` and a certificate file that is not there are
/// refused, each with the reason; and a certificate without its key does not load at all.
#[test]
fn check_config_of_a_storage_node_checks_its_oci_table() {
    let node = Node::new("tscache-cli-check-oci");
    let with = |oci: &str| {
        format!(
            r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "s1"
bucket          = "tier-bucket"
access_key_file = "{key}"

[hot_store]
kind   = "memory"
budget = 1048576

[cold_store]
kind = "memory"

[oci]
{oci}
"#,
            key = node.key.display()
        )
    };
    let load = |oci: &str| settings_from_text(&with(oci), QUICK);
    let plain = load("listen = \"0.0.0.0:0\"").unwrap();
    assert!(checked(&plain).is_ok());

    let overlapping = load("listen = \"127.0.0.1:0\"\nprefix = \"_journal/x/\"").unwrap();
    let why = checked(&overlapping).unwrap_err();
    assert!(why.to_string().contains("overlaps"), "{why}");
    assert_eq!(exit_status(&why), 2);

    let key = node.key.display();
    let no_cert = format!(
        "listen = \"127.0.0.1:0\"\ntls_cert_file = \"/no/such/cert.pem\"\ntls_key_file = \"{key}\""
    );
    let why = checked(&load(&no_cert).unwrap()).unwrap_err();
    assert!(why.to_string().contains("/no/such/cert.pem"), "{why}");

    let lone = format!("listen = \"127.0.0.1:0\"\ntls_cert_file = \"{key}\"");
    let why = load(&lone).unwrap_err();
    assert!(why.to_string().contains("tls_key_file"), "{why}");
}

/// Asserts what `--check-config` finds in the `[oci]` table of a build node, as for a storage
/// node: a plain listener off loopback passes without the warning of a start; a certificate file
/// that is not there is refused with the reason and status 2; a certificate without its key does
/// not load; and a prefix outside every keeping policy does not load, naming `oci.prefix`.
#[test]
fn check_config_of_a_build_node_checks_its_oci_table() {
    let node = Node::new("tscache-cli-check-oci-cache");
    let load = |oci: &str| {
        let text = format!("{}\n[oci]\n{oci}\n", node.text("", ""));
        settings_from_text(&text, QUICK)
    };
    let plain = load("listen = \"0.0.0.0:0\"\nprefix = \"cas/\"").unwrap();
    assert!(checked(&plain).is_ok());

    let key = node.key.display();
    let no_cert = format!(
        "listen = \"127.0.0.1:0\"\nprefix = \"cas/\"\ntls_cert_file = \"/no/such/cert.pem\"\ntls_key_file = \"{key}\""
    );
    let why = checked(&load(&no_cert).unwrap()).unwrap_err();
    assert!(why.to_string().contains("/no/such/cert.pem"), "{why}");
    // A start exits 1 for a file it cannot read (a restart may mend that); `--check-config` exits
    // 2 for any refusal, which the binary test checks.
    assert_eq!(exit_status(&why), 1);

    let lone = format!("listen = \"127.0.0.1:0\"\nprefix = \"cas/\"\ntls_cert_file = \"{key}\"");
    let why = load(&lone).unwrap_err();
    assert!(why.to_string().contains("tls_key_file"), "{why}");

    let unkept = load("listen = \"127.0.0.1:0\"\nprefix = \"_journal/x/\"").unwrap_err();
    assert!(unkept.to_string().contains("oci.prefix"), "{unkept}");
}

/// Asserts that a build node whose `[oci]` certificate is not a certificate is refused at start
/// with exit status 2, before the cache directory is made, as a storage node's front is.
#[tokio::test]
async fn a_build_node_with_a_bad_oci_certificate_is_refused_before_the_cache_opens() {
    let node = Node::new("tscache-cli-oci-cert");
    let key = node.key.display();
    let text = format!(
        "{}\n[oci]\nlisten = \"127.0.0.1:0\"\nprefix = \"cas/\"\ntls_cert_file = \"{key}\"\ntls_key_file = \"{key}\"\n",
        node.text("", "")
    );
    let settings = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let nothing = |_addr: SocketAddr| async {};
    let why = serving(&settings, upstream, disk, nothing)
        .await
        .unwrap_err();
    assert_eq!(exit_status(&why), 2, "{why}");
    assert!(!node.cache_dir().exists());
}

/// Asserts that an OCI address that is taken stops the start of a build node with an error that
/// a restart may mend (exit status 1), and that the S3 listener, which was bound first, is
/// closed again: its address can be bound by someone else as soon as the start has returned.
#[tokio::test]
async fn a_taken_oci_address_stops_the_start_and_closes_the_s3_listener() {
    let node = Node::new("tscache-cli-oci-taken");
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let s3 = free.local_addr().unwrap();
    drop(free);
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap();
    let text = node.text("", "").replace("127.0.0.1:0", &s3.to_string());
    let text = format!("{text}\n[oci]\nlisten = \"{busy}\"\nprefix = \"cas/\"\n");
    let settings = settings_from_text(&text, QUICK)
        .unwrap()
        .into_cache()
        .unwrap();
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let nothing = |_addr: SocketAddr| async {};
    let why = serving(&settings, upstream, disk, nothing)
        .await
        .unwrap_err();
    assert!(why.to_string().contains(&busy.to_string()), "{why}");
    assert_eq!(exit_status(&why), 1, "{why}");
    let again = std::net::TcpListener::bind(s3);
    assert!(again.is_ok(), "the S3 address is still held");
}
